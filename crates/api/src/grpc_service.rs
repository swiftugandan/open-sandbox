use open_sandbox_contracts::api::sandbox_management_service_client::SandboxManagementServiceClient;
use open_sandbox_contracts::api::{
    CreateSandboxRequest as ProtoCreate, DeleteSandboxRequest as ProtoDelete,
    ExecSandboxRequest as ProtoExec, GetSandboxRequest as ProtoGet,
};
use open_sandbox_contracts::error::ApiError;
use open_sandbox_contracts::types::SandboxId;
use tonic::transport::Channel;

use crate::service::{CreateRequest, ExecOutput, ExecRequest, SandboxInfo, SandboxService};

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
            status: "running".into(),
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
        })
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

    async fn exec(
        &self,
        sandbox_id: &SandboxId,
        request: ExecRequest,
    ) -> Result<ExecOutput, ApiError> {
        let mut client = self.client.clone();
        let resp = client
            .exec_sandbox(ProtoExec {
                sandbox_id: sandbox_id.to_string(),
                command: request.command,
            })
            .await
            .map_err(grpc_to_api)?
            .into_inner();

        Ok(ExecOutput {
            exit_code: resp.exit_code,
            stdout: resp.stdout,
            stderr: resp.stderr,
        })
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
        tonic::Code::DeadlineExceeded => ApiError::ExecFailed {
            detail: status.message().to_string(),
        },
        _ => ApiError::Internal {
            detail: status.message().to_string(),
        },
    }
}

fn parse_sandbox_id(id: &str) -> Result<SandboxId, ApiError> {
    uuid::Uuid::parse_str(id)
        .map(SandboxId::from)
        .map_err(|_| ApiError::Internal {
            detail: format!("controller returned invalid sandbox_id: {id}"),
        })
}
