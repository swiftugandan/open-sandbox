use open_sandbox_contracts::api::sandbox_management_service_client::SandboxManagementServiceClient;
use open_sandbox_contracts::api::{
    CreateSandboxRequest as ProtoCreate, DeleteSandboxRequest as ProtoDelete,
    ExecSandboxRequest as ProtoExec, GetSandboxRequest as ProtoGet,
    ListSandboxesRequest as ProtoList,
};
use open_sandbox_contracts::error::ApiError;
use open_sandbox_contracts::types::SandboxId;
use tonic::transport::Channel;

use crate::service::{
    CreateRequest, ExecOutput, ExecRequest, ReadFileRequest, SandboxInfo, SandboxService,
    WriteFileRequest, WriteFilesRequest, WriteFilesResult,
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

    async fn exec(
        &self,
        sandbox_id: &SandboxId,
        request: ExecRequest,
    ) -> Result<ExecOutput, ApiError> {
        let stdin = request.stdin_bytes()?;
        let cwd = request.cwd.unwrap_or_default();
        let mut client = self.client.clone();
        let resp = client
            .exec_sandbox(ProtoExec {
                sandbox_id: sandbox_id.to_string(),
                command: request.command,
                stdin,
                cwd,
            })
            .await
            .map_err(grpc_to_api)?
            .into_inner();

        Ok(ExecOutput {
            exit_code: resp.exit_code,
            stdout: resp.stdout,
            stderr: resp.stderr,
            command_not_found: resp.command_not_found,
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
        let mut client = self.client.clone();
        let resp = client
            .exec_sandbox(ProtoExec {
                sandbox_id: sandbox_id.to_string(),
                command: vec![
                    "sh".into(),
                    "-c".into(),
                    "mkdir -p \"$1\" && tar xzf - -C \"$1\"".into(),
                    "--".into(),
                    cwd.into(),
                ],
                stdin: request.content,
                cwd: String::new(),
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

    async fn write_file(
        &self,
        sandbox_id: &SandboxId,
        request: WriteFileRequest,
    ) -> Result<WriteFilesResult, ApiError> {
        let content = request.content_bytes()?;
        let cwd = request
            .cwd
            .as_deref()
            .unwrap_or(open_sandbox_contracts::constants::DEFAULT_WRITE_CWD);
        // Atomic write: stream content to a temp file in the target's
        // directory, then rename to the final path. Both temp and final
        // must live on the same filesystem for rename to be atomic, so
        // the temp is placed next to the target — not in /tmp.
        // The shell runs the whole sequence; failure at any step exits
        // non-zero and we surface stderr.
        let script = "set -e
            target=\"$1\"
            case \"$target\" in
                /*) abs=\"$target\" ;;
                *)  abs=\"$2/$target\" ;;
            esac
            dir=$(dirname \"$abs\")
            mkdir -p \"$dir\"
            tmp=$(mktemp \"$dir/.opensb.XXXXXX\")
            trap 'rm -f \"$tmp\"' EXIT
            cat > \"$tmp\"
            mv \"$tmp\" \"$abs\"
            trap - EXIT";
        let mut client = self.client.clone();
        let resp = client
            .exec_sandbox(ProtoExec {
                sandbox_id: sandbox_id.to_string(),
                command: vec![
                    "sh".into(),
                    "-c".into(),
                    script.into(),
                    "--".into(),
                    request.path,
                    cwd.into(),
                ],
                stdin: content,
                cwd: String::new(),
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
        let resolved = resolve_path(&request.path, request.cwd.as_deref());
        let mut client = self.client.clone();
        let resp = client
            .exec_sandbox(ProtoExec {
                sandbox_id: sandbox_id.to_string(),
                command: vec!["cat".into(), "--".into(), resolved.clone()],
                stdin: vec![],
                cwd: String::new(),
            })
            .await
            .map_err(grpc_to_api)?
            .into_inner();

        if resp.exit_code != 0 {
            let stderr = String::from_utf8_lossy(&resp.stderr);
            if stderr.contains("No such file") {
                return Err(ApiError::FileNotFound {
                    resolved_path: resolved,
                });
            }
            return Err(ApiError::ExecFailed {
                detail: stderr.into_owned(),
            });
        }
        Ok(resp.stdout)
    }
}

fn resolve_path(path: &str, cwd: Option<&str>) -> String {
    if path.starts_with('/') {
        return path.to_string();
    }
    let base = cwd.unwrap_or(open_sandbox_contracts::constants::DEFAULT_WRITE_CWD);
    format!("{}/{}", base.trim_end_matches('/'), path)
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
