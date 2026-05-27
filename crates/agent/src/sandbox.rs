use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tracing::{info, warn};

use open_sandbox_contracts::controller::{
    PauseSandbox, SandboxState, StartSandbox, StopSandbox, UnpauseSandbox,
};
use open_sandbox_contracts::error::AgentError;
use open_sandbox_contracts::types::SandboxId;

use crate::container::{ContainerConfig, ContainerId, ContainerRuntime, ContainerState};

fn parse_sandbox_id(raw: &str) -> Result<SandboxId, AgentError> {
    uuid::Uuid::parse_str(raw)
        .map(SandboxId::from)
        .map_err(|e| AgentError::Internal {
            detail: e.to_string(),
        })
}

#[derive(Debug, Clone)]
pub struct SandboxEntry {
    pub sandbox_id: SandboxId,
    pub container_id: ContainerId,
    pub host_port: u16,
    pub state: SandboxState,
}

pub struct SandboxManager<R: ContainerRuntime> {
    runtime: Arc<R>,
    sandboxes: Mutex<HashMap<SandboxId, SandboxEntry>>,
    /// v1.0.2 cascade-fix #7: per-sandbox async lock that serializes
    /// lifecycle commands (start / stop / pause / unpause) for the
    /// same sandbox. Without this, the controller can dispatch two
    /// commands back-to-back (e.g. pause then stop) and the agent's
    /// lock-clone-await-relock pattern lets the second command run
    /// against intermediate state. The map of locks is guarded by a
    /// brief std::sync::Mutex; the lock entries themselves are
    /// tokio::sync::Mutex so they can be held across the runtime
    /// call's `.await`. Entries are not GC'd (small per-sandbox cost,
    /// acceptable).
    command_locks: Mutex<HashMap<SandboxId, Arc<tokio::sync::Mutex<()>>>>,
}

impl<R: ContainerRuntime> SandboxManager<R> {
    pub fn new(runtime: Arc<R>) -> Self {
        Self {
            runtime,
            sandboxes: Mutex::new(HashMap::new()),
            command_locks: Mutex::new(HashMap::new()),
        }
    }

