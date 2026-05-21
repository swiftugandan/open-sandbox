use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use open_sandbox_contracts::controller::{SandboxState, StartSandbox, StopSandbox};
use open_sandbox_contracts::error::AgentError;
use open_sandbox_contracts::types::SandboxId;

use crate::container::{ContainerConfig, ContainerId, ContainerRuntime, ExecOutput};

fn parse_sandbox_id(raw: &str) -> Result<SandboxId, AgentError> {
    uuid::Uuid::parse_str(raw)
        .map(SandboxId::from)
        .map_err(|e| AgentError::Internal {
            detail: e.to_string(),
        })
}

#[derive(Debug, Clone)]
pub struct SandboxEntry {
    pub sandbox_id: SandboxId,
    pub container_id: ContainerId,
    pub host_port: u16,
    pub state: SandboxState,
}

pub struct SandboxManager<R: ContainerRuntime> {
    runtime: Arc<R>,
    sandboxes: Mutex<HashMap<SandboxId, SandboxEntry>>,
}

impl<R: ContainerRuntime> SandboxManager<R> {
    pub fn new(runtime: Arc<R>) -> Self {
        Self {
            runtime,
            sandboxes: Mutex::new(HashMap::new()),
        }
    }

    pub async fn start_sandbox(&self, cmd: StartSandbox) -> Result<SandboxState, AgentError> {
        let sandbox_id = parse_sandbox_id(&cmd.sandbox_id)?;

        let config = cmd.config.unwrap_or_default();
        let container_config = ContainerConfig {
            sandbox_id: sandbox_id.clone(),
            image: cmd.image,
            cpu_limit_millicores: config.cpu_limit_millicores,
            memory_limit_bytes: config.memory_limit_bytes,
            env_vars: config.env_vars,
            exposed_port: config.exposed_port,
        };

        match self.runtime.create_and_start(container_config).await {
            Ok(info) => {
                let entry = SandboxEntry {
                    sandbox_id: sandbox_id.clone(),
                    container_id: info.id,
                    host_port: info.host_port,
                    state: SandboxState::Running,
                };
                self.sandboxes.lock().unwrap().insert(sandbox_id, entry);
                Ok(SandboxState::Running)
            }
            Err(_) => {
                let entry = SandboxEntry {
                    sandbox_id: sandbox_id.clone(),
                    container_id: ContainerId(String::new()),
                    host_port: 0,
                    state: SandboxState::Failed,
                };
                self.sandboxes.lock().unwrap().insert(sandbox_id, entry);
                Ok(SandboxState::Failed)
            }
        }
    }

    pub async fn stop_sandbox(&self, cmd: StopSandbox) -> Result<SandboxState, AgentError> {
        let sandbox_id = parse_sandbox_id(&cmd.sandbox_id)?;

        let entry = self
            .sandboxes
            .lock()
            .unwrap()
            .get(&sandbox_id)
            .cloned()
            .ok_or_else(|| AgentError::SandboxNotFound {
                sandbox_id: sandbox_id.to_string(),
            })?;

        let timeout = std::time::Duration::from_secs(cmd.timeout_seconds as u64);
        self.runtime
            .stop_and_remove(&entry.container_id, timeout)
            .await?;
        self.sandboxes.lock().unwrap().remove(&sandbox_id);
        Ok(SandboxState::Stopped)
    }

    pub fn get_sandbox(&self, sandbox_id: &SandboxId) -> Option<SandboxEntry> {
        self.sandboxes.lock().unwrap().get(sandbox_id).cloned()
    }

    pub fn list_sandboxes(&self) -> Vec<SandboxEntry> {
        self.sandboxes.lock().unwrap().values().cloned().collect()
    }

    pub fn host_port_for(&self, sandbox_id: &SandboxId) -> Result<u16, AgentError> {
        self.sandboxes
            .lock()
            .unwrap()
            .get(sandbox_id)
            .map(|e| e.host_port)
            .ok_or_else(|| AgentError::SandboxNotFound {
                sandbox_id: sandbox_id.to_string(),
            })
    }

