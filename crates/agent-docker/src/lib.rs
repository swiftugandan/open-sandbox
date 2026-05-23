use std::collections::HashMap;
use std::time::Duration;

use bollard::container::{
    Config, CreateContainerOptions, ListContainersOptions, RemoveContainerOptions,
    StopContainerOptions,
};
use bollard::exec::{CreateExecOptions, StartExecOptions, StartExecResults};
use bollard::models::HostConfig;
use bytes::Bytes;
use futures::{StreamExt, TryStreamExt};
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot};
use tracing::{info, warn};

use open_sandbox_agent::container::{
    ContainerConfig, ContainerId, ContainerInfo, ContainerRuntime, EXEC_CHANNEL_CAPACITY,
    ExecExitInfo, ExecHandle, ExecStart, detect_command_not_found,
};
use open_sandbox_contracts::constants::DEFAULT_WRITE_CWD;
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

    async fn start_exec(
        &self,
        id: &ContainerId,
        start: ExecStart,
    ) -> Result<ExecHandle, AgentError> {
        let working_dir = if start.cwd.is_empty() {
            None
        } else {
            Some(start.cwd)
        };

        let env: Vec<String> = if start.env.is_empty() {
            Vec::new()
        } else {
            start.env.iter().map(|(k, v)| format!("{k}={v}")).collect()
        };

        let exec_opts = CreateExecOptions {
            cmd: Some(start.command),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            attach_stdin: Some(true),
            working_dir,
            env: if env.is_empty() { None } else { Some(env) },
            ..Default::default()
        };

        let exec_created = self
            .client
            .create_exec(&id.0, exec_opts)
            .await
            .map_err(runtime_err)?;
        let exec_id = exec_created.id.clone();

        let started = self
            .client
            .start_exec(
                &exec_id,
                Some(StartExecOptions {
                    detach: false,
                    ..Default::default()
                }),
            )
            .await
            .map_err(runtime_err)?;

        let StartExecResults::Attached { output, input } = started else {
            return Err(AgentError::Runtime {
                detail: "start_exec returned non-attached result".into(),
            });
        };

        // The Pid is populated by dockerd shortly after start. Poll
        // briefly — same shape as spike 05's strategy for youki, but
        // here via the bollard inspect API.
        let in_container_pid = {
            let mut found: Option<i32> = None;
            for _ in 0..10 {
                let info = self
                    .client
                    .inspect_exec(&exec_id)
                    .await
                    .map_err(runtime_err)?;
                if let Some(pid) = info.pid
                    && pid > 0
                {
                    found = Some(pid as i32);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            // It's not fatal if we don't capture a PID: a
            // microsecond-lifetime exec may have already exited. The
            // registry will then have nothing to clean up (already
            // dead). Use 0 as a sentinel meaning "no PID captured."
            found.unwrap_or(0)
        };

        // Build the caller-facing channels.
        let (stdin_tx, mut stdin_rx) = mpsc::channel::<Bytes>(EXEC_CHANNEL_CAPACITY);
        let (stdout_tx, stdout_rx) = mpsc::channel::<Bytes>(EXEC_CHANNEL_CAPACITY);
        let (stderr_tx, stderr_rx) = mpsc::channel::<Bytes>(EXEC_CHANNEL_CAPACITY);
        let (exited_tx, exited_rx) = oneshot::channel::<ExecExitInfo>();

        // stdin pump: receive from caller, write to bollard input.
        // Drop of stdin_rx (channel closed) triggers input.shutdown().
        tokio::spawn(async move {
            let mut input = input;
            while let Some(bytes) = stdin_rx.recv().await {
                if input.write_all(&bytes).await.is_err() {
                    return;
                }
            }
            let _ = input.shutdown().await;
        });

        // output pump + exit watcher: consume bollard output Stream,
        // demux StdOut/StdErr to the two channels, then on stream end
        // inspect_exec for the exit code and signal exited.
        let client = self.client.clone();
        let exec_id_for_watch = exec_id.clone();
        tokio::spawn(async move {
            let mut output = output;
            let mut stdout_buf_for_cnf: Vec<u8> = Vec::new();
            let mut stderr_buf_for_cnf: Vec<u8> = Vec::new();
            while let Some(chunk_res) = output.next().await {
                let Ok(chunk) = chunk_res else { break };
                match chunk {
                    bollard::container::LogOutput::StdOut { message } => {
                        let m: Bytes = message;
                        if stdout_buf_for_cnf.len() < 4096 {
                            stdout_buf_for_cnf.extend_from_slice(&m);
                        }
                        if stdout_tx.send(m).await.is_err() {
                            // Consumer hung up; drain remaining.
                            return;
                        }
                    }
                    bollard::container::LogOutput::StdErr { message } => {
                        let m: Bytes = message;
                        if stderr_buf_for_cnf.len() < 4096 {
                            stderr_buf_for_cnf.extend_from_slice(&m);
                        }
                        if stderr_tx.send(m).await.is_err() {
                            return;
                        }
                    }
                    _ => {}
                }
            }

            // Stream ended — fetch exit code.
            let inspect = match client.inspect_exec(&exec_id_for_watch).await {
                Ok(i) => i,
                Err(_) => {
                    let _ = exited_tx.send(ExecExitInfo {
                        exit_code: -1,
                        command_not_found: false,
                    });
                    return;
                }
            };
            let exit_code = inspect.exit_code.unwrap_or(-1) as i32;

            // Spike-08 / friction-fix item #8: docker pipes the OCI
            // "executable not found" diagnostic to stdout. Detect on
            // either stream when exit is 127.
            let cnf = exit_code == 127
                && (detect_command_not_found(&stderr_buf_for_cnf)
                    || detect_command_not_found(&stdout_buf_for_cnf));

            let _ = exited_tx.send(ExecExitInfo {
                exit_code,
                command_not_found: cnf,
            });
        });

        Ok(ExecHandle {
            exec_id,
            in_container_pid,
            stdin: stdin_tx,
            stdout: stdout_rx,
            stderr: stderr_rx,
            exited: exited_rx,
        })
    }

    async fn signal_exec(
        &self,
        id: &ContainerId,
        in_container_pid: i32,
        signum: i32,
    ) -> Result<(), AgentError> {
        if in_container_pid <= 0 {
            // No PID captured at start (microsecond exec). Nothing
            // to kill.
            return Ok(());
        }

        // Issue `kill -<signum> <pid>` inside the container via a
        // short docker exec. Requires `kill` in the sandbox image —
        // documented as a sandbox-image requirement in SPEC.md.
        let cmd: Vec<String> = vec![
            "kill".into(),
            format!("-{signum}"),
            in_container_pid.to_string(),
        ];
        let exec = self
            .client
            .create_exec(
                &id.0,
                CreateExecOptions {
                    cmd: Some(cmd),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    ..Default::default()
                },
            )
            .await
            .map_err(runtime_err)?;

        let _ = self
            .client
            .start_exec(
                &exec.id,
                Some(StartExecOptions {
                    detach: false,
                    ..Default::default()
                }),
            )
            .await
            .map_err(runtime_err)?;

        // Inspect to surface the result for telemetry; non-zero
        // typically means the target has already exited (ESRCH),
        // which is benign for our use case.
        let _ = self.client.inspect_exec(&exec.id).await;
        Ok(())
    }

    async fn read_file(
        &self,
        id: &ContainerId,
        path: &str,
        cwd: Option<&str>,
    ) -> Result<Bytes, AgentError> {
        let resolved = resolve_path(path, cwd);

        // Single-command `cat -- <resolved>` exec. Internal; agent
        // logs show this as the runtime's read_file, not as a
        // gateway-emitted shell helper.
        let handle = self
            .start_exec(
                id,
                ExecStart {
                    command: vec!["cat".into(), "--".into(), resolved.clone()],
                    cwd: String::new(),
                    env: HashMap::new(),
                },
            )
            .await?;

        // Drain stdout fully; ignore stdin.
        let mut handle = handle;
        drop(handle.stdin);
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = handle.stdout.recv().await {
            buf.extend_from_slice(&chunk);
        }
        // Also drain stderr to allow the exec to terminate cleanly.
        let mut stderr: Vec<u8> = Vec::new();
        while let Some(chunk) = handle.stderr.recv().await {
            stderr.extend_from_slice(&chunk);
        }
        let info = handle
            .exited
            .await
            .map_err(|_| AgentError::Runtime {
                detail: "read_file exec terminated without exit info".into(),
            })?;

        if info.exit_code != 0 {
            let stderr_text = String::from_utf8_lossy(&stderr);
            if stderr_text.contains("No such file") || stderr_text.contains("not found") {
                return Err(AgentError::Runtime {
                    detail: format!("No such file: {resolved}"),
                });
            }
            return Err(AgentError::Runtime {
                detail: format!(
                    "read_file exit={} stderr={}",
                    info.exit_code,
                    stderr_text.trim()
                ),
            });
        }

        Ok(Bytes::from(buf))
    }

    async fn write_file(
        &self,
        id: &ContainerId,
        path: &str,
        cwd: Option<&str>,
        content: Bytes,
    ) -> Result<(), AgentError> {
        let resolved = resolve_path(path, cwd);
        // Atomicity: write to a temp file in the target's directory,
        // then rename. Two short execs; both visible to logs as
        // first-class runtime operations (no embedded shell script).
        let dir = parent_dir(&resolved);
        let temp = format!("{dir}/.opensb.{}.tmp", uuid::Uuid::new_v4().simple());

        // Step 1: ensure the directory exists.
        let mkdir = self
            .start_exec(
                id,
                ExecStart {
                    command: vec!["mkdir".into(), "-p".into(), "--".into(), dir.clone()],
                    cwd: String::new(),
                    env: HashMap::new(),
                },
            )
            .await?;
        drain_to_exit(mkdir).await?;

        // Step 2: write content via tee into the temp path.
        let writer = self
            .start_exec(
                id,
                ExecStart {
                    command: vec!["tee".into(), "--".into(), temp.clone()],
                    cwd: String::new(),
                    env: HashMap::new(),
                },
            )
            .await?;
        let mut writer = writer;
        // Push content as a single chunk (caller's responsibility
        // to keep payload reasonable for unary write_file).
        let _ = writer.stdin.send(content).await;
        drop(writer.stdin); // signal EOF
        // Drain stdout (tee echoes content) and stderr.
        while writer.stdout.recv().await.is_some() {}
        let mut stderr = Vec::new();
        while let Some(chunk) = writer.stderr.recv().await {
            stderr.extend_from_slice(&chunk);
        }
        let info = writer
            .exited
            .await
            .map_err(|_| AgentError::Runtime {
                detail: "write_file tee exec terminated without exit info".into(),
            })?;
        if info.exit_code != 0 {
            return Err(AgentError::Runtime {
                detail: format!(
                    "write_file tee exit={} stderr={}",
                    info.exit_code,
                    String::from_utf8_lossy(&stderr).trim()
                ),
            });
        }

        // Step 3: atomic rename into place.
        let mv = self
            .start_exec(
                id,
                ExecStart {
                    command: vec!["mv".into(), "--".into(), temp.clone(), resolved.clone()],
                    cwd: String::new(),
                    env: HashMap::new(),
                },
            )
            .await?;
        drain_to_exit(mv).await?;

        Ok(())
    }

    async fn write_files_targz(
        &self,
        id: &ContainerId,
        cwd: Option<&str>,
        tarball: Bytes,
    ) -> Result<(), AgentError> {
        let target_cwd = cwd.unwrap_or(DEFAULT_WRITE_CWD).to_string();

        // mkdir -p target, then tar xzf - -C target. Two short
        // execs, both first-class operations.
        let mkdir = self
            .start_exec(
                id,
                ExecStart {
                    command: vec!["mkdir".into(), "-p".into(), "--".into(), target_cwd.clone()],
                    cwd: String::new(),
                    env: HashMap::new(),
                },
            )
            .await?;
        drain_to_exit(mkdir).await?;

        let extract = self
            .start_exec(
                id,
                ExecStart {
                    command: vec![
                        "tar".into(),
                        "xzf".into(),
                        "-".into(),
                        "-C".into(),
                        target_cwd,
                    ],
                    cwd: String::new(),
                    env: HashMap::new(),
                },
            )
            .await?;
        let mut extract = extract;
        let _ = extract.stdin.send(tarball).await;
        drop(extract.stdin);
        while extract.stdout.recv().await.is_some() {}
        let mut stderr = Vec::new();
        while let Some(chunk) = extract.stderr.recv().await {
            stderr.extend_from_slice(&chunk);
        }
        let info = extract
            .exited
            .await
            .map_err(|_| AgentError::Runtime {
                detail: "write_files_targz tar exec terminated without exit info".into(),
            })?;
        if info.exit_code != 0 {
            return Err(AgentError::Runtime {
                detail: format!(
                    "write_files_targz tar exit={} stderr={}",
                    info.exit_code,
                    String::from_utf8_lossy(&stderr).trim()
                ),
            });
        }

        Ok(())
    }
}

async fn drain_to_exit(handle: ExecHandle) -> Result<(), AgentError> {
    let mut handle = handle;
    drop(handle.stdin);
    while handle.stdout.recv().await.is_some() {}
    let mut stderr = Vec::new();
    while let Some(chunk) = handle.stderr.recv().await {
        stderr.extend_from_slice(&chunk);
    }
    let info = handle
        .exited
        .await
        .map_err(|_| AgentError::Runtime {
            detail: "internal exec terminated without exit info".into(),
        })?;
    if info.exit_code != 0 {
        warn!(
            exit_code = info.exit_code,
            stderr = %String::from_utf8_lossy(&stderr).trim(),
            "internal runtime exec failed"
        );
        return Err(AgentError::Runtime {
            detail: format!(
                "internal exec exit={} stderr={}",
                info.exit_code,
                String::from_utf8_lossy(&stderr).trim()
            ),
        });
    }
    Ok(())
}

fn resolve_path(path: &str, cwd: Option<&str>) -> String {
    if path.starts_with('/') {
        return path.to_string();
    }
    let base = cwd.unwrap_or(DEFAULT_WRITE_CWD);
    format!("{}/{}", base.trim_end_matches('/'), path)
}

fn parent_dir(path: &str) -> String {
    match path.rfind('/') {
        Some(0) => "/".to_string(),
        Some(n) => path[..n].to_string(),
        None => ".".to_string(),
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
