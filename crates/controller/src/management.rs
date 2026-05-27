use std::sync::Arc;

use tonic::{
    Request, Response, Status,
    service::interceptor::InterceptedService,
};

use open_sandbox_contracts::api::{
    CreateSandboxRequest, CreateSandboxResponse, DeleteSandboxRequest, DeleteSandboxResponse,
    GetSandboxRequest, GetSandboxResponse, ListSandboxesRequest, ListSandboxesResponse,
    PauseSandboxRequest, PauseSandboxResponse, UnpauseSandboxRequest, UnpauseSandboxResponse,
    sandbox_management_service_server::{SandboxManagementService, SandboxManagementServiceServer},
};
use open_sandbox_contracts::controller::{
    ControllerCommand, PauseSandbox, StopSandbox, UnpauseSandbox, controller_command,
};
use open_sandbox_contracts::types::SandboxId;

use crate::auth::{AdminAuthInterceptor, LIST_SANDBOXES_MAX};
use crate::error_status::controller_error_to_status;
use crate::grpc::{Controller, CreateSandboxRequest as InternalCreateRequest};
use crate::scheduler::SandboxRequirements;
use crate::store::ControllerStore;

pub struct ManagementHandler<S: ControllerStore> {
    controller: Arc<Controller<S>>,
}

impl<S: ControllerStore + 'static> ManagementHandler<S> {
    pub fn new(controller: Arc<Controller<S>>) -> Self {
        Self { controller }
    }
}

