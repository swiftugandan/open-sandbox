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
}

impl GrpcSandboxService {
    pub fn new(channel: Channel) -> Self {
        Self {
            client: SandboxManagementServiceClient::new(channel),
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
        Ok(Self::new(channel))
    }
}

impl SandboxService for GrpcSandboxService {
    async fn create(&self, request: CreateRequest) -> Result<SandboxInfo, ApiError> {
        let mut client = self.client.clone();
        let resp = client
            .create_sandbox(ProtoCreate {
                image: request.image,
                cpu_millicores: request.cpu_millicores,
                memory_bytes: request.memory_bytes,
                env_vars: request.env_vars,
                exposed_port: request.exposed_port,
            })
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
            .get_sandbox(ProtoGet {
                sandbox_id: sandbox_id.to_string(),
            })
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
            .list_sandboxes(ProtoList {})
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
            .delete_sandbox(ProtoDelete {
                sandbox_id: sandbox_id.to_string(),
            })
            .await
            .map_err(grpc_to_api)?;
        Ok(())
    }
}

fn grpc_to_api(status: tonic::Status) -> ApiError {
    match status.code() {
        tonic::Code::NotFound => ApiError::SandboxNotFound {
            sandbox_id: status.message().to_string(),
        },
        tonic::Code::Unavailable => ApiError::ControllerUnavailable {
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
