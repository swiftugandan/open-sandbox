//! gRPC client adapter — lifecycle calls to the controller.
//!
//! v1.0 removed exec from this path. Streaming I/O (exec, file
//! read, file write, file write_files) goes through the proxy via
//! `ProxyClient::open_io_stream`.

use open_sandbox_contracts::api::sandbox_management_service_client::SandboxManagementServiceClient;
use open_sandbox_contracts::api::{
    CreateSandboxRequest as ProtoCreate, DeleteSandboxRequest as ProtoDelete,
    GetSandboxRequest as ProtoGet, ListSandboxesRequest as ProtoList,
};
use open_sandbox_contracts::error::ApiError;
use open_sandbox_contracts::types::SandboxId;
use tonic::transport::Channel;

use crate::service::{CreateRequest, SandboxInfo, SandboxService};

pub struct GrpcSandboxService {
    client: SandboxManagementServiceClient<Channel>,
    /// Comp-1 F1 wiring: the controller's management gRPC requires
    /// `authorization: Bearer <CONTROLLER_ADMIN_TOKEN>` on every call.
    /// The api gateway must read the same token from env and forward
    /// it on each request.
    admin_token: Option<String>,
}

impl GrpcSandboxService {
    pub fn new(channel: Channel) -> Self {
        Self {
            client: SandboxManagementServiceClient::new(channel),
            admin_token: None,
        }
    }

    pub async fn connect(controller_url: &str) -> Result<Self, ApiError> {
        let channel = Channel::from_shared(controller_url.to_string())
            .map_err(|e| ApiError::ControllerUnavailable {
                detail: e.to_string(),
            })?
            .connect()
            .await
            .map_err(|e| ApiError::ControllerUnavailable {
                detail: e.to_string(),
            })?;
        let admin_token = std::env::var("CONTROLLER_ADMIN_TOKEN").ok();
        Ok(Self {
            client: SandboxManagementServiceClient::new(channel),
            admin_token,
        })
    }

    /// Wrap a proto message in a `tonic::Request` with the
    /// `authorization: Bearer <admin_token>` header attached when the
    /// token is configured.
    fn authed<M>(&self, msg: M) -> tonic::Request<M> {
        let mut req = tonic::Request::new(msg);
        if let Some(token) = &self.admin_token {
            if let Ok(v) = tonic::metadata::MetadataValue::try_from(format!("Bearer {token}")) {
                req.metadata_mut().insert("authorization", v);
            }
        }
        req
    }
}

impl SandboxService for GrpcSandboxService {
    async fn create(&self, request: CreateRequest) -> Result<SandboxInfo, ApiError> {
        let mut client = self.client.clone();
        let resp = client
            .create_sandbox(self.authed(ProtoCreate {
                image: request.image,
                cpu_millicores: request.cpu_millicores,
                memory_bytes: request.memory_bytes,
                env_vars: request.env_vars,
                exposed_port: request.exposed_port,
                pull_policy: request.pull_policy.to_wire() as i32,
            }))
            .await
            .map_err(grpc_to_api)?
            .into_inner();

        Ok(SandboxInfo {
            sandbox_id: parse_sandbox_id(&resp.sandbox_id)?,
            subdomain: resp.subdomain,
            agent_id: resp.agent_id,
            status: resp.status,
            // CreateSandboxResponse doesn't carry an error field —
            // failures show up later via the per-sandbox GET.
            error: None,
        })
    }

    async fn get(&self, sandbox_id: &SandboxId) -> Result<SandboxInfo, ApiError> {
        let mut client = self.client.clone();
        let resp = client
            .get_sandbox(self.authed(ProtoGet {
                sandbox_id: sandbox_id.to_string(),
            }))
            .await
            .map_err(grpc_to_api)?
            .into_inner();

        Ok(SandboxInfo {
            sandbox_id: parse_sandbox_id(&resp.sandbox_id)?,
            subdomain: resp.subdomain,
            agent_id: resp.agent_id,
            status: resp.status,
            error: empty_to_none(resp.error),
        })
    }

    async fn list(&self) -> Result<Vec<SandboxInfo>, ApiError> {
        let mut client = self.client.clone();
        let resp = client
            .list_sandboxes(self.authed(ProtoList {}))
            .await
            .map_err(grpc_to_api)?
            .into_inner();
        let mut out = Vec::with_capacity(resp.sandboxes.len());
        for item in resp.sandboxes {
            out.push(SandboxInfo {
                sandbox_id: parse_sandbox_id(&item.sandbox_id)?,
                subdomain: item.subdomain,
                agent_id: item.agent_id,
                status: item.status,
                error: empty_to_none(item.error),
            });
        }
        Ok(out)
    }