#[tonic::async_trait]
impl<S: ControllerStore + 'static> SandboxManagementService for ManagementHandler<S> {
    async fn create_sandbox(
        &self,
        request: Request<CreateSandboxRequest>,
    ) -> Result<Response<CreateSandboxResponse>, Status> {
        let req = request.into_inner();
        let sandbox_id = SandboxId::new();

        let cpu = if req.cpu_millicores == 0 {
            open_sandbox_contracts::constants::DEFAULT_SANDBOX_CPU_MILLICORES
        } else {
            req.cpu_millicores
        };
        let mem = if req.memory_bytes == 0 {
            open_sandbox_contracts::constants::DEFAULT_SANDBOX_MEMORY_BYTES
        } else {
            req.memory_bytes
        };

        let exposed_port = if req.exposed_port == 0 {
            open_sandbox_contracts::constants::DEFAULT_SANDBOX_EXPOSED_PORT
        } else {
            req.exposed_port
        };

        // v1.0.2 (iter10): fail-closed at the wire boundary. A newer
        // client carrying a stricter PullPolicy variant we don't know
        // about must NOT be silently downgraded to IfNotPresent —
        // that would defeat the air-gap guarantee for callers who set
        // Never. The agent (downstream of this validation) continues
        // to use the lossy `From<i32>` for defense-in-depth.
        let pull_policy = open_sandbox_contracts::types::PullPolicy::from_wire_i32_strict(
            req.pull_policy,
        )
        .map_err(|e| Status::invalid_argument(e.to_string()))?;

        let assignment = self
            .controller
            .create_sandbox(InternalCreateRequest {
                sandbox_id: sandbox_id.clone(),
                image: req.image,
                requirements: SandboxRequirements {
                    cpu_millicores: cpu,
                    memory_bytes: mem,
                },
                env_vars: req.env_vars,
                exposed_port,
                pull_policy,
            })
            .await
            .map_err(|e| controller_error_to_status(&e))?;

        let _ = self
            .controller
            .save_sandbox_state(&sandbox_id, &assignment.agent_id, "creating", None)
            .await;

        Ok(Response::new(CreateSandboxResponse {
            sandbox_id: sandbox_id.to_string(),
            subdomain: sandbox_id.subdomain(),
            agent_id: assignment.agent_id.to_string(),
            status: "creating".into(),
        }))
    }

    async fn get_sandbox(
        &self,
        request: Request<GetSandboxRequest>,
    ) -> Result<Response<GetSandboxResponse>, Status> {
        let req = request.into_inner();
        let sandbox_id = parse_id(&req.sandbox_id)?;

        let entry = self
            .controller
            .find_routing_entry(&sandbox_id)
            .await
            .map_err(|e| controller_error_to_status(&e))?;

        match entry {
            Some(entry) => {
                let row = self
                    .controller
                    .get_sandbox_state(&sandbox_id)
                    .await
                    .map_err(|e| controller_error_to_status(&e))?;
                let (status, error) = match row {
                    Some(r) => (r.state, r.error.unwrap_or_default()),
                    None => ("running".to_string(), String::new()),
                };
                Ok(Response::new(GetSandboxResponse {
                    sandbox_id: entry.sandbox_id.to_string(),
                    agent_id: entry.agent_id.to_string(),
                    subdomain: entry.sandbox_id.subdomain(),
                    status,
                    error,
                }))
            }
            None => Err(Status::not_found(req.sandbox_id.clone())),
        }
    }

    async fn delete_sandbox(
        &self,
        request: Request<DeleteSandboxRequest>,
    ) -> Result<Response<DeleteSandboxResponse>, Status> {
        let req = request.into_inner();
        let sandbox_id = parse_id(&req.sandbox_id)?;

        let entry = self
            .controller
            .find_routing_entry(&sandbox_id)
            .await
            .map_err(|e| controller_error_to_status(&e))?;

        let entry = entry.ok_or_else(|| Status::not_found(req.sandbox_id.clone()))?;

        let command = ControllerCommand {
            payload: Some(controller_command::Payload::StopSandbox(StopSandbox {
                sandbox_id: sandbox_id.to_string(),
                timeout_seconds: open_sandbox_contracts::constants::SANDBOX_STOP_TIMEOUT.as_secs()
                    as u32,
            })),
        };
        self.controller
            .connections
            .send_command(&entry.agent_id, command)
            .await
            .map_err(|e| controller_error_to_status(&e))?;

        // F5: release the reservation alongside the routing entry so the
        // agent's available capacity is credited back atomically.
        self.controller
            .release_sandbox(&sandbox_id)
            .await
            .map_err(|e| controller_error_to_status(&e))?;

        Ok(Response::new(DeleteSandboxResponse { deleted: true }))
    }

    async fn pause_sandbox(
        &self,
        request: Request<PauseSandboxRequest>,
    ) -> Result<Response<PauseSandboxResponse>, Status> {
        let req = request.into_inner();
        let sandbox_id = parse_id(&req.sandbox_id)?;

        let entry = self
            .controller
            .find_routing_entry(&sandbox_id)
            .await
            .map_err(|e| controller_error_to_status(&e))?
            .ok_or_else(|| Status::not_found(req.sandbox_id.clone()))?;

        // Cascade-fix #5: precondition check. Pause only makes sense
        // for a sandbox the agent has reported as Running (or already
        // Pausing/Paused — idempotent). Refusing here returns HTTP 409
        // so the user sees a clear error rather than a 202 followed by
        // an agent runtime error → Failed cascade.
        match self
            .controller
            .get_sandbox_state(&sandbox_id)
            .await
            .map_err(|e| controller_error_to_status(&e))?
        {
            Some(row) => {
                let s = row.state.as_str();
                if !matches!(s, "running" | "pausing" | "paused") {
                    return Err(status_with_invalid_state(format!(
                        "cannot pause sandbox in state '{s}'"
                    )));
                }
            }
            // No row yet — sandbox was just created and the agent hasn't
            // emitted SandboxStatus. Allow the dispatch; the agent's
            // pause_sandbox path will return SandboxNotFound if the
            // container isn't ready yet, which maps to NotFound HTTP.
            None => {}
        }

        let command = ControllerCommand {
            payload: Some(controller_command::Payload::PauseSandbox(PauseSandbox {
                sandbox_id: sandbox_id.to_string(),
            })),
        };
        self.controller
            .connections
            .send_command(&entry.agent_id, command)
            .await
            .map_err(|e| controller_error_to_status(&e))?;
        // Persist the optimistic transition state so GetSandbox /
        // ListSandboxes reflect "pausing" the moment the controller
        // dispatched (rather than the previous steady-state "running"
        // until the agent ACKs). Mirrors create_sandbox's "creating"
        // write. The agent's SandboxStatus(Paused) overwrites this to
        // "paused" once the runtime call completes. Best-effort — if
        // the write fails the agent ACK still arrives later and writes
        // the correct steady-state.
        let _ = self
            .controller
            .save_sandbox_state(&sandbox_id, &entry.agent_id, "pausing", None)
            .await;
        Ok(Response::new(PauseSandboxResponse {
            status: "pausing".into(),
        }))
    }

    async fn unpause_sandbox(
        &self,
        request: Request<UnpauseSandboxRequest>,
    ) -> Result<Response<UnpauseSandboxResponse>, Status> {
        let req = request.into_inner();
        let sandbox_id = parse_id(&req.sandbox_id)?;

        let entry = self
            .controller
            .find_routing_entry(&sandbox_id)
            .await
            .map_err(|e| controller_error_to_status(&e))?
            .ok_or_else(|| Status::not_found(req.sandbox_id.clone()))?;

        // Cascade-fix #5: unpause precondition. The DB row must show
        // the sandbox is paused (or transitioning).
        match self
            .controller
            .get_sandbox_state(&sandbox_id)
            .await
            .map_err(|e| controller_error_to_status(&e))?
        {
            Some(row) => {
                let s = row.state.as_str();
                if !matches!(s, "paused" | "unpausing" | "running") {
                    return Err(status_with_invalid_state(format!(
                        "cannot unpause sandbox in state '{s}'"
                    )));
                }
            }
            None => {}
        }

        let command = ControllerCommand {
            payload: Some(controller_command::Payload::UnpauseSandbox(UnpauseSandbox {
                sandbox_id: sandbox_id.to_string(),
            })),
        };
        self.controller
            .connections
            .send_command(&entry.agent_id, command)
            .await
            .map_err(|e| controller_error_to_status(&e))?;
        let _ = self
            .controller
            .save_sandbox_state(&sandbox_id, &entry.agent_id, "unpausing", None)
            .await;
        Ok(Response::new(UnpauseSandboxResponse {
            status: "unpausing".into(),
        }))
    }

    async fn list_sandboxes(
        &self,
        _request: Request<ListSandboxesRequest>,
    ) -> Result<Response<ListSandboxesResponse>, Status> {
        let mut entries = self
            .controller
            .list_routing_entries()
            .await
            .map_err(|e| controller_error_to_status(&e))?;

        // F1: server-side cap. ListSandboxesRequest has no max_results in
        // contracts/v1.0.1; proper pagination is deferred to a contract bump.
        if entries.len() > LIST_SANDBOXES_MAX {
            tracing::warn!(
                total = entries.len(),
                cap = LIST_SANDBOXES_MAX,
                "ListSandboxes result truncated"
            );
            entries.truncate(LIST_SANDBOXES_MAX);
        }

        let mut sandboxes = Vec::with_capacity(entries.len());
        for entry in entries {
            let row = self
                .controller
                .get_sandbox_state(&entry.sandbox_id)
                .await
                .map_err(|e| controller_error_to_status(&e))?;
            let (status, error) = match row {
                Some(r) => (r.state, r.error.unwrap_or_default()),
                None => ("running".to_string(), String::new()),
            };
            sandboxes.push(GetSandboxResponse {
                sandbox_id: entry.sandbox_id.to_string(),
                agent_id: entry.agent_id.to_string(),
                subdomain: entry.sandbox_id.subdomain(),
                status,
                error,
            });
        }
        Ok(Response::new(ListSandboxesResponse { sandboxes }))
    }
}

