use open_sandbox_contracts::error::ApiError;
use open_sandbox_contracts::types::SandboxId;

#[derive(Debug, Clone, serde::Serialize)]
pub struct SandboxInfo {
    pub sandbox_id: SandboxId,
    pub subdomain: String,
    pub agent_id: String,
    pub status: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateRequest {
    pub image: String,
    #[serde(default = "default_cpu")]
    pub cpu_millicores: u32,
    #[serde(default = "default_memory")]
    pub memory_bytes: u64,
    #[serde(default)]
    pub env_vars: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub exposed_port: u32,
}

fn default_cpu() -> u32 {
    open_sandbox_contracts::constants::DEFAULT_SANDBOX_CPU_MILLICORES
}

fn default_memory() -> u64 {
    open_sandbox_contracts::constants::DEFAULT_SANDBOX_MEMORY_BYTES
}

/// Result of an exec, in the runtime's native byte representation.
///
/// The HTTP handler converts this into [`ExecResponseBody`] for wire format
/// (base64-encoding stdout/stderr per NFR-API-1). Keeping the internal trait
/// type as raw bytes lets non-HTTP consumers (gRPC, tests) work with bytes
/// directly.
#[derive(Debug, Clone)]
pub struct ExecOutput {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub command_not_found: bool,
}

/// JSON wire representation of an exec response. Stdout/stderr are
/// base64-encoded (RFC 4648) — never lossy UTF-8 — so binary tooling output
/// is preserved faithfully.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ExecResponseBody {
    pub exit_code: i32,
    pub stdout_b64: String,
    pub stderr_b64: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<&'static str>,
}

impl From<ExecOutput> for ExecResponseBody {
    fn from(out: ExecOutput) -> Self {
        use base64::Engine;
        let engine = base64::engine::general_purpose::STANDARD;
        let error_code = if out.command_not_found {
            Some("COMMAND_NOT_FOUND")
        } else {
            None
        };
        Self {
            exit_code: out.exit_code,
            stdout_b64: engine.encode(&out.stdout),
            stderr_b64: engine.encode(&out.stderr),
            error_code,
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecRequest {
    pub command: Vec<String>,
    /// UTF-8 stdin string. Mutually exclusive with `stdin_b64`.
    #[serde(default)]
    pub stdin: Option<String>,
    /// Base64-encoded stdin bytes. Mutually exclusive with `stdin`.
    #[serde(default)]
    pub stdin_b64: Option<String>,
    /// Working directory inside the container. None means runtime default.
    #[serde(default)]
    pub cwd: Option<String>,
}

impl ExecRequest {
    /// Decode stdin into bytes. Returns `Ok(empty)` when neither field is set,
    /// `Err(...)` when both are set or `stdin_b64` is malformed.
    pub fn stdin_bytes(&self) -> Result<Vec<u8>, ApiError> {
        match (&self.stdin, &self.stdin_b64) {
            (Some(_), Some(_)) => Err(ApiError::InvalidRequest {
                detail: "stdin and stdin_b64 are mutually exclusive".into(),
            }),
            (Some(s), None) => Ok(s.clone().into_bytes()),
            (None, Some(b)) => {
                use base64::Engine;
                base64::engine::general_purpose::STANDARD
                    .decode(b)
                    .map_err(|e| ApiError::InvalidRequest {
                        detail: format!("stdin_b64 is not valid base64: {e}"),
                    })
            }
            (None, None) => Ok(Vec::new()),
        }
    }
}

pub struct WriteFilesRequest {
    pub content: Vec<u8>,
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct WriteFilesResult {
    pub success: bool,
}

/// Single-file write request. Exactly one of `content` / `content_b64` MUST
/// be present; both-or-neither is `INVALID_REQUEST`.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriteFileRequest {
    pub path: String,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub content_b64: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
}

impl WriteFileRequest {
    pub fn content_bytes(&self) -> Result<Vec<u8>, ApiError> {
        match (&self.content, &self.content_b64) {
            (Some(_), Some(_)) | (None, None) => Err(ApiError::InvalidRequest {
                detail: "exactly one of content or content_b64 must be set".into(),
            }),
            (Some(s), None) => Ok(s.clone().into_bytes()),
            (None, Some(b)) => {
                use base64::Engine;
                base64::engine::general_purpose::STANDARD
                    .decode(b)
                    .map_err(|e| ApiError::InvalidRequest {
                        detail: format!("content_b64 is not valid base64: {e}"),
                    })
            }
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadFileRequest {
    pub path: String,
    #[serde(default)]
    pub cwd: Option<String>,
}

pub trait SandboxService: Send + Sync + 'static {
    fn create(
        &self,
        request: CreateRequest,
    ) -> impl Future<Output = Result<SandboxInfo, ApiError>> + Send;

    fn get(
        &self,
        sandbox_id: &SandboxId,
    ) -> impl Future<Output = Result<SandboxInfo, ApiError>> + Send;

    fn list(&self) -> impl Future<Output = Result<Vec<SandboxInfo>, ApiError>> + Send;

    fn delete(&self, sandbox_id: &SandboxId) -> impl Future<Output = Result<(), ApiError>> + Send;

    fn exec(
        &self,
        sandbox_id: &SandboxId,
        request: ExecRequest,
    ) -> impl Future<Output = Result<ExecOutput, ApiError>> + Send;

    fn write_files(
        &self,
        sandbox_id: &SandboxId,
        request: WriteFilesRequest,
    ) -> impl Future<Output = Result<WriteFilesResult, ApiError>> + Send;

    fn write_file(
        &self,
        sandbox_id: &SandboxId,
        request: WriteFileRequest,
    ) -> impl Future<Output = Result<WriteFilesResult, ApiError>> + Send;

    fn read_file(
        &self,
        sandbox_id: &SandboxId,
        request: ReadFileRequest,
    ) -> impl Future<Output = Result<Vec<u8>, ApiError>> + Send;
}
