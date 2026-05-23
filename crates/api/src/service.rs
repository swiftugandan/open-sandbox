//! Lifecycle-only service trait. Exec and streaming I/O go through
//! `ProxyClient::open_io_stream`; file ops are unary HTTP handlers
//! that internally open OpenIoStream with the appropriate op variant.

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

#[derive(Debug, Clone, serde::Serialize)]
pub struct WriteFilesResult {
    pub success: bool,
}

/// Single-file write JSON body. Exactly one of `content` / `content_b64`
/// MUST be set.
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
pub struct ReadFileQuery {
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
}
