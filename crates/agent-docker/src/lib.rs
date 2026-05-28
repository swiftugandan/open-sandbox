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
    ContainerConfig, ContainerId, ContainerInfo, ContainerRuntime, ContainerState, DirEntry,
    DirListing, EXEC_CHANNEL_CAPACITY, EntryType, ExecExitInfo, ExecHandle, ExecStart,
    FileRevision, consume_inpid_marker, detect_command_not_found, wrap_command_with_inpid_marker,
};
use open_sandbox_contracts::constants::LIST_DIR_MAX_ENTRIES;
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
                        state: ContainerState::Running,
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

    async fn pause(&self, id: &ContainerId) -> Result<(), AgentError> {
        match self.client.pause_container(&id.0).await {
            Ok(_) => Ok(()),
            // Docker returns 409 Conflict when the container is already
            // paused. Treat that as idempotent success rather than a
            // runtime error.
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 409, ..
            }) => Ok(()),
            // Cascade-fix #10: 404 means the container is gone (GC'd
            // out of band, agent restart with a stale id, etc.). Surface
            // as SandboxNotFound so the agent reports a non-terminal
            // state via its dispatch handler instead of writing Failed
            // to the DB.
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => Err(AgentError::SandboxNotFound {
                sandbox_id: id.0.clone(),
            }),
            Err(e) => Err(runtime_err(e)),
        }
    }

    async fn unpause(&self, id: &ContainerId) -> Result<(), AgentError> {
        match self.client.unpause_container(&id.0).await {
            Ok(_) => Ok(()),
            // 409 → already running; treat as idempotent success.
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 409, ..
            }) => Ok(()),
            // Cascade-fix #10: 404 → SandboxNotFound (see pause above).
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => Err(AgentError::SandboxNotFound {
                sandbox_id: id.0.clone(),
            }),
            Err(e) => Err(runtime_err(e)),
        }
    }

    async fn stop_and_remove(&self, id: &ContainerId, timeout: Duration) -> Result<(), AgentError> {
        // Cascade-fix #8: unpause before SIGTERM. Modern Docker
        // (≥19.03) unpauses automatically on `stop`, but older daemons
        // and some OCI-mode setups don't — SIGTERM gets queued in the
        // frozen cgroup until SIGKILL replaces it, inflating DELETE
        // latency by the full grace period. Best-effort: 409 / 404
        // (already running / container gone) are both fine.
        let _ = self.client.unpause_container(&id.0).await;
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

            // Docker's summary `state` is one of: created, restarting,
            // running, paused, exited, removing, dead. Map to the
            // tri-state used by reconcile — paused MUST NOT collapse
            // into Stopped or the agent's reconcile pass will overwrite
            // a legitimately frozen sandbox's state on every sweep.
            let state = match container.state.as_deref() {
                Some("running") => ContainerState::Running,
                Some("paused") => ContainerState::Paused,
                _ => ContainerState::Stopped,
            };

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
                state,
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

    async fn list_dir(
        &self,
        id: &ContainerId,
        path: &str,
        cwd: Option<&str>,
    ) -> Result<DirListing, AgentError> {
        let resolved = resolve_path(path, cwd);
        // Portable across GNU coreutils AND busybox (Alpine,
        // scratch-derived images). The previous shape used
        // `ls -lAn --time-style=+%s` which busybox `ls` rejects
        // with "unrecognized option: time-style" — see
        // FOLLOWUPS_v1.0.3.md #16. The shell loop below
        // sidesteps it by walking children directly + stat'ing
        // each. Output per line:
        //   OSB:<name>|<File type>|<size>|<mtime>|<mode>|<target>
        // The `OSB:` prefix lets us skip stderr-races on stdout
        // (we redirect stderr to /dev/null inside the loop but
        // belt-and-suspenders). `|` separator: filenames
        // containing `|` will have their target column
        // corrupted; an acceptable tradeoff for source-tree
        // workflows. Filenames with newlines will appear as two
        // entries; also rare enough to leave.
        //
        // `for f in * .[!.]* ..?*` is the POSIX-portable
        // dotfile-inclusive glob (excludes . and ..). When a
        // glob has no matches it expands to the literal pattern
        // — the `[ -e ] || [ -L ]` guard rejects those.
        const LIST_DIR_SCRIPT: &str = r#"
cd "$1" 2>/dev/null || exit 0
for f in * .[!.]* ..?*; do
  [ -e "$f" ] || [ -L "$f" ] || continue
  t=""
  if [ -L "$f" ]; then t="$(readlink "$f" 2>/dev/null || true)"; fi
  m="$(stat -c '%F|%s|%Y|%a' "$f" 2>/dev/null || true)"
  printf 'OSB:%s|%s|%s\n' "$f" "$m" "$t"
done
"#;
        let stdout = exec_collect_stdout(
            self,
            id,
            vec![
                "sh".into(),
                "-c".into(),
                LIST_DIR_SCRIPT.into(),
                "os-list-dir".into(),
                resolved.clone(),
            ],
        )
        .await
        .map_err(|e| match e {
            ListExecError::NotFound => AgentError::Runtime {
                detail: format!("No such file: {resolved}"),
            },
            ListExecError::Other(detail) => AgentError::Runtime { detail },
        })?;

        parse_portable_list_output(&stdout, &resolved)
    }

    async fn delete_file(
        &self,
        id: &ContainerId,
        path: &str,
        cwd: Option<&str>,
        recursive: bool,
    ) -> Result<(), AgentError> {
        let resolved = resolve_path(path, cwd);
        // `rm -rf` swallows non-existent paths (idempotent); `rm
        // -f` for the non-recursive case also swallows missing.
        // `rm -rf` would happily remove directories even when the
        // caller passed recursive=false — so we use `rm -f`
        // (matches the documented "errors on directories without
        // recursive") and `rm -rf` for recursive.
        let argv = if recursive {
            vec![
                "rm".into(),
                "-rf".into(),
                "--".into(),
                resolved.clone(),
            ]
        } else {
            vec!["rm".into(), "-f".into(), "--".into(), resolved.clone()]
        };
        let handle = self
            .start_exec(
                id,
                ExecStart {
                    command: argv,
                    cwd: String::new(),
                    env: HashMap::new(),
                },
            )
            .await?;
        drop(handle.stdin);
        // Drain both pipes; rm's success is silent, errors land
        // on stderr (`rm: cannot remove 'foo': Is a directory`).
        let mut stdout_rx = handle.stdout;
        let mut stderr_rx = handle.stderr;
        let stderr_task = async {
            let mut buf: Vec<u8> = Vec::new();
            while let Some(chunk) = stderr_rx.recv().await {
                if buf.len() < 4096 {
                    buf.extend_from_slice(&chunk);
                }
            }
            buf
        };
        let stdout_task = async { while stdout_rx.recv().await.is_some() {} };
        let (_, stderr) = tokio::join!(stdout_task, stderr_task);
        let info = handle.exited.await.map_err(|_| AgentError::Runtime {
            detail: "delete_file exec terminated without exit info".into(),
        })?;
        if info.exit_code == 0 {
            return Ok(());
        }
        let stderr_text = String::from_utf8_lossy(&stderr);
        Err(AgentError::Runtime {
            detail: format!(
                "rm exit={} stderr={}",
                info.exit_code,
                stderr_text.trim()
            ),
        })
    }

    async fn wait_port_listening(
        &self,
        id: &ContainerId,
        port: u32,
        timeout: Duration,
    ) -> Result<bool, AgentError> {
        // Probe inside the container's network namespace via
        // `docker exec sh -c 'nc -z ... in a poll loop'`. The host-
        // port-from-host probe is unreliable on Docker Desktop —
        // see the trait doc comment on wait_port_listening for
        // detail.
        //
        // The shell loop polls `nc -z 127.0.0.1 <port>` every 50ms
        // and exits 0 the moment a listener is detected, or exits
        // 1 when `iterations` polls have all failed. `nc -z` is
        // supported by both BusyBox and GNU netcat.
        //
        // 50ms cadence × (timeout_ms / 50) iterations bounds total
        // duration to ~timeout_ms; small overshoot from sleep
        // granularity is acceptable.
        let timeout_ms = timeout.as_millis().max(50) as u64;
        let iterations = (timeout_ms / 50).max(1);
        let script = format!(
            "i=0; while [ $i -lt {iterations} ]; do \
             nc -z 127.0.0.1 {port} 2>/dev/null && echo OSB:READY && exit 0; \
             i=$((i+1)); sleep 0.05; done; \
             echo OSB:TIMEOUT; exit 1"
        );
        let handle = self
            .start_exec(
                id,
                ExecStart {
                    command: vec!["sh".into(), "-c".into(), script],
                    cwd: String::new(),
                    env: HashMap::new(),
                },
            )
            .await?;
        drop(handle.stdin);
        // Drain stdout AND stderr concurrently. Serial drain
        // (stdout-then-stderr) risks a child stall: if the
        // script's `sh` emits to stderr (e.g., `sleep: not found`
        // on a stripped image) while stdout is idle, the stderr
        // pipe can fill and block the child — at which point it
        // never gets to write the OSB:READY/TIMEOUT sentinel.
        // Caps stay (cheap defense in depth; the sentinels are
        // ~10 bytes), but they're per-stream so an oversize
        // stderr from one stream can't preempt the other.
        let mut stdout_rx = handle.stdout;
        let mut stderr_rx = handle.stderr;
        let stdout_task = async {
            let mut buf: Vec<u8> = Vec::new();
            while let Some(chunk) = stdout_rx.recv().await {
                if buf.len().saturating_add(chunk.len()) > 4096 {
                    // Don't break — keep draining so the child
                    // doesn't stall on a full pipe. Just stop
                    // remembering bytes past the cap; sentinel
                    // landed earlier than this anyway.
                    continue;
                }
                buf.extend_from_slice(&chunk);
            }
            buf
        };
        let stderr_task = async {
            let mut buf: Vec<u8> = Vec::new();
            while let Some(chunk) = stderr_rx.recv().await {
                if buf.len().saturating_add(chunk.len()) > 4096 {
                    continue;
                }
                buf.extend_from_slice(&chunk);
            }
            buf
        };
        let (stdout, stderr) = tokio::join!(stdout_task, stderr_task);
        let info = handle.exited.await.map_err(|_| AgentError::Runtime {
            detail: "wait_port_listening probe terminated without exit info".into(),
        })?;
        let stdout_text = String::from_utf8_lossy(&stdout);
        if stdout_text.contains("OSB:READY") {
            return Ok(true);
        }
        if stdout_text.contains("OSB:TIMEOUT") {
            return Ok(false);
        }
        // Exec failed before the script could print either
        // sentinel. `command_not_found` is the typed signal from
        // start_exec when the EXEC itself couldn't be spawned
        // (image lacks `sh`); a substring search for `not found`
        // on stderr is too broad — any `sh: foo: not found` from
        // an unrelated command in the script would mis-route the
        // error. Prefer the typed signal; surface the raw exit
        // code + stderr otherwise so the operator has the actual
        // failure mode in the message.
        let stderr_text = String::from_utf8_lossy(&stderr);
        if info.command_not_found {
            return Err(AgentError::Runtime {
                detail: "wait_port_listening requires `sh` and `nc` in the container image".into(),
            });
        }
        // Heuristic: if stderr explicitly mentions `nc`, the
        // image has sh+sleep but lacks nc — common on scratch /
        // distroless images that nonetheless have busybox sh.
        if stderr_text.contains("nc:") || stderr_text.contains("nc not found") {
            return Err(AgentError::Runtime {
                detail: "wait_port_listening requires `nc` in the container image".into(),
            });
        }
        Err(AgentError::Runtime {
            detail: format!(
                "wait_port_listening probe exit={} stderr={}",
                info.exit_code,
                stderr_text.trim()
            ),
        })
    }

    async fn stat_revision(
        &self,
        id: &ContainerId,
        path: &str,
        cwd: Option<&str>,
    ) -> Result<FileRevision, AgentError> {
        let resolved = resolve_path(path, cwd);
        // `stat -c "%Y %s"` is portable across GNU coreutils and
        // busybox. Output: "<mtime_secs> <size>". Trade-off: 1s
        // mtime resolution collapses sub-second edits to the same
        // revision; acceptable for the UI's optimistic-write
        // contract because the matching FileMeta from the prior
        // read carries the same shape.
        let stdout = exec_collect_stdout(
            self,
            id,
            vec![
                "stat".into(),
                "-c".into(),
                "%Y %s".into(),
                "--".into(),
                resolved.clone(),
            ],
        )
        .await
        .map_err(|e| match e {
            ListExecError::NotFound => AgentError::Runtime {
                detail: format!("No such file: {resolved}"),
            },
            ListExecError::Other(detail) => AgentError::Runtime { detail },
        })?;

        let text = String::from_utf8_lossy(&stdout);
        let trimmed = text.trim();
        let mut parts = trimmed.split_whitespace();
        let mtime: u64 = parts
            .next()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| AgentError::Runtime {
                detail: format!("stat output malformed: {trimmed:?}"),
            })?;
        let size: u64 = parts
            .next()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| AgentError::Runtime {
                detail: format!("stat output malformed: {trimmed:?}"),
            })?;
        Ok(FileRevision {
            revision: format!("{mtime}:{size}"),
            size,
        })
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

