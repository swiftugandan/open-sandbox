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
pub struct ExecOutput {
    pub exit_code: i32,
    #[serde(with = "bytes_as_string")]
    pub stdout: Vec<u8>,
    #[serde(with = "bytes_as_string")]
    pub stderr: Vec<u8>,
}

mod bytes_as_string {
    use serde::Serializer;

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&String::from_utf8_lossy(bytes))
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ExecRequest {
    pub command: Vec<String>,
}

pub struct WriteFilesRequest {
    pub content: Vec<u8>,
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
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

    fn delete(
        &self,
        sandbox_id: &SandboxId,
    ) -> impl Future<Output = Result<(), ApiError>> + Send;

    fn exec(
        &self,
        sandbox_id: &SandboxId,
        request: ExecRequest,
    ) -> impl Future<Output = Result<ExecOutput, ApiError>> + Send;

    fn write_files(
        &self,
        sandbox_id: &SandboxId,
        request: WriteFilesRequest,
    ) -> impl Future<Output = Result<(), ApiError>> + Send;

    fn read_file(
        &self,
        sandbox_id: &SandboxId,
        request: ReadFileRequest,
    ) -> impl Future<Output = Result<Vec<u8>, ApiError>> + Send;
}
