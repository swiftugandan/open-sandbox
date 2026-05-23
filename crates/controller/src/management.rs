use std::sync::Arc;

use tonic::{
    Request, Response, Status,
    service::interceptor::InterceptedService,
};

use open_sandbox_contracts::api::{
    CreateSandboxRequest, CreateSandboxResponse, DeleteSandboxRequest, DeleteSandboxResponse,
    GetSandboxRequest, GetSandboxResponse, ListSandboxesRequest, ListSandboxesResponse,
    sandbox_management_service_server::{SandboxManagementService, SandboxManagementServiceServer},
};
use open_sandbox_contracts::controller::{ControllerCommand, StopSandbox, controller_command};
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
