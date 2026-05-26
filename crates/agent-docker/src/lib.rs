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

/// Tri-state result of probing whether an image is locally cached.
/// `Unknown` captures the inspect_image-returned-non-404 case (daemon
/// flake, schema drift, etc.) where we can't say either way — the
/// pull/create logic treats Unknown conservatively (will pull on
/// IfNotPresent; will fail on Never).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Presence {
    Present,
    Absent,
    Unknown,
}

pub struct DockerRuntime {
    client: bollard::Docker,
}

impl DockerRuntime {
    pub fn connect() -> Result<Self, AgentError> {
        let client = bollard::Docker::connect_with_local_defaults().map_err(runtime_err)?;
        Ok(Self { client })
    }

    /// Pull `image` with bounded exponential backoff. Comp-4: transient
    /// registry hiccups (rate-limit, 5xx, brief network drops) used to
    /// abort create_and_start with no signal that the failure was
    /// retryable; the controller's scheduler then reported FAILED and
    /// operators thought the image was unreachable.
    ///
    /// Called from create_and_start both on cold-cache (inspect_image
    /// returned 404) and as a fallback when create_container itself
    /// reports the image missing after a successful inspect (TOCTOU
    /// prune race, partial layer GC).
    async fn pull_image_with_retry(&self, image: &str) -> Result<(), AgentError> {
        const MAX_PULL_ATTEMPTS: u32 = 4;
        const PULL_BASE_DELAY_MS: u64 = 500;
        info!(image, "pulling image");
        let mut last_err: Option<AgentError> = None;
        for attempt in 1..=MAX_PULL_ATTEMPTS {
            let result = self
                .client
                .create_image(
                    Some(bollard::image::CreateImageOptions {
                        from_image: image.to_string(),
                        ..Default::default()
                    }),
                    None,
                    None,
                )
                .try_collect::<Vec<_>>()
                .await;
            match result {
                Ok(_) => {
                    info!(image, "image pull complete");
                    return Ok(());
                }
                Err(e) if attempt < MAX_PULL_ATTEMPTS => {
                    let delay_ms = PULL_BASE_DELAY_MS * (1u64 << (attempt - 1));
                    warn!(
                        image,
                        attempt,
                        max = MAX_PULL_ATTEMPTS,
                        delay_ms,
                        error = %e,
                        "image pull failed; retrying"
                    );
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    last_err = Some(runtime_err(e));
                }
                Err(e) => {
                    last_err = Some(runtime_err(e));
                }
            }
        }
        Err(last_err.expect("loop exits via Ok or sets last_err on the final attempt"))
    }