    pub async fn exec_sandbox(
        &self,
        sandbox_id: &SandboxId,
        command: Vec<String>,
        stdin: Vec<u8>,
    ) -> Result<ExecOutput, AgentError> {
        let entry = self
            .sandboxes
            .lock()
            .unwrap()
            .get(sandbox_id)
            .cloned()
            .ok_or_else(|| AgentError::SandboxNotFound {
                sandbox_id: sandbox_id.to_string(),
            })?;

        self.runtime.exec(&entry.container_id, command, stdin).await
    }

    pub async fn reconcile(&self) -> Result<Vec<SandboxEntry>, AgentError> {
        let containers = self.runtime.list_sandbox_containers().await?;
        let mut entries = Vec::with_capacity(containers.len());
        let mut sandboxes = self.sandboxes.lock().unwrap();

        for info in containers {
            let state = if info.running {
                SandboxState::Running
            } else {
                SandboxState::Stopped
            };
            let entry = SandboxEntry {
                sandbox_id: info.sandbox_id.clone(),
                container_id: info.id,
                host_port: info.host_port,
                state,
            };
            sandboxes.insert(info.sandbox_id, entry.clone());
            entries.push(entry);
        }

        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;

    fn stop_cmd(sandbox_id: &SandboxId, timeout_secs: u32) -> StopSandbox {
        StopSandbox {
            sandbox_id: sandbox_id.to_string(),
            timeout_seconds: timeout_secs,
        }
    }

    #[tokio::test]
    async fn start_sandbox_creates_and_starts_container() {
        let runtime = Arc::new(MockContainerRuntime::new());
        let manager = SandboxManager::new(runtime.clone());
        let sandbox_id = SandboxId::new();

        let state = manager
            .start_sandbox(start_cmd(&sandbox_id, "nginx:latest"))
            .await
            .unwrap();

        assert_eq!(state, SandboxState::Running);
        assert_eq!(runtime.created_count(), 1);
    }

    #[tokio::test]
    async fn start_sandbox_records_mapping() {
        let runtime = Arc::new(MockContainerRuntime::new());
        let manager = SandboxManager::new(runtime);
        let sandbox_id = SandboxId::new();

        manager
            .start_sandbox(start_cmd(&sandbox_id, "nginx:latest"))
            .await
            .unwrap();

        let entry = manager.get_sandbox(&sandbox_id);
        assert!(entry.is_some());
        let entry = entry.unwrap();
        assert_eq!(entry.sandbox_id, sandbox_id);
        assert_eq!(entry.state, SandboxState::Running);
    }

    #[tokio::test]
    async fn start_sandbox_returns_failed_on_docker_error() {
        let runtime = Arc::new(FailingContainerRuntime);
        let manager = SandboxManager::new(runtime);
        let sandbox_id = SandboxId::new();

        let state = manager
            .start_sandbox(start_cmd(&sandbox_id, "nginx:latest"))
            .await
            .unwrap();

        assert_eq!(state, SandboxState::Failed);
    }

    #[tokio::test]
    async fn stop_sandbox_stops_and_removes_container() {
        let runtime = Arc::new(MockContainerRuntime::new());
        let manager = SandboxManager::new(runtime.clone());
        let sandbox_id = SandboxId::new();

        manager
            .start_sandbox(start_cmd(&sandbox_id, "nginx:latest"))
            .await
            .unwrap();

        let state = manager
            .stop_sandbox(stop_cmd(&sandbox_id, 10))
            .await
            .unwrap();

        assert_eq!(state, SandboxState::Stopped);
        assert_eq!(runtime.stopped_count(), 1);
    }

    #[tokio::test]
    async fn stop_sandbox_removes_mapping() {
        let runtime = Arc::new(MockContainerRuntime::new());
        let manager = SandboxManager::new(runtime);
        let sandbox_id = SandboxId::new();

        manager
            .start_sandbox(start_cmd(&sandbox_id, "nginx:latest"))
            .await
            .unwrap();

        manager
            .stop_sandbox(stop_cmd(&sandbox_id, 10))
            .await
            .unwrap();

        assert!(manager.get_sandbox(&sandbox_id).is_none());
    }

    #[tokio::test]
    async fn stop_unknown_sandbox_returns_error() {
        let runtime = Arc::new(MockContainerRuntime::new());
        let manager = SandboxManager::new(runtime);

        let result = manager.stop_sandbox(stop_cmd(&SandboxId::new(), 10)).await;

        assert!(matches!(result, Err(AgentError::SandboxNotFound { .. })));
    }

    #[tokio::test]
    async fn host_port_for_running_sandbox() {
        let runtime = Arc::new(MockContainerRuntime::new());
        let manager = SandboxManager::new(runtime);
        let sandbox_id = SandboxId::new();

        manager
            .start_sandbox(start_cmd(&sandbox_id, "nginx:latest"))
            .await
            .unwrap();

        let port = manager.host_port_for(&sandbox_id).unwrap();
        assert!(port > 0);
    }

    #[tokio::test]
    async fn host_port_for_unknown_sandbox_returns_error() {
        let runtime = Arc::new(MockContainerRuntime::new());
        let manager = SandboxManager::new(runtime);

        let result = manager.host_port_for(&SandboxId::new());
        assert!(matches!(result, Err(AgentError::SandboxNotFound { .. })));
    }

    #[tokio::test]
    async fn list_running_sandboxes() {
        let runtime = Arc::new(MockContainerRuntime::new());
        let manager = SandboxManager::new(runtime);

        manager
            .start_sandbox(start_cmd(&SandboxId::new(), "nginx:latest"))
            .await
            .unwrap();
        manager
            .start_sandbox(start_cmd(&SandboxId::new(), "python:3"))
            .await
            .unwrap();

        assert_eq!(manager.list_sandboxes().len(), 2);
    }

    #[tokio::test]
    async fn exec_sandbox_runs_command() {
        let runtime = Arc::new(MockContainerRuntime::new());
        let manager = SandboxManager::new(runtime);
        let sandbox_id = SandboxId::new();

        manager
            .start_sandbox(start_cmd(&sandbox_id, "nginx:latest"))
            .await
            .unwrap();

        let output = manager
            .exec_sandbox(&sandbox_id, vec!["echo".into(), "hello".into()], vec![])
            .await
            .unwrap();

        assert_eq!(output.exit_code, 0);
        assert_eq!(output.stdout, b"echo hello");
    }

    #[tokio::test]
    async fn exec_sandbox_pipes_stdin() {
        let runtime = Arc::new(MockContainerRuntime::new());
        let manager = SandboxManager::new(runtime);
        let sandbox_id = SandboxId::new();

        manager
            .start_sandbox(start_cmd(&sandbox_id, "nginx:latest"))
            .await
            .unwrap();

        let stdin_data = b"tar-data-here".to_vec();
        let output = manager
            .exec_sandbox(
                &sandbox_id,
                vec!["tar".into(), "xzf".into(), "-".into()],
                stdin_data,
            )
            .await
            .unwrap();

        assert_eq!(output.exit_code, 0);
        assert_eq!(output.stdout, b"received 13 bytes");
    }

    #[tokio::test]
    async fn exec_sandbox_unknown_returns_error() {
        let runtime = Arc::new(MockContainerRuntime::new());
        let manager = SandboxManager::new(runtime);

        let result = manager
            .exec_sandbox(&SandboxId::new(), vec!["echo".into()], vec![])
            .await;

        assert!(matches!(result, Err(AgentError::SandboxNotFound { .. })));
    }

    #[tokio::test]
    async fn reconcile_discovers_existing_containers() {
        let runtime = Arc::new(MockContainerRuntime::with_existing(vec![
            mock_container_info(SandboxId::new(), 8080),
            mock_container_info(SandboxId::new(), 8081),
        ]));
        let manager = SandboxManager::new(runtime);

        let reconciled = manager.reconcile().await.unwrap();

        assert_eq!(reconciled.len(), 2);
        assert_eq!(manager.list_sandboxes().len(), 2);
    }
}