/// Distinct error variant so `list_dir` / `stat_revision` can map
/// "missing path" to the agent-level `No such file: ...` detail
/// (which `drive_list_dir` / `drive_read_file` route to
/// `FILE_NOT_FOUND`) without parsing the bollard error string.
enum ListExecError {
    NotFound,
    Other(String),
}

/// Run a one-shot exec inside the container, collect stdout fully,
/// and require exit_code = 0. Non-zero exit with stderr that smells
/// like "no such file" (busybox / coreutils both phrase it that way)
/// surfaces as `ListExecError::NotFound`; anything else surfaces as
/// `ListExecError::Other(detail)`.
async fn exec_collect_stdout(
    runtime: &DockerRuntime,
    id: &ContainerId,
    command: Vec<String>,
) -> Result<Vec<u8>, ListExecError> {
    let handle = runtime
        .start_exec(
            id,
            ExecStart {
                command,
                cwd: String::new(),
                env: HashMap::new(),
            },
        )
        .await
        .map_err(|e| ListExecError::Other(e.to_string()))?;

    let mut handle = handle;
    drop(handle.stdin);
    // Bound the in-memory buffer so a directory with millions of
    // entries can't OOM the agent before parse_ls_lan_output gets a
    // chance to apply the LIST_DIR_MAX_ENTRIES cap. Sized at
    // `LIST_DIR_MAX_ENTRIES * MAX_ENTRY_LINE_BYTES` so we have
    // comfortable headroom over the realistic per-entry size
    // (mode+links+uid+gid+size+mtime+name+symlink-target ≈
    // 50–200 bytes/line).
    const MAX_ENTRY_LINE_BYTES: usize = 512;
    let max_stdout_bytes = LIST_DIR_MAX_ENTRIES * MAX_ENTRY_LINE_BYTES;
    let mut stdout: Vec<u8> = Vec::new();
    while let Some(chunk) = handle.stdout.recv().await {
        if stdout.len().saturating_add(chunk.len()) > max_stdout_bytes {
            return Err(ListExecError::Other(format!(
                "exec stdout exceeded {max_stdout_bytes}-byte cap"
            )));
        }
        stdout.extend_from_slice(&chunk);
    }
    let mut stderr: Vec<u8> = Vec::new();
    while let Some(chunk) = handle.stderr.recv().await {
        // Same cap shape on stderr — busybox can emit a per-entry
        // warning that doesn't gate the success path but still
        // burns memory if uncapped. Use a smaller cap (64KiB).
        const MAX_STDERR_BYTES: usize = 64 * 1024;
        if stderr.len().saturating_add(chunk.len()) > MAX_STDERR_BYTES {
            break;
        }
        stderr.extend_from_slice(&chunk);
    }
    let info = handle.exited.await.map_err(|_| {
        ListExecError::Other("internal exec terminated without exit info".into())
    })?;

    if info.exit_code == 0 {
        Ok(stdout)
    } else {
        // Tightened from a looser `contains("not found")` —
        // see the v2 code-review pass. "command not found" /
        // "executable file not found in $PATH" (the OCI runtime's
        // missing-binary diagnostic) MUST NOT be folded into
        // FILE_NOT_FOUND, or callers would treat a missing `ls`
        // binary on a scratch image as a missing directory.
        let stderr_text = String::from_utf8_lossy(&stderr);
        if stderr_text.contains("No such file") || stderr_text.contains("cannot access") {
            Err(ListExecError::NotFound)
        } else {
            Err(ListExecError::Other(format!(
                "exit={} stderr={}",
                info.exit_code,
                stderr_text.trim()
            )))
        }
    }
}

