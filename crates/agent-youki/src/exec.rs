//! Streaming exec for the youki runtime.
//!
//! Spawns `nsenter` into the target container's namespaces and pumps
//! stdin/stdout/stderr through tokio channels. Per spike 02, the
//! in-namespace child does NOT die when the host-side nsenter is
//! killed — so the agent must capture the in-container PID at start
//! and explicitly issue signals on stream close via
//! `signal_in_container` (used by the ExecRegistry cleanup hook).
//!
//! PID capture strategy per spike 05: poll
//! `/proc/<nsenter_pid>/task/*/children` with 5×10ms backoff.
//! Worst observed on Linux: 12ms.

use std::path::Path;
use std::time::Duration;

use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, warn};

use open_sandbox_agent::container::{
    EXEC_CHANNEL_CAPACITY, ExecExitInfo, ExecHandle, ExecStart, detect_command_not_found,
};
use open_sandbox_contracts::error::AgentError;

use libcontainer::container::Container;

const PID_CAPTURE_ATTEMPTS: usize = 5;
const PID_CAPTURE_INTERVAL: Duration = Duration::from_millis(10);

fn container_pid(container_id: &str, state_dir: &Path) -> Result<i32, AgentError> {
    let container_root = state_dir.join(container_id);
    let container = Container::load(container_root).map_err(|e| AgentError::Runtime {
        detail: format!("failed to load container: {e}"),
    })?;
    let pid = container.pid().ok_or_else(|| AgentError::Runtime {
        detail: "container has no PID".into(),
    })?;
    Ok(pid.as_raw())
}

pub async fn start_exec_streaming(
    container_id: &str,
    state_dir: &Path,
    start: ExecStart,
) -> Result<ExecHandle, AgentError> {
    let target_pid = container_pid(container_id, state_dir)?;

    let exec_id = format!("youki-{}", uuid::Uuid::new_v4().simple());

    let mut cmd = Command::new("nsenter");
    cmd.arg("--target")
        .arg(target_pid.to_string())
        .arg("--mount")
        .arg("--uts")
        .arg("--ipc")
        .arg("--net")
        .arg("--pid");
    if !start.cwd.is_empty() {
        cmd.arg(format!("--wd={}", start.cwd));
    }
    for (k, v) in &start.env {
        cmd.env(k, v);
    }
    cmd.arg("--").args(&start.command);
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| AgentError::Runtime {
        detail: format!("failed to spawn nsenter: {e}"),
    })?;
    let nsenter_pid = child.id().ok_or_else(|| AgentError::Runtime {
        detail: "spawned nsenter has no PID".into(),
    })? as i32;

    // Capture in-container PID per spike 05.
    let mut in_container_pid = 0;
    for _ in 0..PID_CAPTURE_ATTEMPTS {
        if let Some(pid) = read_first_child(nsenter_pid) {
            in_container_pid = pid;
            break;
        }
        tokio::time::sleep(PID_CAPTURE_INTERVAL).await;
    }

    let mut stdin = child.stdin.take().ok_or_else(|| AgentError::Runtime {
        detail: "nsenter stdin not available".into(),
    })?;
    let mut stdout = child.stdout.take().ok_or_else(|| AgentError::Runtime {
        detail: "nsenter stdout not available".into(),
    })?;
    let mut stderr = child.stderr.take().ok_or_else(|| AgentError::Runtime {
        detail: "nsenter stderr not available".into(),
    })?;

    let (stdin_tx, mut stdin_rx) = mpsc::channel::<Bytes>(EXEC_CHANNEL_CAPACITY);
    let (stdout_tx, stdout_rx) = mpsc::channel::<Bytes>(EXEC_CHANNEL_CAPACITY);
    let (stderr_tx, stderr_rx) = mpsc::channel::<Bytes>(EXEC_CHANNEL_CAPACITY);
    let (exited_tx, exited_rx) = oneshot::channel::<ExecExitInfo>();

    // stdin pump.
    tokio::spawn(async move {
        while let Some(bytes) = stdin_rx.recv().await {
            if stdin.write_all(&bytes).await.is_err() {
                return;
            }
        }
        let _ = stdin.shutdown().await;
    });

    // stdout pump.
    let stdout_tx_clone = stdout_tx.clone();
    let mut stdout_for_cnf: Vec<u8> = Vec::new();
    tokio::spawn(async move {
        let mut buf = [0u8; 64 * 1024];
        loop {
            match stdout.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if stdout_for_cnf.len() < 4096 {
                        stdout_for_cnf.extend_from_slice(&buf[..n]);
                    }
                    if stdout_tx_clone
                        .send(Bytes::copy_from_slice(&buf[..n]))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    });

    // stderr pump.
    let stderr_tx_clone = stderr_tx.clone();
    let mut stderr_for_cnf: Vec<u8> = Vec::new();
    tokio::spawn(async move {
        let mut buf = [0u8; 64 * 1024];
        loop {
            match stderr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if stderr_for_cnf.len() < 4096 {
                        stderr_for_cnf.extend_from_slice(&buf[..n]);
                    }
                    if stderr_tx_clone
                        .send(Bytes::copy_from_slice(&buf[..n]))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    });

    // exit watcher.
    tokio::spawn(async move {
        let status = child.wait().await;
        let exit_code = status
            .ok()
            .and_then(|s| s.code())
            .unwrap_or(-1);
        // Drop the tx clones so the pump tasks exit cleanly before
        // we send the exit info. (The actual draining is the
        // responsibility of io_stream.rs which awaits the pump
        // tasks before emitting Exited.)
        drop(stdout_tx);
        drop(stderr_tx);

        // Detect command-not-found per the shared heuristic.
        let cnf = exit_code == 127
            && (detect_command_not_found(&stderr_for_cnf)
                || detect_command_not_found(&stdout_for_cnf));

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

pub async fn signal_in_container(
    container_id: &str,
    state_dir: &Path,
    in_container_pid: i32,
    signum: i32,
) -> Result<(), AgentError> {
    let target_pid = container_pid(container_id, state_dir)?;

    // Issue `nsenter ... -- kill -<signum> <pid>` to deliver the
    // signal inside the container's PID namespace. The youki spike
    // recommended setns+kill(2) syscalls as more structurally pure;
    // we use nsenter for v1.0 to mirror the docker backend's shape.
    // Migration to direct syscalls is a v1.1 optimisation.
    let status = Command::new("nsenter")
        .arg("--target")
        .arg(target_pid.to_string())
        .arg("--mount")
        .arg("--pid")
        .arg("--")
        .arg("kill")
        .arg(format!("-{signum}"))
        .arg(in_container_pid.to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map_err(|e| AgentError::Runtime {
            detail: format!("nsenter kill spawn failed: {e}"),
        })?;
    if !status.success() {
        // ESRCH (no such process) — typically means the target
        // already exited. Treat as success.
        debug!(
            in_container_pid,
            signum,
            "nsenter kill returned non-zero (process likely already exited)"
        );
    }
    Ok(())
}

fn read_first_child(nsenter_pid: i32) -> Option<i32> {
    let pattern = format!("/proc/{nsenter_pid}/task");
    let entries = std::fs::read_dir(&pattern).ok()?;
    for entry in entries.flatten() {
        let children_path = entry.path().join("children");
        if let Ok(content) = std::fs::read_to_string(&children_path) {
            for token in content.split_whitespace() {
                if let Ok(pid) = token.parse::<i32>() {
                    return Some(pid);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_first_child_handles_missing_proc() {
        // PID 0 doesn't have /proc/0/task — should return None.
        assert!(read_first_child(0).is_none());
    }
}
