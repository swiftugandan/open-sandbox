use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use open_sandbox_contracts::controller::{SandboxState, StartSandbox, StopSandbox};
use open_sandbox_contracts::error::AgentError;
use open_sandbox_contracts::types::SandboxId;

use crate::container::{ContainerId, ContainerRuntime};

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

    pub async fn start_sandbox(
        &self,
        cmd: StartSandbox,
    ) -> Result<SandboxState, AgentError> {
        let _ = cmd;
        todo!()
    }

    pub async fn stop_sandbox(
        &self,
        cmd: StopSandbox,
    ) -> Result<SandboxState, AgentError> {
        let _ = cmd;
        todo!()
    }

    pub fn get_sandbox(&self, sandbox_id: &SandboxId) -> Option<SandboxEntry> {
        self.sandboxes.lock().unwrap().get(sandbox_id).cloned()
    }

    pub fn list_sandboxes(&self) -> Vec<SandboxEntry> {
        self.sandboxes.lock().unwrap().values().cloned().collect()
    }

    pub fn host_port_for(&self, sandbox_id: &SandboxId) -> Result<u16, AgentError> {
        let _ = sandbox_id;
        todo!()
    }

    pub async fn reconcile(&self) -> Result<Vec<SandboxEntry>, AgentError> {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;

    fn start_cmd(sandbox_id: &SandboxId, image: &str) -> StartSandbox {
        use open_sandbox_contracts::controller::SandboxConfig;
        StartSandbox {
            sandbox_id: sandbox_id.to_string(),
            image: image.into(),
            config: Some(SandboxConfig {
                cpu_limit_millicores: 1000,
                memory_limit_bytes: 512_000_000,
                env_vars: HashMap::new(),
                exposed_port: 8080,
            }),
        }
    }

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

        assert_eq!(state, SandboxState::Running.into());
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
        assert_eq!(entry.state, SandboxState::Running.into());
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

        assert_eq!(state, SandboxState::Failed.into());
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

        let state = manager.stop_sandbox(stop_cmd(&sandbox_id, 10)).await.unwrap();

        assert_eq!(state, SandboxState::Stopped.into());
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

        manager.stop_sandbox(stop_cmd(&sandbox_id, 10)).await.unwrap();

        assert!(manager.get_sandbox(&sandbox_id).is_none());
    }

    #[tokio::test]
    async fn stop_unknown_sandbox_returns_error() {
        let runtime = Arc::new(MockContainerRuntime::new());
        let manager = SandboxManager::new(runtime);

        let result = manager
            .stop_sandbox(stop_cmd(&SandboxId::new(), 10))
            .await;

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
