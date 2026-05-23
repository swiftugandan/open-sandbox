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
    ExecExitInfo, ExecHandle, ExecStart, consume_inpid_marker, detect_command_not_found,
    wrap_command_with_inpid_marker,
};
use open_sandbox_contracts::constants::DEFAULT_WRITE_CWD;
use open_sandbox_contracts::error::AgentError;
use open_sandbox_contracts::types::SandboxId;

const LABEL_MANAGED_BY: &str = "open-sandbox.managed-by";
const LABEL_MANAGED_BY_VALUE: &str = "open-sandbox-agent";
const LABEL_SANDBOX_ID: &str = "open-sandbox.sandbox-id";
const NANOCPUS_PER_MILLICPU: i64 = 1_000_000;

/// Hard cap on `read_file` output. Comp-4: without this, a request for a
/// multi-gigabyte file inside the sandbox grows Vec<u8> until the agent
/// OOMs and tears down every sandbox on the host.
pub const MAX_READ_BYTES: usize = 256 * 1024 * 1024;

pub struct DockerRuntime {
    client: bollard::Docker,
}

impl DockerRuntime {
    pub fn connect() -> Result<Self, AgentError> {
        let client = bollard::Docker::connect_with_local_defaults().map_err(runtime_err)?;
        Ok(Self { client })
    }

    /// Best-effort force-remove used by create_and_start rollback paths.
    /// Comp-4: errors here are logged but never propagated — the caller
    /// surfaces the original failure to the controller.
    async fn force_remove(&self, container_id: &str) {
        let opts = RemoveContainerOptions {
            force: true,
            v: true,
            ..Default::default()
        };
        if let Err(e) = self.client.remove_container(container_id, Some(opts)).await {
            warn!(container_id, error = %e, "rollback remove_container failed");
        }
    }