/// Parse the output of the portable shell-loop list_dir script.
///
/// Each entry line has the shape:
///   `OSB:<name>|<File-type>|<size>|<mtime>|<mode>|<target>`
///
/// The `OSB:` prefix lets us discard stderr-races on stdout. The
/// `<File-type>` column is the `stat -c '%F'` description — one of
/// "regular file" / "regular empty file" / "directory" / "symbolic
/// link" / "character special file" / "block special file" /
/// "fifo" / "socket". We collapse the long tail to `EntryType::Other`.
/// `<mode>` is `stat -c '%a'` which both GNU and busybox emit as
/// 3- or 4-digit octal; we left-pad to 4 chars so the wire shape
/// matches the youki impl's `{:04o}`.
fn parse_portable_list_output(stdout: &[u8], dir_path: &str) -> Result<DirListing, AgentError> {
    let text = String::from_utf8_lossy(stdout);
    let mut entries: Vec<DirEntry> = Vec::new();
    let mut total_entries: u64 = 0;

    for raw_line in text.lines() {
        let line = match raw_line.strip_prefix("OSB:") {
            Some(s) => s,
            // Non-entry stdout (warning, banner, stray noise) —
            // skip rather than fail the listing.
            None => continue,
        };
        // splitn(6, '|'): name | type | size | mtime | mode | target.
        // Filenames containing '|' will see only the first column
        // captured cleanly — the documented tradeoff.
        let mut cols = line.splitn(6, '|');
        let name = match cols.next() {
            Some(n) if !n.is_empty() => n,
            _ => continue,
        };
        let file_type_str = cols.next().unwrap_or("");
        let size_str = cols.next().unwrap_or("0");
        let mtime_str = cols.next().unwrap_or("0");
        let mode_str = cols.next().unwrap_or("");
        let target = cols.next().unwrap_or("");

        let entry_type = match file_type_str {
            "regular file" | "regular empty file" => EntryType::File,
            "directory" => EntryType::Dir,
            "symbolic link" => EntryType::Symlink,
            _ => EntryType::Other,
        };
        let size: u64 = size_str.parse().unwrap_or(0);
        let mtime: u64 = mtime_str.parse().unwrap_or(0);
        // stat -c '%a' produces 3 or 4 octal digits depending on
        // whether high-nibble (setuid/setgid/sticky) bits are set.
        // Normalize to 4 chars so cross-runtime mode strings match
        // youki's `format!("{:04o}", perms & 0o7777)`.
        let mode = if mode_str.is_empty() {
            String::new()
        } else if mode_str.len() < 4 {
            format!("{mode_str:0>4}")
        } else {
            mode_str.to_string()
        };

        total_entries += 1;
        if entries.len() < LIST_DIR_MAX_ENTRIES {
            entries.push(DirEntry {
                name: name.to_string(),
                entry_type,
                size,
                revision: format!("{mtime}:{size}"),
                mode,
                target: if entry_type == EntryType::Symlink {
                    target.to_string()
                } else {
                    String::new()
                },
            });
        }
    }

    let truncated = total_entries as usize > entries.len();
    Ok(DirListing {
        path: dir_path.to_string(),
        entries,
        truncated,
        total_entries,
    })
}

