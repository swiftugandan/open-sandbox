use std::time::Duration;

use open_sandbox_agent::container::{ContainerConfig, ContainerId, ContainerInfo, ContainerRuntime};
use open_sandbox_contracts::error::AgentError;

pub struct DockerRuntime {
    _client: bollard::Docker,
}

impl DockerRuntime {
    pub fn connect() -> Result<Self, AgentError> {
        let client = bollard::Docker::connect_with_local_defaults().map_err(|e| AgentError::Docker {
            detail: e.to_string(),
        })?;
        Ok(Self { _client: client })
    }
}

impl ContainerRuntime for DockerRuntime {
    async fn create_and_start(
        &self,
        _config: ContainerConfig,
    ) -> Result<ContainerInfo, AgentError> {
        Err(AgentError::Docker {
            detail: "Docker runtime not yet implemented".to_string(),
        })
    }

    async fn stop_and_remove(
        &self,
        _id: &ContainerId,
        _timeout: Duration,
    ) -> Result<(), AgentError> {
        Err(AgentError::Docker {
            detail: "Docker runtime not yet implemented".to_string(),
        })
    }

    async fn list_sandbox_containers(&self) -> Result<Vec<ContainerInfo>, AgentError> {
        Err(AgentError::Docker {
            detail: "Docker runtime not yet implemented".to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use open_sandbox_contracts::types::SandboxId;
    use std::collections::HashMap;

    fn test_config(sandbox_id: SandboxId) -> ContainerConfig {
        ContainerConfig {
            sandbox_id,
            image: "nginx:alpine".to_string(),
            cpu_limit_millicores: 1000,
            memory_limit_bytes: 512 * 1024 * 1024,
            env_vars: HashMap::new(),
            exposed_port: 80,
        }
    }

    #[tokio::test]
    async fn create_and_start_returns_running_container() {
        let runtime = DockerRuntime::connect().unwrap();
        let sandbox_id = SandboxId::new();
        let config = test_config(sandbox_id.clone());

        let info = runtime.create_and_start(config).await.unwrap();

        assert_eq!(info.sandbox_id, sandbox_id);
        assert!(info.running);
        assert!(info.host_port > 0);
        assert!(!info.id.0.is_empty());

        // cleanup
        let _ = runtime
            .stop_and_remove(&info.id, Duration::from_secs(5))
            .await;
    }

    #[tokio::test]
    async fn stop_and_remove_cleans_up_container() {
        let runtime = DockerRuntime::connect().unwrap();
        let sandbox_id = SandboxId::new();
        let config = test_config(sandbox_id);

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
    async fn list_sandbox_containers_finds_labeled_containers() {
        let runtime = DockerRuntime::connect().unwrap();
        let sandbox_id = SandboxId::new();
        let config = test_config(sandbox_id.clone());

        let info = runtime.create_and_start(config).await.unwrap();

        let containers = runtime.list_sandbox_containers().await.unwrap();
        assert!(containers.iter().any(|c| c.sandbox_id == sandbox_id));

        // cleanup
        let _ = runtime
            .stop_and_remove(&info.id, Duration::from_secs(5))
            .await;
    }

    #[tokio::test]
    async fn create_applies_resource_limits() {
        let runtime = DockerRuntime::connect().unwrap();
        let sandbox_id = SandboxId::new();
        let config = ContainerConfig {
            sandbox_id,
            image: "nginx:alpine".to_string(),
            cpu_limit_millicores: 500,
            memory_limit_bytes: 256 * 1024 * 1024,
            env_vars: HashMap::from([("TEST_VAR".into(), "test_value".into())]),
            exposed_port: 80,
        };

        let info = runtime.create_and_start(config).await.unwrap();

        // Verify via bollard inspect that limits were applied
        let client = bollard::Docker::connect_with_local_defaults().unwrap();
        let inspect = client
            .inspect_container(&info.id.0, None)
            .await
            .unwrap();

        let host_config = inspect.host_config.unwrap();
        assert_eq!(host_config.memory, Some(256 * 1024 * 1024));
        assert_eq!(host_config.nano_cpus, Some(500_000_000));

        // cleanup
        let _ = runtime
            .stop_and_remove(&info.id, Duration::from_secs(5))
            .await;
    }
}