    /// Best-effort cleanup of a `.opensb.<uuid>.tmp` file left behind when a
    /// write_file step fails. Comp-4: previously orphans accumulated in the
    /// container's target directory.
    async fn cleanup_tmp_file(&self, container_id: &ContainerId, tmp_path: &str) {
        let rm = self
            .start_exec(
                container_id,
                ExecStart {
                    command: vec!["rm".into(), "-f".into(), tmp_path.to_string()],
                    cwd: String::new(),
                    env: HashMap::new(),
                },
            )
            .await;
        if let Ok(handle) = rm {
            if let Err(e) = drain_to_exit(handle).await {
                warn!(tmp = %tmp_path, error = %e, "tmp file cleanup failed");
            }
        }
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

        // Comp-4: any failure between create_container and the final Ok must
        // remove the orphaned container, or repeated retries leave dead
        // entries piling up under the `sandbox-<uuid>` name and a future
        // recreate hits a name collision.
        if let Err(e) = self
            .client
            .start_container::<String>(&created.id, None)
            .await
            .map_err(runtime_err)
        {
            self.force_remove(&created.id).await;
            return Err(e);
        }

        let inspect = match self.client.inspect_container(&created.id, None).await {
            Ok(i) => i,
            Err(e) => {
                self.force_remove(&created.id).await;
                return Err(runtime_err(e));
            }
        };

        let host_port = match extract_host_port(&inspect, port_hint.as_deref()) {
            Ok(p) => p,
            Err(e) => {
                self.force_remove(&created.id).await;
                return Err(e);
            }
        };
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

        // Wrap so the in-container process self-reports its
        // namespace-local PID on stderr before exec'ing the user's
        // command. See `wrap_command_with_inpid_marker` for the why
        // — bollard's `inspect_exec.pid` is the HOST PID, useless
        // for `kill` inside the container's PID namespace.
        let wrapped_cmd = wrap_command_with_inpid_marker(start.command);

        let exec_opts = CreateExecOptions {
            cmd: Some(wrapped_cmd),
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

        // Build the caller-facing channels.
        let (stdin_tx, mut stdin_rx) = mpsc::channel::<Bytes>(EXEC_CHANNEL_CAPACITY);
        let (stdout_tx, stdout_rx) = mpsc::channel::<Bytes>(EXEC_CHANNEL_CAPACITY);
        let (stderr_tx, stderr_rx) = mpsc::channel::<Bytes>(EXEC_CHANNEL_CAPACITY);
        let (exited_tx, exited_rx) = oneshot::channel::<ExecExitInfo>();
        // The output pump consumes the leading `OPENSB_INPID=<n>\n`
        // line from stderr and sends the pid here. start_exec
        // awaits with a small timeout before returning.
        let (inpid_tx, inpid_rx) = oneshot::channel::<i32>();

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
        // demux StdOut/StdErr to the two channels, strip the
        // leading OPENSB_INPID marker from stderr, then on stream
        // end inspect_exec for the exit code and signal exited.
        let client = self.client.clone();
        let exec_id_for_watch = exec_id.clone();
        tokio::spawn(async move {
            let mut output = output;
            let mut stdout_buf_for_cnf: Vec<u8> = Vec::new();
            let mut stderr_buf_for_cnf: Vec<u8> = Vec::new();
            // INPID extraction state: keep buffering stderr bytes
            // into `inpid_scan_buf` until we either parse the
            // marker (good) or give up (parser returns Err). Once
            // we leave this mode (`inpid_done`), stderr passes
            // through unchanged.
            let mut inpid_scan_buf: Vec<u8> = Vec::new();
            let mut inpid_done = false;
            let mut inpid_tx_slot = Some(inpid_tx);
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
                        if !inpid_done {
                            inpid_scan_buf.extend_from_slice(&m);
                            match consume_inpid_marker(&mut inpid_scan_buf) {
                                Ok(Some(pid)) => {
                                    if let Some(tx) = inpid_tx_slot.take() {
                                        let _ = tx.send(pid);
                                    }
                                    inpid_done = true;
                                    if inpid_scan_buf.is_empty() {
                                        continue;
                                    }
                                    let rest = std::mem::take(&mut inpid_scan_buf);
                                    if stderr_buf_for_cnf.len() < 4096 {
                                        stderr_buf_for_cnf.extend_from_slice(&rest);
                                    }
                                    if stderr_tx.send(Bytes::from(rest)).await.is_err() {
                                        return;
                                    }
                                }
                                Ok(None) => {
                                    // keep waiting for the newline
                                }
                                Err(()) => {
                                    // Marker did not arrive; drop
                                    // the inpid sender so the
                                    // waiter unblocks with a None.
                                    inpid_tx_slot.take();
                                    inpid_done = true;
                                    let rest = std::mem::take(&mut inpid_scan_buf);
                                    if stderr_buf_for_cnf.len() < 4096 {
                                        stderr_buf_for_cnf.extend_from_slice(&rest);
                                    }
                                    if stderr_tx.send(Bytes::from(rest)).await.is_err() {
                                        return;
                                    }
                                }
                            }
                        } else {
                            if stderr_buf_for_cnf.len() < 4096 {
                                stderr_buf_for_cnf.extend_from_slice(&m);
                            }
                            if stderr_tx.send(m).await.is_err() {
                                return;
                            }
                        }
                    }
                    _ => {}
                }
            }
            // Stream ended before the marker arrived (e.g. a
            // microsecond-lived exec). Make sure the start_exec
            // waiter unblocks.
            drop(inpid_tx_slot);

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

        // Wait for the wrapper to emit OPENSB_INPID. Comp-4: bumped from
        // 1s to 5s to tolerate cold containers and cgroup-throttled
        // shells. If the marker still doesn't arrive, fail loud rather
        // than silently returning pid=0 — a pid=0 ExecHandle makes
        // signal_exec a no-op, breaking the spike-01 guarantee that the
        // agent kills the in-container PID on client disconnect.
        let in_container_pid = match tokio::time::timeout(Duration::from_secs(5), inpid_rx).await {
            Ok(Ok(pid)) if pid > 0 => pid,
            Ok(Ok(pid)) => {
                return Err(AgentError::Internal {
                    detail: format!(
                        "OPENSB_INPID wrapper reported invalid pid={pid}; refusing to start exec"
                    ),
                });
            }
            Ok(Err(_)) | Err(_) => {
                return Err(AgentError::Internal {
                    detail:
                        "OPENSB_INPID marker not received within 5s; refusing to start exec \
                         (would break disconnect-driven kill)"
                            .into(),
                });
            }
        };

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
                    command: vec!["cat".into(), resolved.clone()],
                    cwd: String::new(),
                    env: HashMap::new(),
                },
            )
            .await?;

        // Drain stdout fully; ignore stdin. Comp-4: cap the accumulated
        // bytes at MAX_READ_BYTES so a `cat /dev/zero` or a huge in-sandbox
        // file can't OOM the agent process.
        let mut handle = handle;
        drop(handle.stdin);
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = handle.stdout.recv().await {
            if buf.len().saturating_add(chunk.len()) > MAX_READ_BYTES {
                return Err(AgentError::Runtime {
                    detail: format!(
                        "read_file output exceeds {MAX_READ_BYTES}-byte cap"
                    ),
                });
            }
            buf.extend_from_slice(&chunk);
        }
        // Also drain stderr to allow the exec to terminate cleanly.
        let mut stderr: Vec<u8> = Vec::new();
        while let Some(chunk) = handle.stderr.recv().await {
            stderr.extend_from_slice(&chunk);
        }
        let info = handle.exited.await.map_err(|_| AgentError::Runtime {
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

        // Step 1: ensure the directory exists. Note: no `--` —
        // busybox utilities in alpine images don't accept it.
        let mkdir = self
            .start_exec(
                id,
                ExecStart {
                    command: vec!["mkdir".into(), "-p".into(), dir.clone()],
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
                    command: vec!["tee".into(), temp.clone()],
                    cwd: String::new(),
                    env: HashMap::new(),
                },
            )
            .await?;
        let mut writer = writer;
        // Comp-4: check the stdin send — previously `let _ = ...` silently
        // dropped data on pipe error, leaving an empty file with exit 0
        // (tee exits cleanly on no-input). Surface the failure instead.
        if let Err(e) = writer.stdin.send(content).await {
            // Clean up the half-written tmp file before returning.
            self.cleanup_tmp_file(id, &temp).await;
            return Err(AgentError::Runtime {
                detail: format!("write_file tee stdin send failed: {e}"),
            });
        }
        drop(writer.stdin); // signal EOF
        // Drain stdout (tee echoes content) and stderr.
        while writer.stdout.recv().await.is_some() {}
        let mut stderr = Vec::new();
        while let Some(chunk) = writer.stderr.recv().await {
            stderr.extend_from_slice(&chunk);
        }
        let info = writer.exited.await.map_err(|_| AgentError::Runtime {
            detail: "write_file tee exec terminated without exit info".into(),
        })?;
        if info.exit_code != 0 {
            self.cleanup_tmp_file(id, &temp).await;
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
                    command: vec!["mv".into(), temp.clone(), resolved.clone()],
                    cwd: String::new(),
                    env: HashMap::new(),
                },
            )
            .await?;
        // Comp-4: on mv failure, remove the orphan tmp file so a sandbox
        // doing repeated failing writes can't accumulate garbage in the
        // target dir until the container is destroyed.
        if let Err(e) = drain_to_exit(mv).await {
            self.cleanup_tmp_file(id, &temp).await;
            return Err(e);
        }

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
                    command: vec!["mkdir".into(), "-p".into(), target_cwd.clone()],
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
        // Comp-4: same silent-stdin-error fix as write_file.
        if let Err(e) = extract.stdin.send(tarball).await {
            return Err(AgentError::Runtime {
                detail: format!("write_files_targz tar stdin send failed: {e}"),
            });
        }
        drop(extract.stdin);
        while extract.stdout.recv().await.is_some() {}
        let mut stderr = Vec::new();
        while let Some(chunk) = extract.stderr.recv().await {
            stderr.extend_from_slice(&chunk);
        }
        let info = extract.exited.await.map_err(|_| AgentError::Runtime {
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
    let info = handle.exited.await.map_err(|_| AgentError::Runtime {
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
