//! ExecRegistry — tracks live streaming execs so the stream-close
//! cleanup hook can SIGTERM/SIGKILL the in-container PID.
//!
//! Required by spike 01 and spike 02 results: neither the docker
//! runtime nor youki's nsenter mechanism propagates client
//! disconnect to the in-container process. The agent must
//! explicitly issue signals when the gateway-side WebSocket
//! closes. The registry is the bookkeeping that makes that
//! possible.
//!
//! Keyed on `stream_id` (proxy-assigned, wire-level). The
//! runtime's own `exec_id` is held in the record for diagnostic
//! correlation only.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tracing::{info, warn};

use open_sandbox_contracts::types::SandboxId;

use crate::container::{ContainerId, ContainerRuntime};

const SIGTERM: i32 = 15;
const SIGKILL: i32 = 9;

#[derive(Debug, Clone)]
pub struct ExecRecord {
    pub stream_id: String,
    pub sandbox_id: SandboxId,
    pub container_id: ContainerId,
    pub exec_id: String,
    pub in_container_pid: i32,
    pub started_at: Instant,
}

#[derive(Default)]
pub struct ExecRegistry {
    inner: Mutex<HashMap<String, ExecRecord>>,
}

impl ExecRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, record: ExecRecord) {
        info!(
            stream_id = %record.stream_id,
            sandbox_id = %record.sandbox_id,
            exec_id = %record.exec_id,
            in_container_pid = record.in_container_pid,
            "exec_registry.insert"
        );
        self.inner
            .lock()
            .unwrap()
            .insert(record.stream_id.clone(), record);
    }

    pub fn remove(&self, stream_id: &str) -> Option<ExecRecord> {
        self.inner.lock().unwrap().remove(stream_id)
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().unwrap().is_empty()
    }

    pub fn list_for_sandbox(&self, id: &SandboxId) -> Vec<ExecRecord> {
        self.inner
            .lock()
            .unwrap()
            .values()
            .filter(|r| r.sandbox_id == *id)
            .cloned()
            .collect()
    }
}

/// Stream-close cleanup hook. Sends SIGTERM, waits `grace`, then
/// SIGKILL to the in-container PID via the runtime trait.
///
/// `signal_exec` is treated as best-effort: errors (typically
/// "the PID is already gone") are logged at WARN but do not
/// propagate. The cleanup attempt is what matters; the exit
/// notification reaches the caller via the original `IoExited`
/// frame on the I/O stream independently.
pub async fn on_stream_closed<R: ContainerRuntime>(
    runtime: &R,
    registry: &ExecRegistry,
    stream_id: &str,
    grace: Duration,
) {
    let Some(rec) = registry.remove(stream_id) else {
        // No record — already cleaned up, or the exec exited before
        // we could record it (microsecond-lifetime case from spike
        // 05's analysis). Either way, nothing to do.
        return;
    };

    info!(
        stream_id = %rec.stream_id,
        sandbox_id = %rec.sandbox_id,
        in_container_pid = rec.in_container_pid,
        grace_ms = grace.as_millis() as u64,
        "exec_registry.signal_sent signal=SIGTERM"
    );

    if let Err(e) = runtime
        .signal_exec(&rec.container_id, rec.in_container_pid, SIGTERM)
        .await
    {
        warn!(
            stream_id = %rec.stream_id,
            in_container_pid = rec.in_container_pid,
            error = %e,
            "SIGTERM delivery failed (process may have already exited)"
        );
    }

    tokio::time::sleep(grace).await;

    info!(
        stream_id = %rec.stream_id,
        sandbox_id = %rec.sandbox_id,
        in_container_pid = rec.in_container_pid,
        "exec_registry.signal_sent signal=SIGKILL"
    );

    if let Err(e) = runtime
        .signal_exec(&rec.container_id, rec.in_container_pid, SIGKILL)
        .await
    {
        warn!(
            stream_id = %rec.stream_id,
            in_container_pid = rec.in_container_pid,
            error = %e,
            "SIGKILL delivery failed (process likely already exited)"
        );
    }
}