/// Legacy `ls -lAn --time-style=+%s -q -- <path>` parser. Kept for
/// the parser_tests module + a possible future GNU-only fast path
/// (the shell-loop above is ~2× slower than a single ls call on
/// large directories). Today nothing calls it; flagged `#[allow]`
/// so cargo doesn't warn.
#[allow(dead_code)]
fn parse_ls_lan_output(stdout: &[u8], dir_path: &str) -> Result<DirListing, AgentError> {
    let text = String::from_utf8_lossy(stdout);
    let mut all_entries: Vec<DirEntry> = Vec::new();
    let mut total_entries: u64 = 0;

    for raw_line in text.lines() {
        let line = raw_line.trim_end();
        if line.is_empty() {
            continue;
        }
        // `ls -l` emits a `total <n>` header line for directories.
        if line.starts_with("total ") {
            continue;
        }
        let Some(entry) = parse_ls_entry_line(line) else {
            // Skip lines we don't understand rather than fail the
            // whole listing — `ls` can emit extra warnings on stderr
            // that occasionally race onto stdout in tight CI loops.
            continue;
        };
        total_entries += 1;
        if all_entries.len() < LIST_DIR_MAX_ENTRIES {
            all_entries.push(entry);
        }
    }

    let truncated = total_entries as usize > all_entries.len();
    Ok(DirListing {
        path: dir_path.to_string(),
        entries: all_entries,
        truncated,
        total_entries,
    })
}

