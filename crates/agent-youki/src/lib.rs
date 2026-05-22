pub mod cni;
pub mod exec;
pub mod image;
pub mod spec;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use open_sandbox_agent::container::{
    ContainerConfig, ContainerId, ContainerInfo, ContainerRuntime, ExecOutput,
};
use open_sandbox_contracts::error::AgentError;
use open_sandbox_contracts::types::SandboxId;

const CONTAINER_ID_PREFIX: &str = "osb";
const CGROUP_PATH_PREFIX: &str = "/open-sandbox";
const NETWORK_NAME: &str = "open-sandbox";
const STOP_GRACE_PERIOD: Duration = Duration::from_secs(10);

use libcontainer::container::builder::ContainerBuilder;
use libcontainer::container::Container;
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

    fn lock_containers(&self) -> Result<std::sync::MutexGuard<'_, HashMap<String, ContainerMetadata>>, AgentError> {
        self.containers.lock().map_err(|_| AgentError::Runtime {
            detail: "container metadata lock poisoned".into(),
        })
    }
}

impl ContainerRuntime for YoukiRuntime {
    async fn create_and_start(
        &self,
        config: ContainerConfig,
    ) -> Result<ContainerInfo, AgentError> {
        let rootfs = self.image_manager.pull_and_unpack(&config.image).await?;

        let host_port = cni::allocate_port()?;
        let container_id = format!("{CONTAINER_ID_PREFIX}-{}", uuid::Uuid::new_v4());

        let container_dir = self
            .config
            .root_dir
            .join("containers")
            .join(&container_id);
        let bundle_dir = container_dir.join("bundle");
        tokio::fs::create_dir_all(&bundle_dir)
            .await
            .map_err(|e| AgentError::Runtime {
                detail: format!("failed to create bundle directory: {e}"),
            })?;

        let cgroup_path = format!("{CGROUP_PATH_PREFIX}/{container_id}");
        let rootfs_str = rootfs.to_string_lossy().to_string();
        let oci_spec =
            spec::generate_full_spec(&config, &rootfs_str, Some(&cgroup_path))?;
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

        let container_port = u16::try_from(config.exposed_port).map_err(|_| AgentError::Runtime {
            detail: format!("exposed_port {} exceeds u16 range", config.exposed_port),
        })?;

        let mut conflist = cni::generate_conflist(NETWORK_NAME);
        cni::inject_port_mappings(&mut conflist, host_port, container_port);
        cni::invoke_cni(
            &conflist,
            "ADD",
            &container_id,
            &netns_path,
            &self.config.cni_bin_path,
        )
        .await?;

        let state_dir = self.state_dir();
        let cid_c = container_id.clone();
        tokio::task::spawn_blocking(move || -> Result<(), AgentError> {
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
        })??;

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
            running: true,
        })
    }

    async fn stop_and_remove(
        &self,
        id: &ContainerId,
        timeout: Duration,
    ) -> Result<(), AgentError> {
        let container_id = id.0.clone();
        let metadata = self.lock_containers()?.remove(&container_id);

        let state_dir = self.state_dir();
        let cid = container_id.clone();
        let grace = timeout.min(STOP_GRACE_PERIOD);
        tokio::task::spawn_blocking(move || -> Result<(), AgentError> {
            let container_root = state_dir.join(&cid);
            let mut container =
                Container::load(container_root).map_err(|e| AgentError::Runtime {
                    detail: format!("failed to load container for stop: {e}"),
                })?;
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
        let containers = self.lock_containers()?;
        Ok(containers
            .iter()
            .map(|(cid, meta)| ContainerInfo {
                id: ContainerId(cid.clone()),
                sandbox_id: meta.sandbox_id.clone(),
                host_port: meta.host_port,
                running: true,
            })
            .collect())
    }

    async fn exec(
        &self,
        id: &ContainerId,
        command: Vec<String>,
        stdin: Vec<u8>,
    ) -> Result<ExecOutput, AgentError> {
        let container_id = id.0.clone();
        let state_dir = self.state_dir();

        tokio::task::spawn_blocking(move || {
            exec::exec_in_container(&container_id, &state_dir, command, stdin)
        })
        .await
        .map_err(|e| AgentError::Runtime {
            detail: format!("exec task panicked: {e}"),
        })?
    }
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
