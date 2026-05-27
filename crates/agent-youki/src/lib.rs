pub mod cni;
pub mod dns;
pub mod exec;
pub mod image;
pub mod setns_ops;
pub mod spec;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use bytes::Bytes;
use open_sandbox_agent::container::{
    ContainerConfig, ContainerId, ContainerInfo, ContainerRuntime, ContainerState, ExecHandle,
    ExecStart,
};
use open_sandbox_contracts::constants::DEFAULT_WRITE_CWD;
use open_sandbox_contracts::error::AgentError;
use open_sandbox_contracts::types::SandboxId;

const CONTAINER_ID_PREFIX: &str = "osb";
const CGROUP_PATH_PREFIX: &str = "/open-sandbox";
const NETWORK_NAME: &str = "open-sandbox";
const STOP_GRACE_PERIOD: Duration = Duration::from_secs(10);

use libcontainer::container::Container;
use libcontainer::container::builder::ContainerBuilder;
use libcontainer::syscall::syscall::SyscallType;

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

struct ContainerMetadata {
    sandbox_id: SandboxId,
    host_port: u16,
    container_dir: PathBuf,
    netns_path: String,
}

pub struct YoukiRuntime {
    config: YoukiConfig,
    image_manager: image::ImageManager,
    containers: Mutex<HashMap<String, ContainerMetadata>>,
}

impl YoukiRuntime {
    pub fn new(config: YoukiConfig) -> Result<Self, AgentError> {
        for subdir in ["state", "images", "containers"] {
            std::fs::create_dir_all(config.root_dir.join(subdir)).map_err(|e| {
                AgentError::Runtime {
                    detail: format!("failed to create {subdir} directory: {e}"),
                }
            })?;
        }

        let image_manager = image::ImageManager::new(config.root_dir.clone());

        Ok(Self {
            config,
            image_manager,
            containers: Mutex::new(HashMap::new()),
        })
    }

    fn state_dir(&self) -> PathBuf {
        self.config.root_dir.join("state")
    }