    /// Best-effort force-remove used by create_and_start rollback paths.
    /// Comp-4: errors here are logged but never propagated — the caller
    /// surfaces the original failure to the controller.
    ///
    /// `target` accepts either a container id (from `created.id` on
    /// rollback paths) OR a container name (from `name.as_str()` on
    /// the 409 name-conflict recovery arm). Docker's HTTP API at
    /// `/containers/{id}` treats both equivalently — bollard does no
    /// distinction. We log the value under `target` because either form
    /// may appear; the older field name `container_id` would mislead
    /// operators grepping logs by id-shape.
    async fn force_remove(&self, target: &str) {
        let opts = RemoveContainerOptions {
            force: true,
            v: true,
            ..Default::default()
        };
        if let Err(e) = self.client.remove_container(target, Some(opts)).await {
            warn!(target, error = %e, "rollback remove_container failed");
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

        // v1.0.2: honor caller-supplied pull_policy. Defaults to
        // IfNotPresent — matches `docker run` semantics, which is also
        // what the dev fleet measured as the warm-path optimum
        // (~1.1s p50 saved vs the v1.0.1 always-pull behavior).
        //
        // The flow is:
        //   1. Probe local presence via inspect_image (tri-state:
        //      Present | Absent | Unknown). Skipped for Always since
        //      we're going to pull anyway.
        //   2. Pull (or refuse) per policy.
        //   3. Attempt create_container. On 404 (TOCTOU prune race or
        //      partial layer GC under disk pressure), pull and retry
        //      once unless policy is Never. The retry covers the
        //      Unknown-presence case too.
        use open_sandbox_contracts::types::PullPolicy;
        let policy = config.pull_policy;

        let presence = match policy {
            PullPolicy::Always => Presence::Unknown, // unused; we pull unconditionally
            PullPolicy::IfNotPresent | PullPolicy::Never => {
                match self.client.inspect_image(&config.image).await {
                    Ok(_) => Presence::Present,
                    Err(bollard::errors::Error::DockerResponseServerError {
                        status_code: 404,
                        ..
                    }) => Presence::Absent,
                    Err(e) => {
                        warn!(image = %config.image, error = %e, "inspect_image failed");
                        Presence::Unknown
                    }
                }
            }
        };

        match policy {
            PullPolicy::Always => {
                self.pull_image_with_retry(&config.image).await?;
            }
            PullPolicy::IfNotPresent => match presence {
                Presence::Present => {
                    info!(image = %config.image, "image present locally; skipping pull");
                }
                Presence::Absent | Presence::Unknown => {
                    self.pull_image_with_retry(&config.image).await?;
                }
            },
            PullPolicy::Never => match presence {
                Presence::Present => {
                    info!(image = %config.image, "image present locally; pull_policy=never");
                }
                Presence::Absent => {
                    return Err(AgentError::Runtime {
                        detail: format!(
                            "image {} not present locally and pull_policy=never",
                            config.image
                        ),
                    });
                }
                Presence::Unknown => {
                    return Err(AgentError::Runtime {
                        detail: format!(
                            "could not verify image {} presence and pull_policy=never",
                            config.image
                        ),
                    });
                }
            },
        }

        // v1.0.2 (iter4+iter5): bounded retry loop around create+start
        // for port-collision recovery. Pre-allocate the host port,
        // skip inspect_container, and on `start_container` EADDRINUSE
        // force-remove the orphan + allocate a fresh port + retry.
        // v1.0.2 (iter6+iter7+iter8): also recovers 404 image-missing
        // (TOCTOU prune race) and 409 name-conflict (stale orphan from
        // crashed prior agent) by advancing the outer iteration on
        // non-final-attempt failure.
        const MAX_PORT_RETRY_ATTEMPTS: u32 = 3;
        let options = CreateContainerOptions {
            name: name.as_str(),
            platform: None,
        };

        for attempt in 1..=MAX_PORT_RETRY_ATTEMPTS {
            let final_attempt = attempt == MAX_PORT_RETRY_ATTEMPTS;
            let host_port_opt = if config.exposed_port > 0 {
                Some(allocate_free_host_port()?)
            } else {
                None
            };
            let docker_config = build_docker_config(&config, &sandbox_id_str, host_port_opt);

            let created = match self
                .client
                .create_container(Some(options.clone()), docker_config.clone())
                .await
            {
                Ok(c) => c,
                Err(bollard::errors::Error::DockerResponseServerError {
                    status_code: 404, ..
                }) if !matches!(policy, PullPolicy::Never) => {
                    warn!(image = %config.image, "create_container reported image missing; refreshing image and retrying once");
                    // iter8: pull failure on non-final attempt also
                    // advances the outer loop rather than propagating.
                    match self.pull_image_with_retry(&config.image).await {
                        Ok(()) => {}
                        Err(e) if !final_attempt => {
                            warn!(
                                error = %e,
                                attempt,
                                max = MAX_PORT_RETRY_ATTEMPTS,
                                "pull-recovery exhausted retries; advancing outer port-retry"
                            );
                            continue;
                        }
                        Err(e) => return Err(e),
                    }
                    match self
                        .client
                        .create_container(Some(options.clone()), docker_config.clone())
                        .await
                    {
                        Ok(c) => c,
                        Err(e) if !final_attempt => {
                            warn!(
                                error = %e,
                                attempt,
                                max = MAX_PORT_RETRY_ATTEMPTS,
                                "create_container failed after pull-recovery; advancing outer port-retry"
                            );
                            continue;
                        }
                        Err(e) => {
                            return Err(final_attempt_err(
                                "create_container after pull-recovery",
                                MAX_PORT_RETRY_ATTEMPTS,
                                e,
                            ));
                        }
                    }
                }
                Err(ref e) if is_name_conflict_409(e) => {
                    warn!(container_name = %name, "create_container reports name in use; force-removing stale container and retrying once");
                    self.force_remove(name.as_str()).await;
                    match self
                        .client
                        .create_container(Some(options.clone()), docker_config)
                        .await
                    {
                        Ok(c) => c,
                        Err(e) if !final_attempt => {
                            warn!(
                                error = %e,
                                attempt,
                                max = MAX_PORT_RETRY_ATTEMPTS,
                                "create_container still failed after 409 force-remove; advancing outer port-retry"
                            );
                            continue;
                        }
                        Err(e) => {
                            return Err(final_attempt_err(
                                "create_container after 409 force-remove",
                                MAX_PORT_RETRY_ATTEMPTS,
                                e,
                            ));
                        }
                    }
                }
                Err(e) => return Err(runtime_err(e)),
            };

            match self
                .client
                .start_container::<String>(&created.id, None)
                .await
            {
                Ok(_) => {
                    let host_port = host_port_opt.unwrap_or(0);
                    info!(container_id = %created.id, sandbox_id = %sandbox_id_str, host_port, "container started");
                    return Ok(ContainerInfo {
                        id: ContainerId(created.id),
                        sandbox_id: config.sandbox_id,
                        host_port,
                        running: true,
                    });
                }
                Err(e)
                    if is_port_collision(&e) && host_port_opt.is_some() && !final_attempt =>
                {
                    warn!(
                        host_port = ?host_port_opt,
                        attempt,
                        max = MAX_PORT_RETRY_ATTEMPTS,
                        error = %e,
                        "port-bind collision; force-removing orphan and retrying with fresh port"
                    );
                    self.force_remove(&created.id).await;
                    // Small deterministic jitter (10–40ms range from
                    // the sandbox-id first byte) decorrelates retries
                    // against a sibling cycling the same ephemeral pool.
                    let jitter_ms = 10
                        + (sandbox_id_str.as_bytes().first().copied().unwrap_or(0) as u64 % 31);
                    tokio::time::sleep(Duration::from_millis(jitter_ms)).await;
                    continue;
                }
                Err(e) if is_port_collision(&e) && final_attempt => {
                    warn!(
                        host_port = ?host_port_opt,
                        attempt,
                        max = MAX_PORT_RETRY_ATTEMPTS,
                        error = %e,
                        "port-bind collision; giving up after final attempt"
                    );
                    self.force_remove(&created.id).await;
                    return Err(final_attempt_err(
                        "port-bind collision",
                        MAX_PORT_RETRY_ATTEMPTS,
                        e,
                    ));
                }
                Err(e) => {
                    self.force_remove(&created.id).await;
                    return Err(runtime_err(e));
                }
            }
        }
        // The for loop above always returns from one of its arms; this
        // is unreachable while MAX_PORT_RETRY_ATTEMPTS > 0 (it's a
        // const = 3).
        unreachable!("create_and_start retry loop always returns from inside")
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
        // Comp-4: the kill exec used to attach stdout/stderr and then
        // bind the resulting StartExecResults to `_`, leaking an
        // undrained bollard stream per kill. We don't need any output
        // — just fire and forget. Setting detach: true tells docker not
        // to attach in the first place, so there's nothing to drain.
        let exec = self
            .client
            .create_exec(
                &id.0,
                CreateExecOptions {
                    cmd: Some(cmd),
                    attach_stdout: Some(false),
                    attach_stderr: Some(false),
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
                    detach: true,
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

/// Ask the kernel for a free ephemeral TCP port by binding to :0,
/// reading the assigned port, and closing the socket. v1.0.2 (iter4):
/// this replaces the previous `publish_all_ports = true` flow that
/// required a follow-up `inspect_container` round-trip to discover
/// which port Docker assigned (~120ms on macOS Docker Desktop, ~5ms
/// on Linux native). Pre-allocating here lets us pass `port_bindings`
/// explicitly and skip the inspect entirely.
///
/// TOCTOU is inherent: another process can grab the port between
/// `drop(listener)` and the docker daemon's bind. The caller's
/// port-collision retry loop (iter5) handles that.
fn allocate_free_host_port() -> Result<u16, AgentError> {
    let listener =
        std::net::TcpListener::bind("0.0.0.0:0").map_err(|e| AgentError::Runtime {
            detail: format!("could not allocate ephemeral host port: {e}"),
        })?;
    let port = listener
        .local_addr()
        .map_err(|e| AgentError::Runtime {
            detail: format!("could not read ephemeral host port: {e}"),
        })?
        .port();
    drop(listener);
    Ok(port)
}

fn build_docker_config(
    config: &ContainerConfig,
    sandbox_id_str: &str,
    host_port: Option<u16>,
) -> Config<String> {
    let nano_cpus = (config.cpu_limit_millicores as i64) * NANOCPUS_PER_MILLICPU;

    // Explicit host-port binding for the exposed_port — skips the
    // post-start `inspect_container` round-trip. If exposed_port==0
    // we omit both `exposed_ports` and `port_bindings`; sandbox is
    // exec-only and has no host-side mapping.
    let (exposed_ports, port_bindings) = match (config.exposed_port, host_port) {
        (0, _) | (_, None) => (None, None),
        (container_port, Some(hp)) => {
            let port_key = format!("{container_port}/tcp");
            let binding = bollard::models::PortBinding {
                host_ip: Some("0.0.0.0".to_string()),
                host_port: Some(hp.to_string()),
            };
            (
                Some(HashMap::from([(port_key.clone(), HashMap::new())])),
                Some(HashMap::from([(port_key, Some(vec![binding]))])),
            )
        }
    };

    let host_config = HostConfig {
        memory: Some(config.memory_limit_bytes as i64),
        nano_cpus: Some(nano_cpus),
        port_bindings,
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

fn runtime_err(e: bollard::errors::Error) -> AgentError {
    AgentError::Runtime {
        detail: e.to_string(),
    }
}

/// True when a bollard error from `create_container` indicates a
/// name-conflict 409 — i.e., a container named `sandbox-<uuid>`
/// already exists on the host (typically a crashed prior agent or a
/// transient force_remove failure during an earlier port-retry
/// iteration). Docker's current message format is
/// `Conflict. The container name "/sandbox-<uuid>" is already in use
/// by container "..."`; we substring-match the stable "is already in
/// use" phrase. Extracted into a helper so future docker message
/// rewordings are caught by the dedicated tests rather than silently
/// disabling the 409 recovery arm.
fn is_name_conflict_409(e: &bollard::errors::Error) -> bool {
    if let bollard::errors::Error::DockerResponseServerError {
        status_code: 409,
        message,
    } = e
    {
        message.to_ascii_lowercase().contains("is already in use")
    } else {
        false
    }
}

/// True when a bollard error from `start_container` indicates the host
/// port we pre-allocated has been grabbed by another process between
/// our probe-and-release and docker's bind. Docker surfaces this as
/// `DockerResponseServerError { status_code: 500 }` with a
/// driver-specific message — we match the substrings that appear
/// across all current docker versions:
///   * libnetwork (default): "bind: address already in use"
///   * docker-proxy / userland fallback: "Error starting userland proxy: ... bind"
///   * portallocator: "port is already allocated"
fn is_port_collision(e: &bollard::errors::Error) -> bool {
    if let bollard::errors::Error::DockerResponseServerError {
        status_code: 500,
        message,
    } = e
    {
        let m = message.to_ascii_lowercase();
        m.contains("bind: address already in use")
            || m.contains("port is already allocated")
            || (m.contains("userland proxy") && m.contains("bind"))
    } else {
        false
    }
}

/// v1.0.2 (iter9): produce the uniform "exhausted after N attempts"
/// error that all three create_and_start final-attempt arms emit
/// (port-collision, 404 pull-recovery, 409 name-conflict). Without a
/// shared format, operators correlating log-scanner regexes against
/// different arms would miss one.
///
/// Format: `<kind> after <max> attempts: <bollard message>`.
fn final_attempt_err<E: std::fmt::Display>(kind: &str, max: u32, e: E) -> AgentError {
    AgentError::Runtime {
        detail: format!("{kind} after {max} attempts: {e}"),
    }
}

#[cfg(test)]
mod port_collision_tests {
    use super::*;

    fn err(message: &str) -> bollard::errors::Error {
        bollard::errors::Error::DockerResponseServerError {
            status_code: 500,
            message: message.to_string(),
        }
    }

    #[test]
    fn matches_libnetwork_bind_message() {
        assert!(is_port_collision(&err(
            "driver failed programming external connectivity on endpoint sandbox-abc: \
             Error starting userland proxy: listen tcp4 0.0.0.0:50123: bind: address already in use"
        )));
    }

    #[test]
    fn matches_portallocator_message() {
        assert!(is_port_collision(&err(
            "Bind for 0.0.0.0:50123 failed: port is already allocated"
        )));
    }

    #[test]
    fn ignores_unrelated_500() {
        assert!(!is_port_collision(&err("internal server error")));
        assert!(!is_port_collision(&err("cgroup mount failed")));
    }

    #[test]
    fn ignores_404_image_missing() {
        let e = bollard::errors::Error::DockerResponseServerError {
            status_code: 404,
            message: "no such image: foo:bar".into(),
        };
        assert!(!is_port_collision(&e));
    }
}

#[cfg(test)]
mod name_conflict_tests {
    use super::*;

    fn err(status_code: u16, message: &str) -> bollard::errors::Error {
        bollard::errors::Error::DockerResponseServerError {
            status_code,
            message: message.to_string(),
        }
    }

    #[test]
    fn matches_current_docker_message() {
        assert!(is_name_conflict_409(&err(
            409,
            r#"Conflict. The container name "/sandbox-abc123" is already in use by container "deadbeef"."#
        )));
    }

    #[test]
    fn matches_uppercase_variant() {
        assert!(is_name_conflict_409(&err(
            409,
            r#"CONFLICT. THE CONTAINER NAME "/SANDBOX-ABC123" IS ALREADY IN USE BY CONTAINER "DEADBEEF"."#
        )));
    }

    #[test]
    fn ignores_non_409_status() {
        assert!(!is_name_conflict_409(&err(
            500,
            r#"The container name "/sandbox-abc" is already in use"#
        )));
    }

    #[test]
    fn ignores_409_with_unrelated_message() {
        assert!(!is_name_conflict_409(&err(
            409,
            "Conflict. Volume \"shared\" is mounted by 2 containers"
        )));
    }

    #[test]
    fn ignores_non_docker_response_errors() {
        let e = bollard::errors::Error::JsonDataError {
            message: "is already in use".to_string(),
            column: 0,
        };
        assert!(!is_name_conflict_409(&e));
    }
}

#[cfg(test)]
mod final_attempt_err_tests {
    use super::*;

    #[test]
    fn uniform_format_across_kinds() {
        for kind in [
            "port-bind collision",
            "create_container after pull-recovery",
            "create_container after 409 force-remove",
        ] {
            let AgentError::Runtime { detail } =
                final_attempt_err(kind, 3, "underlying-bollard-error")
            else {
                panic!("expected Runtime variant");
            };
            assert_eq!(
                detail,
                format!("{kind} after 3 attempts: underlying-bollard-error"),
                "format drift would break operator log alerts"
            );
        }
    }

    #[test]
    fn embeds_bollard_display() {
        let bollard = bollard::errors::Error::DockerResponseServerError {
            status_code: 500,
            message: "bind: address already in use".into(),
        };
        let AgentError::Runtime { detail } = final_attempt_err("port-bind collision", 3, bollard)
        else {
            panic!("expected Runtime variant");
        };
        assert!(detail.contains("after 3 attempts:"), "got: {detail}");
        assert!(detail.contains("bind: address already in use"), "got: {detail}");
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
            pull_policy: open_sandbox_contracts::types::PullPolicy::IfNotPresent,
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
            pull_policy: open_sandbox_contracts::types::PullPolicy::IfNotPresent,
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
