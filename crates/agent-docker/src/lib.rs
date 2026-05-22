use std::collections::HashMap;
use std::time::Duration;

use bollard::container::{
    Config, CreateContainerOptions, ListContainersOptions, RemoveContainerOptions,
    StopContainerOptions,
};
use bollard::models::HostConfig;

use bollard::exec::{CreateExecOptions, StartExecOptions, StartExecResults};
use futures::{StreamExt, TryStreamExt};

use tracing::info;

use open_sandbox_agent::container::{
    ContainerConfig, ContainerId, ContainerInfo, ContainerRuntime, ExecOptions, ExecOutput,
};
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
        let client = bollard::Docker::connect_with_local_defaults().map_err(runtime_err)?;
        Ok(Self { client })
    }
}

impl ContainerRuntime for DockerRuntime {
    async fn create_and_start(&self, config: ContainerConfig) -> Result<ContainerInfo, AgentError> {
        let sandbox_id_str = config.sandbox_id.to_string();
        let name = format!("sandbox-{sandbox_id_str}");

        let port_hint = if config.exposed_port > 0 {
            Some(format!("{}/tcp", config.exposed_port))
        } else {
            None
        };

        info!(image = %config.image, "pulling image");
        self.client
            .create_image(
                Some(bollard::image::CreateImageOptions {
                    from_image: config.image.clone(),
                    ..Default::default()
                }),
                None,
                None,
            )
            .try_collect::<Vec<_>>()
            .await
            .map_err(runtime_err)?;
        info!(image = %config.image, "image pull complete");

        let docker_config = build_docker_config(&config, &sandbox_id_str);
        let options = CreateContainerOptions {
            name: name.as_str(),
            platform: None,
        };

        let created = self
            .client
            .create_container(Some(options), docker_config)
            .await
            .map_err(runtime_err)?;

        self.client
            .start_container::<String>(&created.id, None)
            .await
            .map_err(runtime_err)?;

        let inspect = self
            .client
            .inspect_container(&created.id, None)
            .await
            .map_err(runtime_err)?;

        let host_port = extract_host_port(&inspect, port_hint.as_deref())?;
        info!(container_id = %created.id, sandbox_id = %sandbox_id_str, host_port, "container started");

        Ok(ContainerInfo {
            id: ContainerId(created.id),
            sandbox_id: config.sandbox_id,
            host_port,
            running: true,
        })
    }

