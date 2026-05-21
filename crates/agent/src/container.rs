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
        command: Vec<String>,
        stdin: Vec<u8>,
    ) -> impl Future<Output = Result<ExecOutput, AgentError>> + Send;
}
