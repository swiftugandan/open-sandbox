use std::sync::Arc;

use tonic::{Request, Response, Status};

use open_sandbox_contracts::api::{
    CreateSandboxRequest, CreateSandboxResponse, DeleteSandboxRequest, DeleteSandboxResponse,
    ExecSandboxRequest, ExecSandboxResponse, GetSandboxRequest, GetSandboxResponse,
    sandbox_management_service_server::{SandboxManagementService, SandboxManagementServiceServer},
};
use open_sandbox_contracts::controller::{
    ControllerCommand, ExecCommand, StopSandbox, controller_command,
};
use open_sandbox_contracts::types::SandboxId;

use crate::exec_broker::ExecBroker;
use crate::grpc::{Controller, CreateSandboxRequest as InternalCreateRequest};
use crate::scheduler::SandboxRequirements;
use crate::store::ControllerStore;

pub struct ManagementHandler<S: ControllerStore> {
    controller: Arc<Controller<S>>,
    exec_broker: Arc<ExecBroker>,
}

impl<S: ControllerStore + 'static> ManagementHandler<S> {
    pub fn new(controller: Arc<Controller<S>>, exec_broker: Arc<ExecBroker>) -> Self {
        Self {
            controller,
            exec_broker,
        }
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
            .map_err(|e| Status::internal(e.to_string()))?;

        let _ = self
            .controller
            .save_sandbox_state(&sandbox_id, &assignment.agent_id, "creating")
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
            .map_err(|e| Status::internal(e.to_string()))?;

        match entry {
            Some(entry) => {
                let status = self
                    .controller
                    .get_sandbox_state(&sandbox_id)
                    .await
                    .map_err(|e| Status::internal(e.to_string()))?
                    .unwrap_or_else(|| "running".into());
                Ok(Response::new(GetSandboxResponse {
                    sandbox_id: entry.sandbox_id.to_string(),
                    agent_id: entry.agent_id.to_string(),
                    subdomain: entry.sandbox_id.subdomain(),
                    status,
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
            .map_err(|e| Status::internal(e.to_string()))?;

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
            .map_err(|e| Status::internal(e.to_string()))?;

        self.controller
            .remove_routing_entry(&sandbox_id)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        Ok(Response::new(DeleteSandboxResponse { deleted: true }))
    }

    async fn exec_sandbox(
        &self,
        request: Request<ExecSandboxRequest>,
    ) -> Result<Response<ExecSandboxResponse>, Status> {
        let req = request.into_inner();
        let sandbox_id = parse_id(&req.sandbox_id)?;

        let entry = self
            .controller
            .find_routing_entry(&sandbox_id)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        let entry = entry.ok_or_else(|| Status::not_found(req.sandbox_id.clone()))?;

        let exec_id = uuid::Uuid::new_v4().to_string();
        let rx = self.exec_broker.register(exec_id.clone());

        let command = ControllerCommand {
            payload: Some(controller_command::Payload::Exec(ExecCommand {
                sandbox_id: sandbox_id.to_string(),
                command: req.command,
                exec_id: exec_id.clone(),
                stdin: req.stdin,
            })),
        };
        self.controller
            .connections
            .send_command(&entry.agent_id, command)
            .await
            .map_err(|e| {
                self.exec_broker.cancel(&exec_id);
                Status::internal(e.to_string())
            })?;

        let result = tokio::time::timeout(open_sandbox_contracts::constants::EXEC_TIMEOUT, rx)
            .await
            .map_err(|_| {
                self.exec_broker.cancel(&exec_id);
                Status::deadline_exceeded("exec timeout")
            })?
            .map_err(|_| Status::internal("exec result channel closed"))?;

        Ok(Response::new(ExecSandboxResponse {
            exit_code: result.exit_code,
            stdout: result.stdout,
            stderr: result.stderr,
        }))
    }
}

// tonic handlers require Status as the error type
#[allow(clippy::result_large_err)]
fn parse_id(id: &str) -> Result<SandboxId, Status> {
    uuid::Uuid::parse_str(id)
        .map(SandboxId::from)
        .map_err(|_| Status::invalid_argument("invalid sandbox_id"))
}

pub fn management_service<S: ControllerStore + 'static>(
    controller: Arc<Controller<S>>,
    exec_broker: Arc<ExecBroker>,
) -> SandboxManagementServiceServer<ManagementHandler<S>> {
    SandboxManagementServiceServer::new(ManagementHandler::new(controller, exec_broker))
}