/// Parse a single `ls -lAn` line. Returns None on malformed input.
#[allow(dead_code)]
fn parse_ls_entry_line(line: &str) -> Option<DirEntry> {
    // Real-world `ls -lAn` output right-justifies numeric columns
    // (links, uid, gid, size) — runs of spaces appear between them
    // whenever column widths vary. Walk the line manually: skip
    // leading whitespace, take six whitespace-delimited tokens,
    // then capture everything after the sixth column's trailing
    // whitespace as the name field (which may itself contain
    // spaces, OR be `<name> -> <target>` for symlinks).
    //
    // A naive `split_whitespace` + `line.find(token)` recovery is
    // broken for filenames that match an earlier column (e.g. a
    // file literally named "1000" matching the uid column).
    let mut cursor = 0usize;
    let bytes = line.as_bytes();
    let take_token = |cursor: &mut usize| -> Option<&str> {
        // Skip leading whitespace.
        while *cursor < bytes.len() && bytes[*cursor].is_ascii_whitespace() {
            *cursor += 1;
        }
        let start = *cursor;
        while *cursor < bytes.len() && !bytes[*cursor].is_ascii_whitespace() {
            *cursor += 1;
        }
        if start == *cursor {
            None
        } else {
            Some(&line[start..*cursor])
        }
    };
    let mode_str = take_token(&mut cursor)?;
    let _links = take_token(&mut cursor)?;
    let _uid = take_token(&mut cursor)?;
    let _gid = take_token(&mut cursor)?;
    let size_str = take_token(&mut cursor)?;
    let mtime_str = take_token(&mut cursor)?;
    // Skip one run of whitespace and use the rest as the name field.
    while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
        cursor += 1;
    }
    if cursor >= bytes.len() {
        return None;
    }
    let name_field = &line[cursor..];

    let kind_char = mode_str.chars().next()?;
    let entry_type = match kind_char {
        '-' => EntryType::File,
        'd' => EntryType::Dir,
        'l' => EntryType::Symlink,
        _ => EntryType::Other,
    };

    // For symlinks the name field is `<name> -> <target>`. The
    // ` -> ` separator is unambiguous because POSIX ls is required
    // to emit it for symlinks.
    let (name, target) = if entry_type == EntryType::Symlink {
        match name_field.split_once(" -> ") {
            Some((n, t)) => (n.to_string(), t.to_string()),
            None => (name_field.to_string(), String::new()),
        }
    } else {
        (name_field.to_string(), String::new())
    };

    // Mode bits as octal — see mode_string_to_octal for the
    // setuid/setgid/sticky-bit handling.
    let mode = mode_string_to_octal(mode_str).unwrap_or_else(|| mode_str.to_string());

    let size: u64 = size_str.parse().unwrap_or(0);
    let mtime: u64 = mtime_str.parse().unwrap_or(0);
    let revision = format!("{mtime}:{size}");

    Some(DirEntry {
        name,
        entry_type,
        size,
        revision,
        mode,
        target,
    })
}

