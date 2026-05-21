use std::collections::HashMap;
use std::time::Duration;

use bollard::container::{
    Config, CreateContainerOptions, ListContainersOptions, RemoveContainerOptions,
    StopContainerOptions,
};
use bollard::models::HostConfig;

use open_sandbox_agent::container::{ContainerConfig, ContainerId, ContainerInfo, ContainerRuntime};
use open_sandbox_contracts::error::AgentError;
use open_sandbox_contracts::types::SandboxId;

const LABEL_MANAGED_BY: &str = "open-sandbox.managed-by";
const LABEL_MANAGED_BY_VALUE: &str = "open-sandbox-agent";
const LABEL_SANDBOX_ID: &str = "open-sandbox.sandbox-id";
const NANOCPUS_PER_MILLICPU: i64 = 1_000_000;

pub struct DockerRuntime {
    client: bollard::Docker,
}

impl DockerRuntime {
    pub fn connect() -> Result<Self, AgentError> {
        let client = bollard::Docker::connect_with_local_defaults().map_err(docker_err)?;
        Ok(Self { client })
    }
}

impl ContainerRuntime for DockerRuntime {
    async fn create_and_start(
        &self,
        config: ContainerConfig,
    ) -> Result<ContainerInfo, AgentError> {
        let sandbox_id_str = config.sandbox_id.to_string();
        let container_port = format!("{}/tcp", config.exposed_port);
        let name = format!("sandbox-{sandbox_id_str}");

        let docker_config = build_docker_config(&config, &sandbox_id_str, &container_port);
        let options = CreateContainerOptions {
            name: name.as_str(),
            platform: None,
        };

        let created = self
            .client
            .create_container(Some(options), docker_config)
            .await
            .map_err(docker_err)?;

        self.client
            .start_container::<String>(&created.id, None)
            .await
            .map_err(docker_err)?;

        let inspect = self
            .client
            .inspect_container(&created.id, None)
            .await
            .map_err(docker_err)?;

        let host_port = extract_host_port(&inspect, &container_port)?;

        Ok(ContainerInfo {
            id: ContainerId(created.id),
            sandbox_id: config.sandbox_id,
            host_port,
            running: true,
        })
    }

    async fn stop_and_remove(
        &self,
        id: &ContainerId,
        timeout: Duration,
    ) -> Result<(), AgentError> {
        let stop_opts = StopContainerOptions {
            t: timeout.as_secs() as i64,
        };
        // Stop may fail if already stopped
        let _ = self.client.stop_container(&id.0, Some(stop_opts)).await;

        let remove_opts = RemoveContainerOptions {
            force: true,
            v: true,
            ..Default::default()
        };
        self.client
            .remove_container(&id.0, Some(remove_opts))
            .await
            .map_err(docker_err)?;

        Ok(())
    }

    async fn list_sandbox_containers(&self) -> Result<Vec<ContainerInfo>, AgentError> {
        let filter = format!("{LABEL_MANAGED_BY}={LABEL_MANAGED_BY_VALUE}");
        let options = ListContainersOptions {
            all: true,
            filters: HashMap::from([("label".to_string(), vec![filter])]),
            ..Default::default()
        };

        let containers = self
            .client
            .list_containers(Some(options))
            .await
            .map_err(docker_err)?;

        let mut result = Vec::with_capacity(containers.len());
        for container in containers {
            let Some(ref id) = container.id else {
                continue;
            };

            let labels = container.labels.unwrap_or_default();
            let Some(sandbox_id_str) = labels.get(LABEL_SANDBOX_ID) else {
                continue;
            };

            let sandbox_id = uuid::Uuid::parse_str(sandbox_id_str)
                .map(SandboxId::from)
                .map_err(|e| AgentError::Internal {
                    detail: e.to_string(),
                })?;

            let running = container
                .state
                .as_deref()
                .is_some_and(|s| s == "running");

            let host_port = container
                .ports
                .unwrap_or_default()
                .iter()
                .find_map(|p| p.public_port)
                .unwrap_or(0);

            result.push(ContainerInfo {
                id: ContainerId(id.clone()),
                sandbox_id,
                host_port,
                running,
            });
        }

        Ok(result)
    }
}

fn build_docker_config(
    config: &ContainerConfig,
    sandbox_id_str: &str,
    container_port: &str,
) -> Config<String> {
    let nano_cpus = (config.cpu_limit_millicores as i64) * NANOCPUS_PER_MILLICPU;

    let host_config = HostConfig {
        memory: Some(config.memory_limit_bytes as i64),
        nano_cpus: Some(nano_cpus),
        publish_all_ports: Some(true),
        ..Default::default()
    };

    let mut env_vec: Vec<String> = config
        .env_vars
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    env_vec.sort();

    let labels = HashMap::from([
        (LABEL_MANAGED_BY.to_string(), LABEL_MANAGED_BY_VALUE.to_string()),
        (LABEL_SANDBOX_ID.to_string(), sandbox_id_str.to_string()),
    ]);

    Config::<String> {
        image: Some(config.image.clone()),
        labels: Some(labels),
        exposed_ports: Some(HashMap::from([(container_port.to_string(), HashMap::new())])),
        host_config: Some(host_config),
        env: Some(env_vec),
        ..Default::default()
    }
}

fn extract_host_port(
    inspect: &bollard::models::ContainerInspectResponse,
    container_port: &str,
) -> Result<u16, AgentError> {
    let ports = inspect
        .network_settings
        .as_ref()
        .and_then(|ns| ns.ports.as_ref())
        .ok_or_else(|| AgentError::Docker {
            detail: "no port mappings found".into(),
        })?;

    let bindings = ports
        .get(container_port)
        .and_then(|b| b.as_ref())
        .ok_or_else(|| AgentError::Docker {
            detail: format!("no binding for {container_port}"),
        })?;

    bindings
        .first()
        .and_then(|b| b.host_port.as_ref())
        .and_then(|p| p.parse::<u16>().ok())
        .ok_or_else(|| AgentError::Docker {
            detail: "could not parse host port".into(),
        })
}

fn docker_err(e: bollard::errors::Error) -> AgentError {
    AgentError::Docker {
        detail: e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

        let client = bollard::Docker::connect_with_local_defaults().unwrap();
        let inspect = client.inspect_container(&info.id.0, None).await.unwrap();

        let host_config = inspect.host_config.unwrap();
        assert_eq!(host_config.memory, Some(256 * 1024 * 1024));
        assert_eq!(host_config.nano_cpus, Some(500_000_000));

        let _ = runtime
            .stop_and_remove(&info.id, Duration::from_secs(5))
            .await;
    }
}