    async fn delete(&self, sandbox_id: &SandboxId) -> Result<(), ApiError> {
        let mut client = self.client.clone();
        client
            .delete_sandbox(self.authed(ProtoDelete {
                sandbox_id: sandbox_id.to_string(),
            }))
            .await
            .map_err(grpc_to_api)?;
        Ok(())
    }
}

fn grpc_to_api(status: tonic::Status) -> ApiError {
    // v1.0.2 cascade: prefer the x-os-error-code trailer over the
    // tonic::Code-based fallback. The trailer carries the structured
    // ControllerError variant name (set in controller_error_to_status),
    // so a Code::NotFound that means "agent_id not registered" no longer
    // collapses to ApiError::SandboxNotFound. Falls back to the legacy
    // mapping for any Status that doesn't carry the trailer (other gRPC
    // peers, tonic-internal errors, pre-v1.0.2 emitters).
    if let Some(code) = status
        .metadata()
        .get(open_sandbox_contracts::constants::ERROR_CODE_HEADER)
        .and_then(|v| v.to_str().ok())
    {
        return match code {
            "INVALID_TOKEN" => ApiError::Unauthorized {
                detail: status.message().to_string(),
            },
            "SANDBOX_NOT_FOUND" => ApiError::SandboxNotFound {
                sandbox_id: status.message().to_string(),
            },
            "AGENT_NOT_FOUND" => ApiError::ControllerUnavailable {
                detail: status.message().to_string(),
            },
            "NO_AVAILABLE_AGENTS" => ApiError::ControllerUnavailable {
                detail: format!("no available agents: {}", status.message()),
            },
            "DATABASE_ERROR" | "INTERNAL" | "UNKNOWN" => ApiError::Internal {
                detail: status.message().to_string(),
            },
            _ => ApiError::Internal {
                detail: format!("{code}: {}", status.message()),
            },
        };
    }
    match status.code() {
        tonic::Code::NotFound => ApiError::SandboxNotFound {
            sandbox_id: status.message().to_string(),
        },
        tonic::Code::Unavailable => ApiError::ControllerUnavailable {
            detail: status.message().to_string(),
        },
        // v1.0.2 (iter10): a Status::invalid_argument from the
        // controller (e.g. UnknownPullPolicy at the wire boundary)
        // is operator-actionable client-side input — surface it as
        // HTTP 400 instead of collapsing to 500 INTERNAL_ERROR.
        // The fail-closed validation is pointless if its rationale
        // gets buried under a generic 5xx at the gateway.
        tonic::Code::InvalidArgument => ApiError::InvalidRequest {
            detail: status.message().to_string(),
        },
        _ => ApiError::Internal {
            detail: status.message().to_string(),
        },
    }
}

fn empty_to_none(s: String) -> Option<String> {
    if s.is_empty() { None } else { Some(s) }
}

fn parse_sandbox_id(id: &str) -> Result<SandboxId, ApiError> {
    uuid::Uuid::parse_str(id)
        .map(SandboxId::from)
        .map_err(|_| ApiError::Internal {
            detail: format!("controller returned invalid sandbox_id: {id}"),
        })
}

#[cfg(test)]
mod grpc_to_api_tests {
    use super::*;

    /// v1.0.2 (iter10): exercises the gateway's mapping of
    /// `Status::invalid_argument` from the controller (e.g.
    /// `UnknownPullPolicy` rejected at the wire boundary) into a
    /// 4xx-class `ApiError`. Without this arm the same Status
    /// silently became `ApiError::Internal` → HTTP 500, defeating
    /// iter10's fail-closed design end-to-end.
    #[test]
    fn invalid_argument_maps_to_invalid_request() {
        let status = tonic::Status::invalid_argument(
            "unknown PullPolicy wire value 4 (expected 0..=3); refusing to silently downgrade",
        );
        let api_err = grpc_to_api(status);
        match api_err {
            ApiError::InvalidRequest { detail } => {
                assert!(
                    detail.contains("PullPolicy") && detail.contains("4"),
                    "operator-actionable detail must survive the gateway boundary: {detail}"
                );
            }
            other => panic!("expected InvalidRequest, got {other:?}"),
        }
    }

    /// Trailer-based mapping still wins over the Code-based fallback.
    /// Without this guard a structured ControllerError that happens
    /// to use Code::InvalidArgument under the hood could end up in
    /// the wrong arm.
    #[test]
    fn x_os_error_code_trailer_overrides_code() {
        use open_sandbox_contracts::constants::ERROR_CODE_HEADER;
        let mut status = tonic::Status::invalid_argument("ignored");
        status
            .metadata_mut()
            .insert(ERROR_CODE_HEADER, "SANDBOX_NOT_FOUND".parse().unwrap());
        match grpc_to_api(status) {
            ApiError::SandboxNotFound { .. } => {}
            other => panic!("expected SandboxNotFound from trailer, got {other:?}"),
        }
    }
}