    /// Borrow the per-sandbox lifecycle lock. Lazily inserts a fresh
    /// `tokio::sync::Mutex` on first access.
    fn command_lock(&self, sandbox_id: &SandboxId) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.command_locks.lock().unwrap();
        locks
            .entry(sandbox_id.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    pub async fn start_sandbox(&self, cmd: StartSandbox) -> Result<SandboxState, AgentError> {
        let sandbox_id = parse_sandbox_id(&cmd.sandbox_id)?;
        // Cascade-fix #7: hold the per-sandbox lifecycle lock across
        // create_and_start so a stop dispatched mid-creation can't
        // interleave with the create.
        let lock = self.command_lock(&sandbox_id);
        let _guard = lock.lock().await;

        let config = cmd.config.unwrap_or_default();
        let container_config = ContainerConfig {
            sandbox_id: sandbox_id.clone(),
            image: cmd.image,
            cpu_limit_millicores: config.cpu_limit_millicores,
            memory_limit_bytes: config.memory_limit_bytes,
            env_vars: config.env_vars,
            exposed_port: config.exposed_port,
            pull_policy: open_sandbox_contracts::types::PullPolicy::from(config.pull_policy),
        };

        // v1.0.2 (iter11): bound the runtime's create_and_start by
        // `SANDBOX_CREATE_DEADLINE`. The wrap lives here in
        // SandboxManager rather than inside each ContainerRuntime
        // impl so DockerRuntime and YoukiRuntime are bounded
        // uniformly (production uses youki per ADR-009; without this
        // hoist the deadline only applied to the dev/docker path).
        // The DockerRuntime's worst-case retry budget is documented
        // in `pull_image_with_retry` + the port-retry loop (~90s of
        // sleeps + RTTs under sustained registry pressure); the
        // 60s ceiling intentionally fails loud BELOW that budget so
        // an outlier doesn't camp on an agent thread for the full
        // worst case. On expiry the partially-created container (if
        // create_container succeeded but start_container was
        // mid-flight) leaks until the agent process restarts — a
        // periodic agent-side reconcile-and-cleanup is a separate
        // follow-up (`SandboxManager::reconcile` exists but has no
        // periodic caller today).
        let deadline = open_sandbox_contracts::constants::SANDBOX_CREATE_DEADLINE;
        let sandbox_id_for_err = sandbox_id.clone();
        let runtime_result = match tokio::time::timeout(
            deadline,
            self.runtime.create_and_start(container_config),
        )
        .await
        {
            Ok(r) => r,
            Err(_) => {
                warn!(
                    sandbox_id = %sandbox_id_for_err,
                    deadline_secs = deadline.as_secs(),
                    "create_and_start deadline exceeded; partial container (if any) leaks until reconcile sweep is wired"
                );
                Err(AgentError::Runtime {
                    detail: format!(
                        "create_and_start deadline of {}s exceeded for sandbox {sandbox_id_for_err}",
                        deadline.as_secs()
                    ),
                })
            }
        };

        match runtime_result {
            Ok(info) => {
                info!(sandbox_id = %sandbox_id, host_port = info.host_port, "sandbox started");
                let entry = SandboxEntry {
                    sandbox_id: sandbox_id.clone(),
                    container_id: info.id,
                    host_port: info.host_port,
                    state: SandboxState::Running,
                };
                self.sandboxes.lock().unwrap().insert(sandbox_id, entry);
                Ok(SandboxState::Running)
            }
            Err(e) => {
                warn!(sandbox_id = %sandbox_id, error = %e, "sandbox start failed");
                let entry = SandboxEntry {
                    sandbox_id: sandbox_id.clone(),
                    container_id: ContainerId(String::new()),
                    host_port: 0,
                    state: SandboxState::Failed,
                };
                self.sandboxes.lock().unwrap().insert(sandbox_id, entry);
                // Bubble the runtime error up so controller_client
                // can include the detail in SandboxStatus.error_message.
                // The `creating → failed` transition is already
                // captured by the SandboxEntry insert above, so this
                // doesn't lose any local accounting.
                Err(e)
            }
        }
    }

    /// v1.0.2: freeze the in-container processes via the runtime's
    /// pause primitive (Docker pause / cgroup-v2 freezer). The
    /// SandboxEntry's local state is updated to `Paused` so subsequent
    /// reconcile passes report the right snapshot. Idempotent: pausing
    /// an already-paused sandbox returns Paused without dispatching.
    ///
    /// Holds the per-sandbox lifecycle lock for the duration of the
    /// runtime call (cascade-fix #7) so a concurrent stop_sandbox
    /// can't interleave.
    pub async fn pause_sandbox(&self, cmd: PauseSandbox) -> Result<SandboxState, AgentError> {
        let sandbox_id = parse_sandbox_id(&cmd.sandbox_id)?;
        let lock = self.command_lock(&sandbox_id);
        let _guard = lock.lock().await;
        let entry = self
            .sandboxes
            .lock()
            .unwrap()
            .get(&sandbox_id)
            .cloned()
            .ok_or_else(|| AgentError::SandboxNotFound {
                sandbox_id: sandbox_id.to_string(),
            })?;
        if entry.state == SandboxState::Paused {
            return Ok(SandboxState::Paused);
        }
        self.runtime.pause(&entry.container_id).await?;
        if let Some(slot) = self.sandboxes.lock().unwrap().get_mut(&sandbox_id) {
            slot.state = SandboxState::Paused;
        }
        info!(sandbox_id = %sandbox_id, "sandbox paused");
        Ok(SandboxState::Paused)
    }

    /// v1.0.2: resume a paused sandbox. Inverse of `pause_sandbox`;
    /// idempotent on already-running sandboxes.
    pub async fn unpause_sandbox(&self, cmd: UnpauseSandbox) -> Result<SandboxState, AgentError> {
        let sandbox_id = parse_sandbox_id(&cmd.sandbox_id)?;
        let lock = self.command_lock(&sandbox_id);
        let _guard = lock.lock().await;
        let entry = self
            .sandboxes
            .lock()
            .unwrap()
            .get(&sandbox_id)
            .cloned()
            .ok_or_else(|| AgentError::SandboxNotFound {
                sandbox_id: sandbox_id.to_string(),
            })?;
        if entry.state == SandboxState::Running {
            return Ok(SandboxState::Running);
        }
        self.runtime.unpause(&entry.container_id).await?;
        if let Some(slot) = self.sandboxes.lock().unwrap().get_mut(&sandbox_id) {
            slot.state = SandboxState::Running;
        }
        info!(sandbox_id = %sandbox_id, "sandbox unpaused");
        Ok(SandboxState::Running)
    }

    pub async fn stop_sandbox(&self, cmd: StopSandbox) -> Result<SandboxState, AgentError> {
        let sandbox_id = parse_sandbox_id(&cmd.sandbox_id)?;
        // Cascade-fix #7: hold the per-sandbox lifecycle lock across
        // the runtime call so concurrent pause/unpause/start commands
        // for the same sandbox don't race the stop.
        let lock = self.command_lock(&sandbox_id);
        let _guard = lock.lock().await;

        let entry = self
            .sandboxes
            .lock()
            .unwrap()
            .get(&sandbox_id)
            .cloned()
            .ok_or_else(|| AgentError::SandboxNotFound {
                sandbox_id: sandbox_id.to_string(),
            })?;

        let timeout = std::time::Duration::from_secs(cmd.timeout_seconds as u64);
        self.runtime
            .stop_and_remove(&entry.container_id, timeout)
            .await?;
        self.sandboxes.lock().unwrap().remove(&sandbox_id);
        info!(sandbox_id = %sandbox_id, "sandbox stopped");
        Ok(SandboxState::Stopped)
    }

    pub fn get_sandbox(&self, sandbox_id: &SandboxId) -> Option<SandboxEntry> {
        self.sandboxes.lock().unwrap().get(sandbox_id).cloned()
    }

    pub fn list_sandboxes(&self) -> Vec<SandboxEntry> {
        self.sandboxes.lock().unwrap().values().cloned().collect()
    }

    pub fn runtime(&self) -> &Arc<R> {
        &self.runtime
    }

    pub fn host_port_for(&self, sandbox_id: &SandboxId) -> Result<u16, AgentError> {
        self.sandboxes
            .lock()
            .unwrap()
            .get(sandbox_id)
            .map(|e| e.host_port)
            .ok_or_else(|| AgentError::SandboxNotFound {
                sandbox_id: sandbox_id.to_string(),
            })
    }

    /// Look up the container ID for a sandbox. Used by the
    /// proxy-side IO stream router (`io_stream::drive_io_session`)
    /// to dispatch into the runtime. Returns the same
    /// `SandboxNotFound` error as the rest of this manager.
    pub fn container_id_for(&self, sandbox_id: &SandboxId) -> Result<ContainerId, AgentError> {
        self.sandboxes
            .lock()
            .unwrap()
            .get(sandbox_id)
            .map(|e| e.container_id.clone())
            .ok_or_else(|| AgentError::SandboxNotFound {
                sandbox_id: sandbox_id.to_string(),
            })
    }

    /// Look up both the container ID and the current sandbox state in
    /// a single lock acquisition. Used by the IO stream router to
    /// reject exec/file ops against paused sandboxes — without this,
    /// a frozen container would accept the syscall and block in-kernel
    /// while the gateway's keepalive (which terminates outside the
    /// container) keeps the WebSocket alive indefinitely. Cascade-fix #3.
    pub fn container_for_io(
        &self,
        sandbox_id: &SandboxId,
    ) -> Result<(ContainerId, SandboxState), AgentError> {
        self.sandboxes
            .lock()
            .unwrap()
            .get(sandbox_id)
            .map(|e| (e.container_id.clone(), e.state))
            .ok_or_else(|| AgentError::SandboxNotFound {
                sandbox_id: sandbox_id.to_string(),
            })
    }

    pub async fn reconcile(&self) -> Result<Vec<SandboxEntry>, AgentError> {
        let containers = self.runtime.list_sandbox_containers().await?;
        let mut entries = Vec::with_capacity(containers.len());
        let mut sandboxes = self.sandboxes.lock().unwrap();

        for info in containers {
            let state = match info.state {
                ContainerState::Running => SandboxState::Running,
                ContainerState::Paused => SandboxState::Paused,
                ContainerState::Stopped => SandboxState::Stopped,
            };
            let entry = SandboxEntry {
                sandbox_id: info.sandbox_id.clone(),
                container_id: info.id,
                host_port: info.host_port,
                state,
            };
            sandboxes.insert(info.sandbox_id, entry.clone());
            entries.push(entry);
        }

        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;
    use std::time::Duration;

    fn stop_cmd(sandbox_id: &SandboxId, timeout_secs: u32) -> StopSandbox {
        StopSandbox {
            sandbox_id: sandbox_id.to_string(),
            timeout_seconds: timeout_secs,
        }
    }

    #[tokio::test]
    async fn start_sandbox_creates_and_starts_container() {
        let runtime = Arc::new(MockContainerRuntime::new());
        let manager = SandboxManager::new(runtime.clone());
        let sandbox_id = SandboxId::new();

        let state = manager
            .start_sandbox(start_cmd(&sandbox_id, "nginx:latest"))
            .await
            .unwrap();

        assert_eq!(state, SandboxState::Running);
        assert_eq!(runtime.created_count(), 1);
    }

    #[tokio::test]
    async fn start_sandbox_records_mapping() {
        let runtime = Arc::new(MockContainerRuntime::new());
        let manager = SandboxManager::new(runtime);
        let sandbox_id = SandboxId::new();

        manager
            .start_sandbox(start_cmd(&sandbox_id, "nginx:latest"))
            .await
            .unwrap();

        let entry = manager.get_sandbox(&sandbox_id);
        assert!(entry.is_some());
        let entry = entry.unwrap();
        assert_eq!(entry.sandbox_id, sandbox_id);
        assert_eq!(entry.state, SandboxState::Running);
    }

    #[tokio::test]
    async fn start_sandbox_returns_err_with_runtime_detail_on_failure() {
        let runtime = Arc::new(FailingContainerRuntime);
        let manager = SandboxManager::new(runtime);
        let sandbox_id = SandboxId::new();

        let err = manager
            .start_sandbox(start_cmd(&sandbox_id, "nginx:latest"))
            .await
            .expect_err("expected runtime error to bubble up");
        let detail = err.to_string();
        assert!(
            detail.contains("mock runtime failure"),
            "expected runtime detail in error, got: {detail}"
        );

        // Local accounting still records the failed entry so a
        // subsequent stop/get observes the terminal state.
        let entry = manager.sandboxes.lock().unwrap().get(&sandbox_id).cloned();
        assert_eq!(entry.map(|e| e.state), Some(SandboxState::Failed));
    }

    /// v1.0.2 (iter11): anchors the operator-facing deadline-exceeded
    /// error message format so a future refactor can't silently drift
    /// log-scanner regexes / dashboards. The actual deadline value is
    /// the const `SANDBOX_CREATE_DEADLINE = 60s`; with paused time we
    /// advance virtual time past it without real wall-clock delay.
    #[tokio::test(start_paused = true)]
    async fn start_sandbox_deadline_exceeded_returns_anchored_error() {
        let deadline = open_sandbox_contracts::constants::SANDBOX_CREATE_DEADLINE;
        // Runtime sleeps for 2× the deadline so the timeout always fires.
        let runtime = Arc::new(SlowContainerRuntime {
            sleep: deadline * 2,
        });
        let manager = SandboxManager::new(runtime);
        let sandbox_id = SandboxId::new();

        let err = manager
            .start_sandbox(start_cmd(&sandbox_id, "nginx:latest"))
            .await
            .expect_err("expected timeout error");
        let detail = err.to_string();
        // Format is "create_and_start deadline of {N}s exceeded for sandbox {uuid}".
        assert!(
            detail.contains("create_and_start deadline of"),
            "format drift would break operator alerts; got: {detail}"
        );
        assert!(
            detail.contains(&format!("{}s exceeded", deadline.as_secs())),
            "deadline-secs must be in the error; got: {detail}"
        );
        assert!(
            detail.contains(&sandbox_id.to_string()),
            "sandbox_id must be in the error for log correlation; got: {detail}"
        );
        // Failed-state accounting still applies.
        let entry = manager.sandboxes.lock().unwrap().get(&sandbox_id).cloned();
        assert_eq!(entry.map(|e| e.state), Some(SandboxState::Failed));
    }

    /// Happy path: runtime returns Ok well before the deadline; no
    /// timeout interference. Pinned alongside the deadline test so
    /// future deadline changes can't accidentally break the success
    /// path.
    #[tokio::test(start_paused = true)]
    async fn start_sandbox_runtime_within_deadline_succeeds() {
        let runtime = Arc::new(SlowContainerRuntime {
            sleep: Duration::from_millis(50),
        });
        let manager = SandboxManager::new(runtime);
        let sandbox_id = SandboxId::new();
        let state = manager
            .start_sandbox(start_cmd(&sandbox_id, "nginx:latest"))
            .await
            .expect("should succeed within deadline");
        assert_eq!(state, SandboxState::Running);
    }

    #[tokio::test]
    async fn stop_sandbox_stops_and_removes_container() {
        let runtime = Arc::new(MockContainerRuntime::new());
        let manager = SandboxManager::new(runtime.clone());
        let sandbox_id = SandboxId::new();

        manager
            .start_sandbox(start_cmd(&sandbox_id, "nginx:latest"))
            .await
            .unwrap();

        let state = manager
            .stop_sandbox(stop_cmd(&sandbox_id, 10))
            .await
            .unwrap();

        assert_eq!(state, SandboxState::Stopped);
        assert_eq!(runtime.stopped_count(), 1);
    }

    #[tokio::test]
    async fn stop_sandbox_removes_mapping() {
        let runtime = Arc::new(MockContainerRuntime::new());
        let manager = SandboxManager::new(runtime);
        let sandbox_id = SandboxId::new();

        manager
            .start_sandbox(start_cmd(&sandbox_id, "nginx:latest"))
            .await
            .unwrap();

        manager
            .stop_sandbox(stop_cmd(&sandbox_id, 10))
            .await
            .unwrap();

        assert!(manager.get_sandbox(&sandbox_id).is_none());
    }

    #[tokio::test]
    async fn stop_unknown_sandbox_returns_error() {
        let runtime = Arc::new(MockContainerRuntime::new());
        let manager = SandboxManager::new(runtime);

        let result = manager.stop_sandbox(stop_cmd(&SandboxId::new(), 10)).await;

        assert!(matches!(result, Err(AgentError::SandboxNotFound { .. })));
    }

    #[tokio::test]
    async fn host_port_for_running_sandbox() {
        let runtime = Arc::new(MockContainerRuntime::new());
        let manager = SandboxManager::new(runtime);
        let sandbox_id = SandboxId::new();

        manager
            .start_sandbox(start_cmd(&sandbox_id, "nginx:latest"))
            .await
            .unwrap();

        let port = manager.host_port_for(&sandbox_id).unwrap();
        assert!(port > 0);
    }

    #[tokio::test]
    async fn host_port_for_unknown_sandbox_returns_error() {
        let runtime = Arc::new(MockContainerRuntime::new());
        let manager = SandboxManager::new(runtime);

        let result = manager.host_port_for(&SandboxId::new());
        assert!(matches!(result, Err(AgentError::SandboxNotFound { .. })));
    }

    #[tokio::test]
    async fn list_running_sandboxes() {
        let runtime = Arc::new(MockContainerRuntime::new());
        let manager = SandboxManager::new(runtime);

        manager
            .start_sandbox(start_cmd(&SandboxId::new(), "nginx:latest"))
            .await
            .unwrap();
        manager
            .start_sandbox(start_cmd(&SandboxId::new(), "python:3"))
            .await
            .unwrap();

        assert_eq!(manager.list_sandboxes().len(), 2);
    }

    #[tokio::test]
    async fn container_id_for_returns_id_for_running_sandbox() {
        let runtime = Arc::new(MockContainerRuntime::new());
        let manager = SandboxManager::new(runtime);
        let sandbox_id = SandboxId::new();

        manager
            .start_sandbox(start_cmd(&sandbox_id, "nginx:latest"))
            .await
            .unwrap();

        let cid = manager.container_id_for(&sandbox_id).unwrap();
        assert!(!cid.0.is_empty());
    }

    #[tokio::test]
    async fn container_id_for_unknown_sandbox_errs() {
        let runtime = Arc::new(MockContainerRuntime::new());
        let manager = SandboxManager::new(runtime);

        let result = manager.container_id_for(&SandboxId::new());
        assert!(matches!(result, Err(AgentError::SandboxNotFound { .. })));
    }

    fn pause_cmd(sandbox_id: &SandboxId) -> PauseSandbox {
        PauseSandbox {
            sandbox_id: sandbox_id.to_string(),
        }
    }
    fn unpause_cmd(sandbox_id: &SandboxId) -> UnpauseSandbox {
        UnpauseSandbox {
            sandbox_id: sandbox_id.to_string(),
        }
    }

    #[tokio::test]
    async fn pause_then_unpause_transitions_state_and_calls_runtime() {
        let runtime = Arc::new(MockContainerRuntime::new());
        let manager = SandboxManager::new(runtime.clone());
        let sandbox_id = SandboxId::new();
        manager
            .start_sandbox(start_cmd(&sandbox_id, "alpine:3"))
            .await
            .unwrap();

        let s = manager.pause_sandbox(pause_cmd(&sandbox_id)).await.unwrap();
        assert_eq!(s, SandboxState::Paused);
        assert_eq!(runtime.paused_count(), 1);
        assert_eq!(
            manager.get_sandbox(&sandbox_id).unwrap().state,
            SandboxState::Paused
        );

        let s = manager
            .unpause_sandbox(unpause_cmd(&sandbox_id))
            .await
            .unwrap();
        assert_eq!(s, SandboxState::Running);
        assert_eq!(runtime.unpaused_count(), 1);
        assert_eq!(
            manager.get_sandbox(&sandbox_id).unwrap().state,
            SandboxState::Running
        );
    }

    #[tokio::test]
    async fn pause_is_idempotent_when_already_paused() {
        let runtime = Arc::new(MockContainerRuntime::new());
        let manager = SandboxManager::new(runtime.clone());
        let sandbox_id = SandboxId::new();
        manager
            .start_sandbox(start_cmd(&sandbox_id, "alpine:3"))
            .await
            .unwrap();
        manager.pause_sandbox(pause_cmd(&sandbox_id)).await.unwrap();

        // Second pause is a steady-state success that does NOT dispatch
        // to the runtime — important because Docker returns 409 on the
        // already-paused case, and we don't want spurious runtime calls.
        let s = manager.pause_sandbox(pause_cmd(&sandbox_id)).await.unwrap();
        assert_eq!(s, SandboxState::Paused);
        assert_eq!(runtime.paused_count(), 1, "no extra runtime calls");
    }

    #[tokio::test]
    async fn unpause_is_idempotent_when_already_running() {
        let runtime = Arc::new(MockContainerRuntime::new());
        let manager = SandboxManager::new(runtime.clone());
        let sandbox_id = SandboxId::new();
        manager
            .start_sandbox(start_cmd(&sandbox_id, "alpine:3"))
            .await
            .unwrap();

        let s = manager
            .unpause_sandbox(unpause_cmd(&sandbox_id))
            .await
            .unwrap();
        assert_eq!(s, SandboxState::Running);
        assert_eq!(runtime.unpaused_count(), 0, "no runtime call on no-op");
    }

    #[tokio::test]
    async fn container_for_io_returns_state() {
        // Cascade-fix #3: the IO router needs both ContainerId and
        // SandboxState in one lock acquisition so it can reject ops
        // against paused sandboxes up front.
        let runtime = Arc::new(MockContainerRuntime::new());
        let manager = SandboxManager::new(runtime);
        let sandbox_id = SandboxId::new();
        manager
            .start_sandbox(start_cmd(&sandbox_id, "alpine:3"))
            .await
            .unwrap();
        let (_, state) = manager.container_for_io(&sandbox_id).unwrap();
        assert_eq!(state, SandboxState::Running);

        manager
            .pause_sandbox(pause_cmd(&sandbox_id))
            .await
            .unwrap();
        let (_, state) = manager.container_for_io(&sandbox_id).unwrap();
        assert_eq!(state, SandboxState::Paused);
    }

    #[tokio::test]
    async fn pause_unknown_sandbox_errs() {
        let runtime = Arc::new(MockContainerRuntime::new());
        let manager = SandboxManager::new(runtime);
        let result = manager.pause_sandbox(pause_cmd(&SandboxId::new())).await;
        assert!(matches!(result, Err(AgentError::SandboxNotFound { .. })));
    }

    #[tokio::test]
    async fn pause_serializes_against_concurrent_stop() {
        // Cascade-fix #7: pause and stop for the same sandbox must
        // serialize. Before this fix, the lock-then-await pattern let
        // a stop's stop_and_remove complete while a concurrent pause
        // was still awaiting runtime.pause; the post-await get_mut
        // would no-op, and the runtime calls overlapped.
        let runtime = Arc::new(MockContainerRuntime::new());
        let manager = Arc::new(SandboxManager::new(runtime.clone()));
        let sandbox_id = SandboxId::new();
        manager
            .start_sandbox(start_cmd(&sandbox_id, "alpine:3"))
            .await
            .unwrap();

        // Dispatch pause and stop concurrently.
        let m1 = manager.clone();
        let id1 = sandbox_id.clone();
        let pause_task = tokio::spawn(async move { m1.pause_sandbox(pause_cmd(&id1)).await });
        let m2 = manager.clone();
        let id2 = sandbox_id.clone();
        let stop_task = tokio::spawn(async move { m2.stop_sandbox(stop_cmd(&id2, 1)).await });

        let _ = pause_task.await.unwrap();
        let _ = stop_task.await.unwrap();

        // Exactly one of {pause-then-stop, stop-then-pause-fails} must
        // have happened. With serialization: stop wins → runtime
        // stop_and_remove called exactly once; map empty. Pause runs
        // first → state=Paused briefly, then stop runs → map empty.
        // Either way the final state is consistent: no entry exists.
        assert!(manager.get_sandbox(&sandbox_id).is_none());
        assert_eq!(runtime.stopped_count(), 1);
    }

    #[tokio::test]
    async fn reconcile_preserves_paused_state() {
        // Regression for cascade-fix #1: prior versions mapped
        // ContainerInfo.running (binary) → SandboxState (binary), so
        // a paused container returned `running=false` from
        // list_sandbox_containers and silently collapsed to Stopped
        // on every reconcile sweep. The tri-state ContainerState
        // must round-trip.
        let sandbox_id = SandboxId::new();
        let runtime = Arc::new(MockContainerRuntime::with_existing(vec![
            mock_container_info_paused(sandbox_id.clone(), 8080),
        ]));
        let manager = SandboxManager::new(runtime);
        let entries = manager.reconcile().await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].state, SandboxState::Paused);
        assert_eq!(
            manager.get_sandbox(&sandbox_id).unwrap().state,
            SandboxState::Paused,
        );
    }

    #[tokio::test]
    async fn reconcile_discovers_existing_containers() {
        let runtime = Arc::new(MockContainerRuntime::with_existing(vec![
            mock_container_info(SandboxId::new(), 8080),
            mock_container_info(SandboxId::new(), 8081),
        ]));
        let manager = SandboxManager::new(runtime);

        let reconciled = manager.reconcile().await.unwrap();

        assert_eq!(reconciled.len(), 2);
        assert_eq!(manager.list_sandboxes().len(), 2);
    }
}
