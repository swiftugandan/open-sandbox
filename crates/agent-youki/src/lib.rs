pub mod cni;
pub mod exec;
pub mod image;
pub mod spec;

use std::path::PathBuf;
use std::time::Duration;

use open_sandbox_agent::container::{
    ContainerConfig, ContainerId, ContainerInfo, ContainerRuntime, ExecOutput,
};
use open_sandbox_contracts::error::AgentError;

pub struct YoukiConfig {
    pub root_dir: PathBuf,
    pub cni_bin_path: PathBuf,
}

impl Default for YoukiConfig {
    fn default() -> Self {
        Self {
            root_dir: PathBuf::from("/run/open-sandbox"),
            cni_bin_path: PathBuf::from("/opt/cni/bin"),
        }
    }
}

pub struct YoukiRuntime {
    _config: YoukiConfig,
}

impl YoukiRuntime {
    pub fn new(_config: YoukiConfig) -> Result<Self, AgentError> {
        todo!()
    }
}

impl ContainerRuntime for YoukiRuntime {
    async fn create_and_start(
        &self,
        _config: ContainerConfig,
    ) -> Result<ContainerInfo, AgentError> {
        todo!()
    }

    async fn stop_and_remove(
        &self,
        _id: &ContainerId,
        _timeout: Duration,
    ) -> Result<(), AgentError> {
        todo!()
    }

    async fn list_sandbox_containers(&self) -> Result<Vec<ContainerInfo>, AgentError> {
        todo!()
    }

    async fn exec(
        &self,
        _id: &ContainerId,
        _command: Vec<String>,
        _stdin: Vec<u8>,
    ) -> Result<ExecOutput, AgentError> {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use open_sandbox_contracts::types::SandboxId;

    use super::*;

    fn test_config() -> YoukiConfig {
        YoukiConfig {
            root_dir: PathBuf::from("/tmp/youki-test"),
            cni_bin_path: PathBuf::from("/opt/cni/bin"),
        }
    }

    fn sandbox_config(sandbox_id: SandboxId) -> ContainerConfig {
        ContainerConfig {
            sandbox_id,
            image: "alpine:latest".to_string(),
            cpu_limit_millicores: 1000,
            memory_limit_bytes: 512 * 1024 * 1024,
            env_vars: HashMap::new(),
            exposed_port: 8080,
        }
    }

    #[tokio::test]
    async fn create_and_start_returns_running_container() {
        let runtime = YoukiRuntime::new(test_config()).unwrap();
        let sandbox_id = SandboxId::new();
        let config = sandbox_config(sandbox_id.clone());

        let info = runtime.create_and_start(config).await.unwrap();

        assert_eq!(info.sandbox_id, sandbox_id);
        assert!(info.running);
        assert!(info.host_port > 0);
        assert!(!info.id.0.is_empty());

        let _ = runtime
            .stop_and_remove(&info.id, Duration::from_secs(5))
            .await;
    }

    #[tokio::test]
    async fn stop_and_remove_cleans_up() {
        let runtime = YoukiRuntime::new(test_config()).unwrap();
        let sandbox_id = SandboxId::new();
        let config = sandbox_config(sandbox_id);

        let info = runtime.create_and_start(config).await.unwrap();
        let container_id = info.id.clone();

        runtime
            .stop_and_remove(&container_id, Duration::from_secs(5))
            .await
            .unwrap();

        let containers = runtime.list_sandbox_containers().await.unwrap();
        assert!(!containers.iter().any(|c| c.id == container_id));
    }

    #[tokio::test]
    async fn list_finds_managed_containers() {
        let runtime = YoukiRuntime::new(test_config()).unwrap();
        let sandbox_id = SandboxId::new();
        let config = sandbox_config(sandbox_id.clone());

        let info = runtime.create_and_start(config).await.unwrap();

        let containers = runtime.list_sandbox_containers().await.unwrap();
        assert!(containers.iter().any(|c| c.sandbox_id == sandbox_id));

        let _ = runtime
            .stop_and_remove(&info.id, Duration::from_secs(5))
            .await;
    }

    #[tokio::test]
    async fn exec_runs_command() {
        let runtime = YoukiRuntime::new(test_config()).unwrap();
        let sandbox_id = SandboxId::new();
        let config = sandbox_config(sandbox_id);

        let info = runtime.create_and_start(config).await.unwrap();

        let output = runtime
            .exec(&info.id, vec!["echo".into(), "hello".into()], vec![])
            .await
            .unwrap();

        assert_eq!(output.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "hello");

        let _ = runtime
            .stop_and_remove(&info.id, Duration::from_secs(5))
            .await;
    }
}
