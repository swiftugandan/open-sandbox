//! Streaming exec for the youki runtime.
//!
//! Spawns `nsenter` into the target container's namespaces and pumps
//! stdin/stdout/stderr through tokio channels. Per spike 02, the
//! in-namespace child does NOT die when the host-side nsenter is
//! killed — so the agent must capture the in-container PID at start
//! and explicitly issue signals on stream close via
//! `signal_in_container` (used by the ExecRegistry cleanup hook).
//!
//! In-container PID capture: the wrapped command emits
//! `OPENSB_INPID=<pid>\n` on stderr before exec'ing the user's
//! command (see `container::wrap_command_with_inpid_marker`). The
//! stderr pump strips and forwards it. Replaces the v0.7-era
//! `/proc/<nsenter_pid>/task/*/children` walk, which captured the
//! HOST pid — wrong for kill inside the container's PID namespace.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};
use tracing::debug;

use open_sandbox_agent::container::{
    EXEC_CHANNEL_CAPACITY, ExecExitInfo, ExecHandle, ExecStart, consume_inpid_marker,
    detect_command_not_found, wrap_command_with_inpid_marker,
};
use open_sandbox_contracts::error::AgentError;

use libcontainer::container::Container;

pub(crate) fn container_pid(container_id: &str, state_dir: &Path) -> Result<i32, AgentError> {
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

    // exec_id is an opaque, runtime-internal token. Use the same
    // hex shape as the docker runtime (which forwards docker's
    // 64-char exec id) so clients can't tell the two runtimes
    // apart from this field.
    let exec_id = uuid::Uuid::new_v4().simple().to_string();

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
    // Wrap so the in-container process self-reports its
    // namespace-local PID. nsenter's child pid (host PID) is not
    // valid inside the container's PID namespace where signals
    // must be delivered. See container::wrap_command_with_inpid_marker.
    let wrapped = wrap_command_with_inpid_marker(start.command);
    cmd.arg("--").args(&wrapped);
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| AgentError::Runtime {
        detail: format!("failed to spawn nsenter: {e}"),
    })?;
    let nsenter_pid = child.id().ok_or_else(|| AgentError::Runtime {
        detail: "spawned nsenter has no PID".into(),
    })? as i32;

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
    let (inpid_tx, inpid_rx) = oneshot::channel::<i32>();

    // Shared cnf-sniff buffers between the pump tasks (which read
    // the first ~4 KiB into them) and the exit watcher (which
    // applies `detect_command_not_found` after the child exits).
    let stdout_cnf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let stderr_cnf = Arc::new(Mutex::new(Vec::<u8>::new()));

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
    let stdout_cnf_pump = stdout_cnf.clone();
    let stdout_pump = tokio::spawn(async move {
        let mut buf = [0u8; 64 * 1024];
        loop {
            match stdout.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    {
                        let mut g = stdout_cnf_pump.lock().unwrap();
                        if g.len() < 4096 {
                            g.extend_from_slice(&buf[..n]);
                        }
                    }
                    if stdout_tx
                        .send(Bytes::copy_from_slice(&buf[..n]))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
        drop(stdout_tx);
    });

    // stderr pump. Strips the leading `OPENSB_INPID=<n>\n` marker
    // (emitted by the shell wrapper) and forwards the pid via
    // `inpid_tx`; subsequent bytes pass through unchanged.
    let stderr_cnf_pump = stderr_cnf.clone();
    let stderr_pump = tokio::spawn(async move {
        let mut buf = [0u8; 64 * 1024];
        let mut inpid_scan_buf: Vec<u8> = Vec::new();
        let mut inpid_done = false;
        let mut inpid_tx_slot = Some(inpid_tx);
        loop {
            match stderr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let bytes = &buf[..n];
                    if !inpid_done {
                        inpid_scan_buf.extend_from_slice(bytes);
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
                                {
                                    let mut g = stderr_cnf_pump.lock().unwrap();
                                    if g.len() < 4096 {
                                        g.extend_from_slice(&rest);
                                    }
                                }
                                if stderr_tx.send(Bytes::from(rest)).await.is_err() {
                                    break;
                                }
                            }
                            Ok(None) => continue,
                            Err(()) => {
                                inpid_tx_slot.take();
                                inpid_done = true;
                                let rest = std::mem::take(&mut inpid_scan_buf);
                                {
                                    let mut g = stderr_cnf_pump.lock().unwrap();
                                    if g.len() < 4096 {
                                        g.extend_from_slice(&rest);
                                    }
                                }
                                if stderr_tx.send(Bytes::from(rest)).await.is_err() {
                                    break;
                                }
                            }
                        }
                    } else {
                        {
                            let mut g = stderr_cnf_pump.lock().unwrap();
                            if g.len() < 4096 {
                                g.extend_from_slice(bytes);
                            }
                        }
                        if stderr_tx.send(Bytes::copy_from_slice(bytes)).await.is_err() {
                            break;
                        }
                    }
                }
            }
        }
        drop(inpid_tx_slot);
        drop(stderr_tx);
    });

    // exit watcher. Awaits the child + both pumps so the cnf
    // buffers are fully populated before we read them.
    tokio::spawn(async move {
        let status = child.wait().await;
        let exit_code = status.ok().and_then(|s| s.code()).unwrap_or(-1);
        let _ = stdout_pump.await;
        let _ = stderr_pump.await;

        let cnf = if exit_code == 127 {
            let stderr_g = stderr_cnf.lock().unwrap();
            let stdout_g = stdout_cnf.lock().unwrap();
            detect_command_not_found(&stderr_g) || detect_command_not_found(&stdout_g)
        } else {
            false
        };

        let _ = exited_tx.send(ExecExitInfo {
            exit_code,
            command_not_found: cnf,
        });
    });

    // Wait briefly for the wrapper to emit OPENSB_INPID on stderr.
    let in_container_pid = tokio::time::timeout(Duration::from_secs(1), inpid_rx)
        .await
        .ok()
        .and_then(|r| r.ok())
        .unwrap_or(0);

    debug!(
        exec_id = %exec_id,
        nsenter_pid = nsenter_pid,
        in_container_pid = in_container_pid,
        "youki exec started"
    );

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
            signum, "nsenter kill returned non-zero (process likely already exited)"
        );
    }
    Ok(())
}
