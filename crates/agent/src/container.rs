use std::collections::HashMap;
use std::time::Duration;

use open_sandbox_contracts::error::AgentError;
use open_sandbox_contracts::types::SandboxId;

#[derive(Debug, Clone)]
pub struct ContainerConfig {
    pub sandbox_id: SandboxId,
    pub image: String,
    pub cpu_limit_millicores: u32,
    pub memory_limit_bytes: u64,
    pub env_vars: HashMap<String, String>,
    pub exposed_port: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ContainerId(pub String);

#[derive(Debug, Clone)]
pub struct ContainerInfo {
    pub id: ContainerId,
    pub sandbox_id: SandboxId,
    pub host_port: u16,
    pub running: bool,
}

#[derive(Debug, Clone)]
pub struct ExecOutput {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// Set when the runtime determined the executable was missing. Lets the
    /// agent (and ultimately the API) distinguish "command not found" from a
    /// process that ran and exited 127 of its own accord.
    pub command_not_found: bool,
}

#[derive(Debug, Clone)]
pub struct ExecOptions {
    pub command: Vec<String>,
    pub stdin: Vec<u8>,
    /// Working directory inside the container. Empty string means the
    /// runtime's default (typically `/home` per FR-13).
    pub cwd: String,
}

pub trait ContainerRuntime: Send + Sync {
    fn create_and_start(
        &self,
        config: ContainerConfig,
    ) -> impl Future<Output = Result<ContainerInfo, AgentError>> + Send;

    fn stop_and_remove(
        &self,
        id: &ContainerId,
        timeout: Duration,
    ) -> impl Future<Output = Result<(), AgentError>> + Send;

    fn list_sandbox_containers(
        &self,
    ) -> impl Future<Output = Result<Vec<ContainerInfo>, AgentError>> + Send;

    fn exec(
        &self,
        id: &ContainerId,
        options: ExecOptions,
    ) -> impl Future<Output = Result<ExecOutput, AgentError>> + Send;
}

/// Heuristic shared by all runtimes: scan stderr for the canonical OCI
/// "executable file not found" diagnostic produced by runc/crun/youki when
/// the requested binary cannot be resolved in the container.
///
/// Centralising the pattern here keeps every runtime backend consistent and
/// guards against drift between Docker, youki, and any future runtime.
pub fn detect_command_not_found(stderr: &[u8]) -> bool {
    let s = String::from_utf8_lossy(stderr);
    let lower = s.to_ascii_lowercase();
    lower.contains("executable file not found")
        || lower.contains("no such file or directory")
            && (lower.contains("exec:") || lower.contains("starting container process"))
}