/// Convert an `ls -l`-style permission string like "drwxr-xr-x" or
/// "-rwsr-xr-t" to a 4-character octal mode string. The first
/// digit captures setuid (4), setgid (2), and sticky (1) bits; the
/// trailing three digits are the per-class rwx triplets. Returns
/// None on malformed input. Used by the legacy ls-based parser
/// kept for parser_tests; the live path uses `stat -c '%a'` which
/// emits the octal directly.
#[allow(dead_code)]
fn mode_string_to_octal(mode_str: &str) -> Option<String> {
    if mode_str.len() < 10 {
        return None;
    }
    let bytes = mode_str.as_bytes();
    let owner = mode_triplet(&bytes[1..4])?;
    let group = mode_triplet(&bytes[4..7])?;
    let other = mode_triplet(&bytes[7..10])?;
    // High nibble — setuid (4) from owner's execute slot, setgid
    // (2) from group's, sticky (1) from other's. ls signals each
    // via the corresponding letter in the execute position:
    //   's' / 'S' on owner → setuid (with / without execute)
    //   's' / 'S' on group → setgid
    //   't' / 'T' on other → sticky
    let mut high = 0u8;
    if matches!(bytes[3], b's' | b'S') {
        high |= 4;
    }
    if matches!(bytes[6], b's' | b'S') {
        high |= 2;
    }
    if matches!(bytes[9], b't' | b'T') {
        high |= 1;
    }
    Some(format!("{high}{owner}{group}{other}"))
}

