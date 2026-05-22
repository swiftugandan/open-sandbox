use open_sandbox_contracts::api::sandbox_management_service_client::SandboxManagementServiceClient;
use open_sandbox_contracts::api::{
    CreateSandboxRequest as ProtoCreate, DeleteSandboxRequest as ProtoDelete,
    ExecSandboxRequest as ProtoExec, GetSandboxRequest as ProtoGet,
};
use open_sandbox_contracts::error::ApiError;
use open_sandbox_contracts::types::SandboxId;
use tonic::transport::Channel;

use crate::service::{
    CreateRequest, ExecOutput, ExecRequest, ReadFileRequest, SandboxInfo, SandboxService,
    WriteFilesRequest, WriteFilesResult,
};

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
                stdin: vec![],
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

    async fn write_files(
        &self,
        sandbox_id: &SandboxId,
        request: WriteFilesRequest,
    ) -> Result<WriteFilesResult, ApiError> {
        let cwd = request
            .cwd
            .as_deref()
            .unwrap_or(open_sandbox_contracts::constants::DEFAULT_WRITE_CWD);
        let exec_req = ExecRequest {
            command: vec![
                "sh".into(),
                "-c".into(),
                "mkdir -p \"$1\" && tar xzf - -C \"$1\"".into(),
                "--".into(),
                cwd.into(),
            ],
        };
        let mut client = self.client.clone();
        let resp = client
            .exec_sandbox(ProtoExec {
                sandbox_id: sandbox_id.to_string(),
                command: exec_req.command,
                stdin: request.content,
            })
            .await
            .map_err(grpc_to_api)?
            .into_inner();

        if resp.exit_code != 0 {
            return Err(ApiError::ExecFailed {
                detail: String::from_utf8_lossy(&resp.stderr).into_owned(),
            });
        }
        Ok(WriteFilesResult { success: true })
    }

    async fn read_file(
        &self,
        sandbox_id: &SandboxId,
        request: ReadFileRequest,
    ) -> Result<Vec<u8>, ApiError> {
        let path = match &request.cwd {
            Some(cwd) => format!(
                "{}/{}",
                cwd.trim_end_matches('/'),
                request.path.trim_start_matches('/')
            ),
            None => request.path.clone(),
        };
        let mut client = self.client.clone();
        let resp = client
            .exec_sandbox(ProtoExec {
                sandbox_id: sandbox_id.to_string(),
                command: vec!["cat".into(), "--".into(), path.clone()],
                stdin: vec![],
            })
            .await
            .map_err(grpc_to_api)?
            .into_inner();

        if resp.exit_code != 0 {
            let stderr = String::from_utf8_lossy(&resp.stderr);
            if stderr.contains("No such file") {
                return Err(ApiError::FileNotFound { path });
            }
            return Err(ApiError::ExecFailed {
                detail: stderr.into_owned(),
            });
        }
        Ok(resp.stdout)
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