// tonic handlers require Status as the error type
#[allow(clippy::result_large_err)]
fn parse_id(id: &str) -> Result<SandboxId, Status> {
    uuid::Uuid::parse_str(id)
        .map(SandboxId::from)
        .map_err(|_| Status::invalid_argument("invalid sandbox_id"))
}

/// Build a FailedPrecondition Status carrying the
/// `x-os-error-code: INVALID_STATE` trailer so the api gateway routes
/// it to ApiError::InvalidState → HTTP 409 (rather than the legacy
/// tonic::Code::FailedPrecondition fallback). Cascade-fix #5.
fn status_with_invalid_state(msg: impl Into<String>) -> Status {
    let mut status = Status::failed_precondition(msg.into());
    if let Ok(v) =
        tonic::metadata::MetadataValue::try_from("INVALID_STATE")
    {
        status
            .metadata_mut()
            .insert(open_sandbox_contracts::constants::ERROR_CODE_HEADER, v);
    }
    status
}

/// Wrap the management service with the required admin-token interceptor.
/// Every RPC requires `authorization: Bearer <CONTROLLER_ADMIN_TOKEN>`.
/// See REVIEW_LOG.md F1.
pub fn management_service<S: ControllerStore + 'static>(
    controller: Arc<Controller<S>>,
    auth: AdminAuthInterceptor,
) -> InterceptedService<SandboxManagementServiceServer<ManagementHandler<S>>, AdminAuthInterceptor>
{
    InterceptedService::new(
        SandboxManagementServiceServer::new(ManagementHandler::new(controller)),
        auth,
    )
}