#[allow(dead_code)]
fn mode_triplet(bytes: &[u8]) -> Option<u8> {
    if bytes.len() < 3 {
        return None;
    }
    let r = if bytes[0] == b'r' { 4 } else { 0 };
    let w = if bytes[1] == b'w' { 2 } else { 0 };
    let x = match bytes[2] {
        b'x' | b's' | b't' => 1,
        _ => 0,
    };
    Some(r + w + x)
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
mod parser_tests {
    //! Pure-parser tests for the ls / stat output parsers used by
    //! list_dir / stat_revision. No Docker daemon required.

    use super::*;

    #[test]
    fn mode_triplet_handles_basic_combinations() {
        assert_eq!(mode_triplet(b"rwx").unwrap(), 7);
        assert_eq!(mode_triplet(b"rw-").unwrap(), 6);
        assert_eq!(mode_triplet(b"r-x").unwrap(), 5);
        assert_eq!(mode_triplet(b"r--").unwrap(), 4);
        assert_eq!(mode_triplet(b"---").unwrap(), 0);
        // setuid / sticky bits map to executable bit being set.
        assert_eq!(mode_triplet(b"rws").unwrap(), 7);
        assert_eq!(mode_triplet(b"rwt").unwrap(), 7);
    }

    #[test]
    fn mode_string_to_octal_basic() {
        assert_eq!(mode_string_to_octal("-rw-r--r--").as_deref(), Some("0644"));
        assert_eq!(mode_string_to_octal("drwxr-xr-x").as_deref(), Some("0755"));
        assert_eq!(mode_string_to_octal("lrwxrwxrwx").as_deref(), Some("0777"));
        assert_eq!(mode_string_to_octal("----------").as_deref(), Some("0000"));
    }

    #[test]
    fn mode_string_to_octal_preserves_setuid_setgid_sticky() {
        // setuid binary with execute (s).
        assert_eq!(mode_string_to_octal("-rwsr-xr-x").as_deref(), Some("4755"));
        // setuid no execute (S) — bit set, owner execute clear.
        assert_eq!(mode_string_to_octal("-rwSr-xr-x").as_deref(), Some("4655"));
        // setgid binary with execute.
        assert_eq!(mode_string_to_octal("-rwxr-sr-x").as_deref(), Some("2755"));
        // setgid no execute.
        assert_eq!(mode_string_to_octal("-rwxr-Sr-x").as_deref(), Some("2745"));
        // sticky dir with execute (t).
        assert_eq!(mode_string_to_octal("drwxr-xr-t").as_deref(), Some("1755"));
        // sticky no execute (T).
        assert_eq!(mode_string_to_octal("drwxr-xr-T").as_deref(), Some("1754"));
        // setuid + setgid + sticky together.
        assert_eq!(mode_string_to_octal("-rwsr-sr-t").as_deref(), Some("7755"));
    }

    #[test]
    fn mode_string_to_octal_rejects_short_input() {
        assert!(mode_string_to_octal("-rwx").is_none());
    }

    #[test]
    fn parse_ls_entry_line_file() {
        // Format: <mode> <links> <uid> <gid> <size> <mtime> <name>
        let line = "-rw-r--r-- 1 1000 1000 421 1716800200 README.md";
        let e = parse_ls_entry_line(line).expect("parses");
        assert_eq!(e.name, "README.md");
        assert_eq!(e.entry_type, EntryType::File);
        assert_eq!(e.size, 421);
        assert_eq!(e.revision, "1716800200:421");
        assert_eq!(e.mode, "0644");
        assert_eq!(e.target, "");
    }

    #[test]
    fn parse_ls_entry_line_handles_right_aligned_columns() {
        // Real ls right-justifies numeric columns: when sibling
        // files have widely varying sizes ls pads the narrower
        // ones with leading spaces. The parser must collapse runs
        // of whitespace between columns.
        let line = "-rw-r--r--   1 1000 1000  421 1716800200 README.md";
        let e = parse_ls_entry_line(line).expect("right-aligned line parses");
        assert_eq!(e.name, "README.md");
        assert_eq!(e.size, 421);
        assert_eq!(e.revision, "1716800200:421");
    }

    #[test]
    fn parse_ls_entry_line_preserves_filename_matching_earlier_column() {
        // A file literally named "1000" must not collide with the
        // uid/gid columns when the parser recovers the name field.
        // The cursor-based extraction (not substring-match) is the
        // fix; this regression test pins the corner case.
        let line = "-rw-r--r-- 1 1000 1000 4 1716800200 1000";
        let e = parse_ls_entry_line(line).expect("parses");
        assert_eq!(e.name, "1000");
        assert_eq!(e.size, 4);
        assert_eq!(e.revision, "1716800200:4");
    }

    #[test]
    fn parse_ls_entry_line_preserves_filename_with_spaces() {
        let line = "-rw-r--r-- 1 1000 1000 7 1716800200 hello world.txt";
        let e = parse_ls_entry_line(line).expect("parses");
        assert_eq!(e.name, "hello world.txt");
    }

    #[test]
    fn parse_ls_entry_line_dir() {
        let line = "drwxr-xr-x 2 1000 1000 0 1716800123 src";
        let e = parse_ls_entry_line(line).expect("parses");
        assert_eq!(e.name, "src");
        assert_eq!(e.entry_type, EntryType::Dir);
        assert_eq!(e.mode, "0755");
    }

    #[test]
    fn parse_ls_entry_line_symlink_extracts_target() {
        let line = "lrwxrwxrwx 1 0 0 16 1716800100 logs -> /var/log";
        let e = parse_ls_entry_line(line).expect("parses");
        assert_eq!(e.name, "logs");
        assert_eq!(e.entry_type, EntryType::Symlink);
        assert_eq!(e.target, "/var/log");
        assert_eq!(e.mode, "0777");
    }

    #[test]
    fn parse_ls_entry_line_other_kinds_collapse_to_other() {
        // FIFO / socket / device — first char is p / s / b / c.
        for prefix in ["prw-rw-rw-", "srw-rw-rw-", "brw-r--r--", "crw-r--r--"] {
            let line = format!("{prefix} 1 0 0 0 1716800100 weird");
            let e = parse_ls_entry_line(&line).expect("parses");
            assert_eq!(e.entry_type, EntryType::Other, "for prefix {prefix:?}");
        }
    }

    #[test]
    fn parse_portable_list_output_basic() {
        let stdout = b"\
OSB:README.md|regular file|421|1716800200|644|
OSB:src|directory|0|1716800123|755|
OSB:logs|symbolic link|16|1716800100|777|/var/log
OSB:empty.txt|regular empty file|0|1716800000|644|
OSB:socket|socket|0|1716800000|755|
";
        let l = parse_portable_list_output(stdout, "/workspace").unwrap();
        assert_eq!(l.entries.len(), 5);
        assert_eq!(l.total_entries, 5);
        assert!(!l.truncated);
        assert_eq!(l.entries[0].name, "README.md");
        assert_eq!(l.entries[0].entry_type, EntryType::File);
        assert_eq!(l.entries[0].size, 421);
        assert_eq!(l.entries[0].revision, "1716800200:421");
        assert_eq!(l.entries[0].mode, "0644");
        assert_eq!(l.entries[0].target, "");

        assert_eq!(l.entries[1].entry_type, EntryType::Dir);
        assert_eq!(l.entries[1].mode, "0755");

        assert_eq!(l.entries[2].entry_type, EntryType::Symlink);
        assert_eq!(l.entries[2].target, "/var/log");

        // "regular empty file" is a stat -c '%F' synonym for an
        // empty regular file — must still classify as File.
        assert_eq!(l.entries[3].entry_type, EntryType::File);

        // FIFO / socket / device → Other.
        assert_eq!(l.entries[4].entry_type, EntryType::Other);
    }

    #[test]
    fn parse_portable_list_output_handles_4_digit_mode() {
        // stat -c '%a' on a setuid binary emits "4755" (no extra
        // padding needed). Verify we don't pre-pad those.
        let stdout = b"OSB:passwd|regular file|54080|1716800200|4755|\n";
        let l = parse_portable_list_output(stdout, "/usr/bin").unwrap();
        assert_eq!(l.entries[0].mode, "4755");
    }

    #[test]
    fn parse_portable_list_output_skips_non_osb_prefix_lines() {
        // Stderr races / banners get filtered.
        let stdout = b"\
sh: line 2: cd: /missing: No such file or directory
OSB:file.txt|regular file|10|1716800000|644|
";
        let l = parse_portable_list_output(stdout, "/workspace").unwrap();
        assert_eq!(l.entries.len(), 1);
        assert_eq!(l.entries[0].name, "file.txt");
    }

    #[test]
    fn parse_portable_list_output_caps_at_5000_entries() {
        let mut output = String::new();
        let overflow = LIST_DIR_MAX_ENTRIES + 5;
        for i in 0..overflow {
            output.push_str(&format!(
                "OSB:f{i:05}.txt|regular file|7|1716800200|644|\n"
            ));
        }
        let l = parse_portable_list_output(output.as_bytes(), "/big").unwrap();
        assert_eq!(l.entries.len(), LIST_DIR_MAX_ENTRIES);
        assert_eq!(l.total_entries as usize, overflow);
        assert!(l.truncated);
    }

    #[test]
    fn parse_ls_lan_output_skips_total_line_and_caps_entries() {
        let mut output = String::from("total 12\n");
        // Generate more than LIST_DIR_MAX_ENTRIES entries so the
        // truncation path engages.
        let overflow = LIST_DIR_MAX_ENTRIES + 3;
        for i in 0..overflow {
            output.push_str(&format!(
                "-rw-r--r-- 1 0 0 7 1716800200 f{i:05}.txt\n"
            ));
        }
        let listing = parse_ls_lan_output(output.as_bytes(), "/workspace").unwrap();
        assert_eq!(listing.path, "/workspace");
        assert_eq!(listing.entries.len(), LIST_DIR_MAX_ENTRIES);
        assert_eq!(listing.total_entries as usize, overflow);
        assert!(listing.truncated);
    }

    #[test]
    fn parse_ls_lan_output_under_cap_keeps_all_entries() {
        let output = "\
total 4
-rw-r--r-- 1 1000 1000 421 1716800200 README.md
drwxr-xr-x 2 1000 1000 0 1716800123 src
lrwxrwxrwx 1 0 0 16 1716800100 logs -> /var/log
";
        let listing = parse_ls_lan_output(output.as_bytes(), "/workspace").unwrap();
        assert_eq!(listing.entries.len(), 3);
        assert_eq!(listing.total_entries, 3);
        assert!(!listing.truncated);
        assert_eq!(listing.entries[0].name, "README.md");
        assert_eq!(listing.entries[1].name, "src");
        assert_eq!(listing.entries[2].name, "logs");
        assert_eq!(listing.entries[2].target, "/var/log");
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
        assert_eq!(info.state, ContainerState::Running);
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