    fn lock_containers(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, HashMap<String, ContainerMetadata>>, AgentError> {
        self.containers.lock().map_err(|_| AgentError::Runtime {
            detail: "container metadata lock poisoned".into(),
        })
    }

    /// Comp-5: best-effort rollback of a half-created libcontainer
    /// container. Removes the state_dir entry + bundle on disk; errors
    /// are warn-logged but never propagated (caller is already returning
    /// the original failure).
    async fn rollback_libcontainer(&self, container_id: &str, container_dir: &std::path::Path) {
        let state_root = self.state_dir().join(container_id);
        if state_root.exists() {
            if let Err(e) = tokio::fs::remove_dir_all(&state_root).await {
                tracing::warn!(container_id, error = %e, "rollback: remove state_dir failed");
            }
        }
        if container_dir.exists() {
            if let Err(e) = tokio::fs::remove_dir_all(container_dir).await {
                tracing::warn!(container_id, error = %e, "rollback: remove bundle failed");
            }
        }
    }

    /// Comp-5: best-effort rollback of a CNI ADD. Tears down the
    /// network namespace + releases any allocated IP. Errors are
    /// warn-logged.
    async fn rollback_cni(
        &self,
        conflist: &cni::CniConfList,
        container_id: &str,
        netns_path: &str,
    ) {
        if let Err(e) = cni::invoke_cni(
            conflist,
            "DEL",
            container_id,
            netns_path,
            &self.config.cni_bin_path,
        )
        .await
        {
            tracing::warn!(container_id, error = %e, "rollback: CNI DEL failed");
        }
    }
}

impl ContainerRuntime for YoukiRuntime {
    async fn create_and_start(&self, config: ContainerConfig) -> Result<ContainerInfo, AgentError> {
        // v1.0.2: honor pull_policy. The image_manager's `.complete`
        // marker gives us IfNotPresent semantics (skip layer fetch when
        // the digest dir exists); iter12 added a `force` flag for the
        // Always path so floating tags actually refresh.
        //   - `IfNotPresent` / default: pull_and_unpack_with(force=false)
        //     — manifest+config still fetched (we need the digest to
        //     locate the cache), but layer extraction short-circuits
        //     on the `.complete` marker.
        //   - `Always`: pull_and_unpack_with(force=true) — removes the
        //     marker and re-extracts. Iter12 closes the long-standing
        //     "Always silently degrades to IfNotPresent" gap.
        //   - `Never`: skip the registry round-trip entirely and fail
        //     if no local digest matches. youki's pull_and_unpack
        //     always probes the manifest, which is a registry hit;
        //     until a tag→digest local index lands we return Runtime
        //     with a clear message.
        use open_sandbox_contracts::types::PullPolicy;
        if matches!(config.pull_policy, PullPolicy::Never) {
            return Err(AgentError::Runtime {
                detail: "pull_policy=never is not yet supported by the youki runtime: youki's image_manager always probes the manifest before deciding to extract, so it cannot satisfy the no-registry-contact contract Never requires. A future iteration adds a local tag→digest index that lets Never resolve without a manifest probe. Until then, set IfNotPresent or Always.".into(),
            });
        }
        let force = matches!(config.pull_policy, PullPolicy::Always);
        let rootfs = self
            .image_manager
            .pull_and_unpack_with(&config.image, force)
            .await?;
        dns::write_resolv_conf(&rootfs).await?;

        let host_port = cni::allocate_port()?;
        let container_id = format!("{CONTAINER_ID_PREFIX}-{}", uuid::Uuid::new_v4());

        let container_dir = self.config.root_dir.join("containers").join(&container_id);
        let bundle_dir = container_dir.join("bundle");
        tokio::fs::create_dir_all(&bundle_dir)
            .await
            .map_err(|e| AgentError::Runtime {
                detail: format!("failed to create bundle directory: {e}"),
            })?;

        let cgroup_path = format!("{CGROUP_PATH_PREFIX}/{container_id}");
        let rootfs_str = rootfs.to_string_lossy().to_string();
        let oci_spec = spec::generate_full_spec(&config, &rootfs_str, Some(&cgroup_path))?;
        let spec_json = serde_json::to_vec_pretty(&oci_spec).map_err(|e| AgentError::Runtime {
            detail: format!("failed to serialize OCI spec: {e}"),
        })?;
        tokio::fs::write(bundle_dir.join("config.json"), &spec_json)
            .await
            .map_err(|e| AgentError::Runtime {
                detail: format!("failed to write config.json: {e}"),
            })?;

        let state_dir = self.state_dir();
        let bundle_dir_c = bundle_dir.clone();
        let cid_c = container_id.clone();
        let init_pid = tokio::task::spawn_blocking(move || -> Result<i32, AgentError> {
            let container = ContainerBuilder::new(cid_c, SyscallType::Linux)
                .with_root_path(&state_dir)
                .map_err(|e| AgentError::Runtime {
                    detail: format!("invalid state directory: {e}"),
                })?
                .as_init(&bundle_dir_c)
                .with_systemd(false)
                .build()
                .map_err(|e| AgentError::Runtime {
                    detail: format!("failed to create container: {e}"),
                })?;

            container
                .pid()
                .map(|p| p.as_raw())
                .ok_or_else(|| AgentError::Runtime {
                    detail: "container has no init PID after creation".into(),
                })
        })
        .await
        .map_err(|e| AgentError::Runtime {
            detail: format!("container creation task panicked: {e}"),
        })??;

        let netns_path = format!("/proc/{init_pid}/ns/net");

        let container_port =
            u16::try_from(config.exposed_port).map_err(|_| AgentError::Runtime {
                detail: format!("exposed_port {} exceeds u16 range", config.exposed_port),
            })?;

        let mut conflist = cni::generate_conflist(NETWORK_NAME);
        cni::inject_port_mappings(&mut conflist, host_port, container_port);
        // Comp-5: track every partial state we set up so a failure later
        // in create_and_start can roll back instead of leaking
        // libcontainer state_dir + CNI ip allocations + container
        // bundle on disk. Mirrors the comp-4 docker rollback approach.
        let cni_added = match cni::invoke_cni(
            &conflist,
            "ADD",
            &container_id,
            &netns_path,
            &self.config.cni_bin_path,
        )
        .await
        {
            Ok(_) => true,
            Err(e) => {
                self.rollback_libcontainer(&container_id, &container_dir).await;
                return Err(e);
            }
        };

        let state_dir = self.state_dir();
        let cid_c = container_id.clone();
        if let Err(e) = tokio::task::spawn_blocking(move || -> Result<(), AgentError> {
            let container_root = state_dir.join(&cid_c);
            let mut container =
                Container::load(container_root).map_err(|e| AgentError::Runtime {
                    detail: format!("failed to load container for start: {e}"),
                })?;
            container.start().map_err(|e| AgentError::Runtime {
                detail: format!("failed to start container: {e}"),
            })
        })
        .await
        .map_err(|e| AgentError::Runtime {
            detail: format!("container start task panicked: {e}"),
        })
        .and_then(|r| r)
        {
            if cni_added {
                self.rollback_cni(&conflist, &container_id, &netns_path)
                    .await;
            }
            self.rollback_libcontainer(&container_id, &container_dir).await;
            return Err(e);
        }

        let sandbox_id = config.sandbox_id.clone();
        self.lock_containers()?.insert(
            container_id.clone(),
            ContainerMetadata {
                sandbox_id: sandbox_id.clone(),
                host_port,
                container_dir,
                netns_path,
            },
        );

        Ok(ContainerInfo {
            id: ContainerId(container_id),
            sandbox_id,
            host_port,
            state: ContainerState::Running,
        })
    }