    async fn stop_and_remove(&self, id: &ContainerId, timeout: Duration) -> Result<(), AgentError> {
        let stop_opts = StopContainerOptions {
            t: timeout.as_secs() as i64,
        };
        let _ = self.client.stop_container(&id.0, Some(stop_opts)).await;

        let remove_opts = RemoveContainerOptions {
            force: true,
            v: true,
            ..Default::default()
        };
        self.client
            .remove_container(&id.0, Some(remove_opts))
            .await
            .map_err(runtime_err)?;

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
            .map_err(runtime_err)?;

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

            let running = container.state.as_deref().is_some_and(|s| s == "running");

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

    async fn exec(
        &self,
        id: &ContainerId,
        options: ExecOptions,
    ) -> Result<ExecOutput, AgentError> {
        let attach_stdin = !options.stdin.is_empty();
        let working_dir = if options.cwd.is_empty() {
            None
        } else {
            Some(options.cwd)
        };
        let exec_opts = CreateExecOptions {
            cmd: Some(options.command),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            attach_stdin: Some(attach_stdin),
            working_dir,
            ..Default::default()
        };
        let stdin = options.stdin;

        let exec_created = self
            .client
            .create_exec(&id.0, exec_opts)
            .await
            .map_err(runtime_err)?;

        let start_opts = StartExecOptions {
            detach: false,
            ..Default::default()
        };

        let result = self
            .client
            .start_exec(&exec_created.id, Some(start_opts))
            .await
            .map_err(runtime_err)?;

        let mut stdout_buf = Vec::new();
        let mut stderr_buf = Vec::new();

        if let StartExecResults::Attached {
            mut output,
            mut input,
        } = result
        {
            if !stdin.is_empty() {
                use tokio::io::AsyncWriteExt;
                input
                    .write_all(&stdin)
                    .await
                    .map_err(|e| AgentError::Runtime {
                        detail: e.to_string(),
                    })?;
                input.shutdown().await.map_err(|e| AgentError::Runtime {
                    detail: e.to_string(),
                })?;
            }

            while let Some(chunk) = output.next().await {
                let chunk = chunk.map_err(runtime_err)?;
                match chunk {
                    bollard::container::LogOutput::StdOut { message } => {
                        stdout_buf.extend_from_slice(&message);
                    }
                    bollard::container::LogOutput::StdErr { message } => {
                        stderr_buf.extend_from_slice(&message);
                    }
                    _ => {}
                }
            }
        }

        let inspect = self
            .client
            .inspect_exec(&exec_created.id)
            .await
            .map_err(runtime_err)?;

        let exit_code = inspect.exit_code.unwrap_or(-1) as i32;

        // Docker pipes the OCI runtime's "command not found" diagnostic to
        // stdout (bug #8 in the SDK friction report) when the exec syscall
        // fails. Detect the pattern on either stream and lift it into stderr
        // so callers don't have to look at stdout for runtime-level errors.
        let mut cnf = false;
        if exit_code == 127 {
            if open_sandbox_agent::container::detect_command_not_found(&stderr_buf) {
                cnf = true;
            } else if open_sandbox_agent::container::detect_command_not_found(&stdout_buf) {
                cnf = true;
                stderr_buf.extend_from_slice(&stdout_buf);
                stdout_buf.clear();
            }
        }

        Ok(ExecOutput {
            exit_code,
            stdout: stdout_buf,
            stderr: stderr_buf,
            command_not_found: cnf,
        })
    }
}

fn build_docker_config(config: &ContainerConfig, sandbox_id_str: &str) -> Config<String> {
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
        (
            LABEL_MANAGED_BY.to_string(),
            LABEL_MANAGED_BY_VALUE.to_string(),
        ),
        (LABEL_SANDBOX_ID.to_string(), sandbox_id_str.to_string()),
    ]);

    let exposed_ports = if config.exposed_port > 0 {
        let port_key = format!("{}/tcp", config.exposed_port);
        Some(HashMap::from([(port_key, HashMap::new())]))
    } else {
        None
    };

    Config::<String> {
        image: Some(config.image.clone()),
        cmd: Some(
            open_sandbox_contracts::constants::DEFAULT_SANDBOX_ENTRYPOINT
                .iter()
                .map(|s| s.to_string())
                .collect(),
        ),
        labels: Some(labels),
        exposed_ports,
        host_config: Some(host_config),
        env: Some(env_vec),
        ..Default::default()
    }
}

fn extract_host_port(
    inspect: &bollard::models::ContainerInspectResponse,
    port_hint: Option<&str>,
) -> Result<u16, AgentError> {
    let ports = inspect
        .network_settings
        .as_ref()
        .and_then(|ns| ns.ports.as_ref())
        .ok_or_else(|| AgentError::Runtime {
            detail: "no port mappings found".into(),
        })?;

    let bindings = if let Some(key) = port_hint {
        ports.get(key).and_then(|b| b.as_ref())
    } else {
        ports.values().find_map(|b| b.as_ref())
    }
    .ok_or_else(|| AgentError::Runtime {
        detail: "no port bindings available".into(),
    })?;

    bindings
        .first()
        .and_then(|b| b.host_port.as_ref())
        .and_then(|p| p.parse::<u16>().ok())
        .ok_or_else(|| AgentError::Runtime {
            detail: "could not parse host port".into(),
        })
}

fn runtime_err(e: bollard::errors::Error) -> AgentError {
    AgentError::Runtime {
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