    async fn pause(&self, id: &ContainerId) -> Result<(), AgentError> {
        let container_id = id.0.clone();
        let state_dir = self.state_dir();
        tokio::task::spawn_blocking(move || -> Result<(), AgentError> {
            use libcontainer::container::ContainerStatus;
            let container_root = state_dir.join(&container_id);
            let mut container =
                Container::load(container_root).map_err(|e| AgentError::Runtime {
                    detail: format!("failed to load container for pause: {e}"),
                })?;
            // Cascade-fix #6: libcontainer's pause refuses to operate
            // on an already-paused container (returns an error). Map
            // that to idempotent success — mirrors agent-docker's 409
            // handling and avoids surfacing spurious failures when the
            // in-memory state has drifted from the kernel (e.g. after
            // an agent restart on a sandbox that was paused before the
            // restart).
            if container.status() == ContainerStatus::Paused {
                return Ok(());
            }
            container.pause().map_err(|e| AgentError::Runtime {
                detail: format!("failed to pause container: {e}"),
            })
        })
        .await
        .map_err(|e| AgentError::Runtime {
            detail: format!("pause task panicked: {e}"),
        })?
    }

    async fn unpause(&self, id: &ContainerId) -> Result<(), AgentError> {
        let container_id = id.0.clone();
        let state_dir = self.state_dir();
        tokio::task::spawn_blocking(move || -> Result<(), AgentError> {
            use libcontainer::container::ContainerStatus;
            let container_root = state_dir.join(&container_id);
            let mut container =
                Container::load(container_root).map_err(|e| AgentError::Runtime {
                    detail: format!("failed to load container for unpause: {e}"),
                })?;
            // Cascade-fix #6: idempotent unpause on an already-running
            // container.
            if container.status() == ContainerStatus::Running {
                return Ok(());
            }
            container.resume().map_err(|e| AgentError::Runtime {
                detail: format!("failed to resume container: {e}"),
            })
        })
        .await
        .map_err(|e| AgentError::Runtime {
            detail: format!("unpause task panicked: {e}"),
        })?
    }

    async fn stop_and_remove(&self, id: &ContainerId, timeout: Duration) -> Result<(), AgentError> {
        let container_id = id.0.clone();
        let metadata = self.lock_containers()?.remove(&container_id);

        let state_dir = self.state_dir();
        let cid = container_id.clone();
        let grace = timeout.min(STOP_GRACE_PERIOD);
        tokio::task::spawn_blocking(move || -> Result<(), AgentError> {
            use libcontainer::container::ContainerStatus;
            let container_root = state_dir.join(&cid);
            let mut container =
                Container::load(container_root).map_err(|e| AgentError::Runtime {
                    detail: format!("failed to load container for stop: {e}"),
                })?;
            // Cascade-fix #8: SIGTERM to a cgroup-frozen process is
            // queued in-kernel, not delivered, until the freezer is
            // released. Unpause first so the grace period actually
            // gives the in-container process a chance to handle the
            // signal — otherwise DELETE blocks for the full grace
            // before SIGKILL.
            if container.status() == ContainerStatus::Paused {
                let _ = container.resume();
            }
            // SIGTERM may fail if process already exited — not an error during shutdown
            let _ = container.kill(nix::sys::signal::Signal::SIGTERM, true);
            std::thread::sleep(grace);
            container.delete(true).map_err(|e| AgentError::Runtime {
                detail: format!("failed to delete container: {e}"),
            })
        })
        .await
        .map_err(|e| AgentError::Runtime {
            detail: format!("stop task panicked: {e}"),
        })??;

        if let Some(meta) = &metadata {
            let conflist = cni::generate_conflist(NETWORK_NAME);
            // CNI DEL is best-effort cleanup; network may already be torn down
            let _ = cni::invoke_cni(
                &conflist,
                "DEL",
                &container_id,
                &meta.netns_path,
                &self.config.cni_bin_path,
            )
            .await;
        }

        if let Some(meta) = metadata {
            // Best-effort cleanup; directory may not exist if creation failed partway
            let _ = tokio::fs::remove_dir_all(&meta.container_dir).await;
        }

        Ok(())
    }

    async fn list_sandbox_containers(&self) -> Result<Vec<ContainerInfo>, AgentError> {
        // Snapshot the in-memory map first so the lock isn't held across
        // libcontainer status reads (each is a blocking filesystem op).
        let snapshot: Vec<(String, SandboxId, u16)> = self
            .lock_containers()?
            .iter()
            .map(|(cid, meta)| (cid.clone(), meta.sandbox_id.clone(), meta.host_port))
            .collect();
        let state_dir = self.state_dir();
        tokio::task::spawn_blocking(move || {
            snapshot
                .into_iter()
                .map(|(cid, sandbox_id, host_port)| {
                    // Best-effort: if Container::load fails (state dir
                    // missing) we treat the entry as Stopped so reconcile
                    // discards it. Anything other than Running / Paused
                    // is Stopped — see ContainerState::Paused docs.
                    let container_root = state_dir.join(&cid);
                    let state = Container::load(container_root)
                        .ok()
                        .map(|c| {
                            use libcontainer::container::ContainerStatus;
                            match c.status() {
                                ContainerStatus::Running => ContainerState::Running,
                                ContainerStatus::Paused => ContainerState::Paused,
                                _ => ContainerState::Stopped,
                            }
                        })
                        .unwrap_or(ContainerState::Stopped);
                    ContainerInfo {
                        id: ContainerId(cid),
                        sandbox_id,
                        host_port,
                        state,
                    }
                })
                .collect::<Vec<_>>()
        })
        .await
        .map_err(|e| AgentError::Runtime {
            detail: format!("list_sandbox_containers task panicked: {e}"),
        })
        .map(Ok)?
    }

    async fn start_exec(
        &self,
        id: &ContainerId,
        start: ExecStart,
    ) -> Result<ExecHandle, AgentError> {
        let container_id = id.0.clone();
        let state_dir = self.state_dir();
        exec::start_exec_streaming(&container_id, &state_dir, start).await
    }

    async fn signal_exec(
        &self,
        id: &ContainerId,
        in_container_pid: i32,
        signum: i32,
    ) -> Result<(), AgentError> {
        if in_container_pid <= 0 {
            return Ok(());
        }
        let container_id = id.0.clone();
        let state_dir = self.state_dir();
        exec::signal_in_container(&container_id, &state_dir, in_container_pid, signum).await
    }

    async fn read_file(
        &self,
        id: &ContainerId,
        path: &str,
        cwd: Option<&str>,
    ) -> Result<Bytes, AgentError> {
        let resolved = resolve_path(path, cwd);
        let target_pid = exec::container_pid(&id.0, &self.state_dir())?;
        setns_ops::read_file_in_ns(target_pid, resolved).await
    }

    async fn write_file(
        &self,
        id: &ContainerId,
        path: &str,
        cwd: Option<&str>,
        content: Bytes,
    ) -> Result<(), AgentError> {
        let resolved = resolve_path(path, cwd);
        let target_pid = exec::container_pid(&id.0, &self.state_dir())?;
        setns_ops::write_file_in_ns(target_pid, resolved, content).await
    }

    async fn write_files_targz(
        &self,
        id: &ContainerId,
        cwd: Option<&str>,
        tarball: Bytes,
    ) -> Result<(), AgentError> {
        let target = cwd.unwrap_or(DEFAULT_WRITE_CWD).to_string();
        let target_pid = exec::container_pid(&id.0, &self.state_dir())?;
        setns_ops::extract_targz_in_ns(target_pid, target, tarball).await
    }
}

fn resolve_path(path: &str, cwd: Option<&str>) -> String {
    if path.starts_with('/') {
        return path.to_string();
    }
    let base = cwd.unwrap_or(DEFAULT_WRITE_CWD);
    format!("{}/{}", base.trim_end_matches('/'), path)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use open_sandbox_contracts::types::SandboxId;
    use serial_test::serial;

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
            pull_policy: open_sandbox_contracts::types::PullPolicy::IfNotPresent,
        }
    }

    #[tokio::test]
    #[serial]
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
    #[serial]
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
    #[serial]
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
    #[serial]
    async fn start_exec_runs_echo() {
        let runtime = YoukiRuntime::new(test_config()).unwrap();
        let sandbox_id = SandboxId::new();
        let config = sandbox_config(sandbox_id);

        let info = runtime.create_and_start(config).await.unwrap();

        let handle = runtime
            .start_exec(
                &info.id,
                ExecStart {
                    command: vec!["echo".into(), "hello".into()],
                    cwd: String::new(),
                    env: HashMap::new(),
                },
            )
            .await
            .unwrap();

        let mut handle = handle;
        drop(handle.stdin);
        let mut stdout: Vec<u8> = Vec::new();
        while let Some(chunk) = handle.stdout.recv().await {
            stdout.extend_from_slice(&chunk);
        }
        let exit = handle.exited.await.unwrap();
        assert_eq!(exit.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&stdout).trim(), "hello");

        let _ = runtime
            .stop_and_remove(&info.id, Duration::from_secs(5))
            .await;
    }
}
