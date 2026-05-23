//! `drive_io_session` — wires a single sandbox I/O stream between
//! the proxy (gateway-originated) and the runtime (docker / youki).
//!
//! Responsibilities:
//!   - Read the first client frame (must be IoStart) and dispatch
//!     to runtime.start_exec or runtime.read_file based on the
//!     IoStart.params variant.
//!   - For exec sessions: register in ExecRegistry (so the
//!     stream-close cleanup hook can SIGTERM/SIGKILL on disconnect
//!     per spikes 01+02), then pump stdin/stdout/stderr/signal
//!     frames in both directions until exit or close.
//!   - For read_file: emit the file bytes as stdout frames, then a
//!     terminal IoExited.
//!   - On terminal events, emit the right server frame
//!     (`IoStarted` first, then `IoExited` on success, or
//!     `IoError` on failure) and call `on_stream_closed` to
//!     clean up registry + signals.
//!
//! Backpressure: bounded mpsc channels at every link; per spike
//! 04, this propagates the slow-client backpressure all the way
//! back to the in-container process.

use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use tokio::sync::mpsc;
use tokio_stream::{Stream, StreamExt};
use tracing::{info, warn};

use open_sandbox_contracts::constants::EXEC_KILL_GRACE;
use open_sandbox_contracts::error::AgentError;
use open_sandbox_contracts::proxy::{
    IoClientFrame, IoClose, IoError as IoErrorMsg, IoExited, IoServerFrame, IoSignal, IoStarted,
    io_client_frame, io_server_frame, io_start,
};
use open_sandbox_contracts::types::SandboxId;

use crate::container::{ContainerId, ContainerRuntime, ExecStart};
use crate::exec_registry::{ExecRecord, ExecRegistry, on_stream_closed};

/// 64 KiB chunk size for streaming read_file responses. Same order
/// of magnitude as bollard's typical chunk and the WS framing budget.
const READ_FILE_CHUNK_BYTES: usize = 64 * 1024;

#[allow(clippy::too_many_arguments)]
pub async fn drive_io_session<R, S>(
    runtime: Arc<R>,
    registry: Arc<ExecRegistry>,
    stream_id: String,
    sandbox_id: SandboxId,
    container_id: ContainerId,
    mut client_frames: S,
    server_tx: mpsc::Sender<IoServerFrame>,
) where
    R: ContainerRuntime + Send + Sync + 'static,
    S: Stream<Item = Result<IoClientFrame, AgentError>> + Unpin + Send + 'static,
{
    // 1. Receive IoStart (must be the first client frame).
    let start = match client_frames.next().await {
        Some(Ok(IoClientFrame {
            payload: Some(io_client_frame::Payload::Start(s)),
            ..
        })) => s,
        Some(Ok(_)) => {
            send_error(
                &server_tx,
                &stream_id,
                "INVALID_REQUEST",
                "first frame must be IoStart",
            )
            .await;
            return;
        }
        Some(Err(e)) => {
            send_error(&server_tx, &stream_id, "IO_STREAM_FAILED", &e.to_string()).await;
            return;
        }
        None => return, // client disconnected before sending IoStart
    };

    // 2. Dispatch based on op variant.
    match start.params {
        Some(io_start::Params::Exec(params)) => {
            drive_exec(
                runtime,
                registry,
                stream_id,
                sandbox_id,
                container_id,
                params,
                client_frames,
                server_tx,
            )
            .await;
        }
        Some(io_start::Params::ReadFile(params)) => {
            drive_read_file(runtime, stream_id, container_id, params, server_tx).await;
        }
        None => {
            send_error(
                &server_tx,
                &stream_id,
                "INVALID_REQUEST",
                "IoStart.params must be set",
            )
            .await;
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn drive_exec<R, S>(
    runtime: Arc<R>,
    registry: Arc<ExecRegistry>,
    stream_id: String,
    sandbox_id: SandboxId,
    container_id: ContainerId,
    params: open_sandbox_contracts::proxy::ExecParams,
    client_frames: S,
    server_tx: mpsc::Sender<IoServerFrame>,
) where
    R: ContainerRuntime + Send + Sync + 'static,
    S: Stream<Item = Result<IoClientFrame, AgentError>> + Unpin + Send + 'static,
{
    let exec_start = ExecStart {
        command: params.command,
        cwd: params.cwd,
        env: params.env,
    };

    info!(
        stream_id = %stream_id,
        sandbox_id = %sandbox_id,
        cmd.argv = exec_start.command.join(" "),
        cmd.cwd = %exec_start.cwd,
        "io_session.start op=exec"
    );

    let handle = match runtime.start_exec(&container_id, exec_start).await {
        Ok(h) => h,
        Err(e) => {
            send_error(&server_tx, &stream_id, "RUNTIME_ERROR", &e.to_string()).await;
            return;
        }
    };

    let exec_id = handle.exec_id.clone();
    let in_container_pid = handle.in_container_pid;

    info!(
        stream_id = %stream_id,
        exec_id = %exec_id,
        in_container_pid,
        "io_session.exec_pid_captured"
    );

    // Register so stream-close cleanup can find the PID.
    registry.insert(ExecRecord {
        stream_id: stream_id.clone(),
        sandbox_id: sandbox_id.clone(),
        container_id: container_id.clone(),
        exec_id: exec_id.clone(),
        in_container_pid,
        started_at: Instant::now(),
    });

    // Emit IoStarted to the client.
    let started_frame = IoServerFrame {
        stream_id: stream_id.clone(),
        payload: Some(io_server_frame::Payload::Started(IoStarted {
            exec_id: exec_id.clone(),
            in_container_pid,
        })),
    };
    if server_tx.send(started_frame).await.is_err() {
        cleanup(&*runtime, &registry, &stream_id).await;
        return;
    }

    pump_exec_session(
        runtime.clone(),
        registry.clone(),
        stream_id.clone(),
        container_id.clone(),
        in_container_pid,
        handle,
        client_frames,
        server_tx,
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn pump_exec_session<R, S>(
    runtime: Arc<R>,
    registry: Arc<ExecRegistry>,
    stream_id: String,
    container_id: ContainerId,
    in_container_pid: i32,
    handle: crate::container::ExecHandle,
    client_frames: S,
    server_tx: mpsc::Sender<IoServerFrame>,
) where
    R: ContainerRuntime + Send + Sync + 'static,
    S: Stream<Item = Result<IoClientFrame, AgentError>> + Unpin + Send + 'static,
{
    let crate::container::ExecHandle {
        stdin,
        stdout,
        stderr,
        exited,
        ..
    } = handle;

    // Internal channels for demux of client frames.
    let (signal_tx, mut signal_rx) = mpsc::channel::<IoSignal>(4);
    let (close_tx, mut close_rx) = mpsc::channel::<IoClose>(1);

    // Client-frames demux task: routes Stdin to handle.stdin,
    // Signal to signal_rx, Close to close_rx. Duplicate Start is a
    // protocol error and ends the session.
    let demux_handle = tokio::spawn(client_frames_demux(
        client_frames,
        stdin,
        signal_tx,
        close_tx,
        stream_id.clone(),
    ));

    // Output pump tasks: handle.stdout/stderr → server_tx.
    let stdout_handle = tokio::spawn(output_pump(
        stdout,
        server_tx.clone(),
        stream_id.clone(),
        OutputKind::Stdout,
    ));
    let stderr_handle = tokio::spawn(output_pump(
        stderr,
        server_tx.clone(),
        stream_id.clone(),
        OutputKind::Stderr,
    ));

    // Main control loop: wait for exit or signal or close.
    let mut exited_pinned = exited;
    let exit_outcome = loop {
        tokio::select! {
            info = &mut exited_pinned => break Some(info),
            Some(sig) = signal_rx.recv() => {
                if let Err(e) = runtime
                    .signal_exec(&container_id, in_container_pid, sig.signum as i32)
                    .await
                {
                    warn!(
                        stream_id = %stream_id,
                        in_container_pid,
                        signum = sig.signum,
                        error = %e,
                        "signal_exec failed"
                    );
                }
            }
            Some(_close) = close_rx.recv() => {
                // Client requested end-of-session. Fire the cleanup
                // hook (SIGTERM + SIGKILL after grace) and stop.
                break None;
            }
        }
    };

    // Stop demux (signal/close/stdin source) — once exec has
    // terminated, no more client frames are meaningful.
    demux_handle.abort();

    let terminal_frame = match exit_outcome {
        Some(Ok(info)) => {
            // Drain output pumps before emitting Exited so any
            // buffered stdout/stderr bytes reach the client first.
            // Runtime backend drops stdout_tx/stderr_tx on process
            // exit, so the pumps return cleanly here.
            let _ = stdout_handle.await;
            let _ = stderr_handle.await;
            info!(
                stream_id = %stream_id,
                exit_code = info.exit_code,
                command_not_found = info.command_not_found,
                "exec_registry.exec_exited"
            );
            IoServerFrame {
                stream_id: stream_id.clone(),
                payload: Some(io_server_frame::Payload::Exited(IoExited {
                    exit_code: info.exit_code,
                    command_not_found: info.command_not_found,
                })),
            }
        }
        Some(Err(_)) => {
            // Runtime dropped the sender without sending — treat as
            // an internal runtime error.
            stdout_handle.abort();
            stderr_handle.abort();
            IoServerFrame {
                stream_id: stream_id.clone(),
                payload: Some(io_server_frame::Payload::Error(IoErrorMsg {
                    code: "RUNTIME_ERROR".into(),
                    detail: "runtime exited without sending exit info".into(),
                })),
            }
        }
        None => {
            // Close from client. Emit nothing; the cleanup hook will
            // SIGTERM/SIGKILL the in-container PID.
            cleanup(&*runtime, &registry, &stream_id).await;
            stdout_handle.abort();
            stderr_handle.abort();
            return;
        }
    };

    let _ = server_tx.send(terminal_frame).await;
    cleanup(&*runtime, &registry, &stream_id).await;
}

#[derive(Copy, Clone)]
enum OutputKind {
    Stdout,
    Stderr,
}

async fn output_pump(
    mut rx: mpsc::Receiver<Bytes>,
    server_tx: mpsc::Sender<IoServerFrame>,
    stream_id: String,
    kind: OutputKind,
) {
    while let Some(bytes) = rx.recv().await {
        let frame = IoServerFrame {
            stream_id: stream_id.clone(),
            payload: Some(match kind {
                OutputKind::Stdout => io_server_frame::Payload::Stdout(bytes.to_vec()),
                OutputKind::Stderr => io_server_frame::Payload::Stderr(bytes.to_vec()),
            }),
        };
        if server_tx.send(frame).await.is_err() {
            return;
        }
    }
}

async fn client_frames_demux<S>(
    mut client_frames: S,
    stdin: mpsc::Sender<Bytes>,
    signal_tx: mpsc::Sender<IoSignal>,
    close_tx: mpsc::Sender<IoClose>,
    stream_id: String,
) where
    S: Stream<Item = Result<IoClientFrame, AgentError>> + Unpin + Send + 'static,
{
    while let Some(frame) = client_frames.next().await {
        let Ok(IoClientFrame {
            payload: Some(p), ..
        }) = frame
        else {
            return;
        };
        match p {
            io_client_frame::Payload::Stdin(bytes) => {
                if stdin.send(Bytes::from(bytes)).await.is_err() {
                    return;
                }
            }
            io_client_frame::Payload::Signal(s) => {
                let _ = signal_tx.send(s).await;
            }
            io_client_frame::Payload::Close(c) => {
                if c.stdin_eof {
                    // Half-close stdin only — drop the sender side to
                    // signal EOF to the in-container process. Session
                    // continues; the process may still emit output.
                    drop(stdin);
                    return;
                }
                let _ = close_tx.send(c).await;
                return;
            }
            io_client_frame::Payload::Start(_) => {
                // Duplicate Start is a protocol error. Treat as
                // disconnect (the main loop will clean up).
                warn!(stream_id = %stream_id, "duplicate IoStart received; ending session");
                return;
            }
        }
    }
    // Stream ended without a Close. The main loop will detect this
    // when stdin drops (process gets EOF) and the process eventually
    // exits, OR via the close_rx not firing — in which case the
    // session terminates only when the runtime sends IoExited.
    drop(stdin);
}

async fn drive_read_file<R>(
    runtime: Arc<R>,
    stream_id: String,
    container_id: ContainerId,
    params: open_sandbox_contracts::proxy::ReadFileParams,
    server_tx: mpsc::Sender<IoServerFrame>,
) where
    R: ContainerRuntime,
{
    let cwd = if params.cwd.is_empty() {
        None
    } else {
        Some(params.cwd.as_str())
    };

    info!(
        stream_id = %stream_id,
        path = %params.path,
        cwd = ?cwd,
        "io_session.start op=read_file"
    );

    match runtime.read_file(&container_id, &params.path, cwd).await {
        Ok(bytes) => {
            // Chunk into stdout frames.
            for chunk in bytes.chunks(READ_FILE_CHUNK_BYTES) {
                let frame = IoServerFrame {
                    stream_id: stream_id.clone(),
                    payload: Some(io_server_frame::Payload::Stdout(chunk.to_vec())),
                };
                if server_tx.send(frame).await.is_err() {
                    return;
                }
            }
            let exited = IoServerFrame {
                stream_id: stream_id.clone(),
                payload: Some(io_server_frame::Payload::Exited(IoExited {
                    exit_code: 0,
                    command_not_found: false,
                })),
            };
            let _ = server_tx.send(exited).await;
        }
        Err(e) => {
            let code = match &e {
                AgentError::Runtime { detail } if detail.contains("No such file") => "FILE_NOT_FOUND",
                _ => "READ_FAILED",
            };
            send_error(&server_tx, &stream_id, code, &e.to_string()).await;
        }
    }
}

async fn send_error(
    server_tx: &mpsc::Sender<IoServerFrame>,
    stream_id: &str,
    code: &str,
    detail: &str,
) {
    let frame = IoServerFrame {
        stream_id: stream_id.to_string(),
        payload: Some(io_server_frame::Payload::Error(IoErrorMsg {
            code: code.to_string(),
            detail: detail.to_string(),
        })),
    };
    let _ = server_tx.send(frame).await;
}

async fn cleanup<R: ContainerRuntime>(runtime: &R, registry: &ExecRegistry, stream_id: &str) {
    on_stream_closed(runtime, registry, stream_id, EXEC_KILL_GRACE).await;
}

// =====================================================================
// Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use open_sandbox_contracts::proxy::{
        ExecParams, IoClose, IoSignal, IoStart, ReadFileParams, io_client_frame, io_server_frame,
    };
    use tokio_stream::wrappers::ReceiverStream;

    use crate::testutil::MockContainerRuntime;

    /// Build (client_tx, server_rx, drive_handle).
    /// drive_handle is the spawned drive_io_session task.
    fn spawn_session<R: ContainerRuntime + Send + Sync + 'static>(
        runtime: Arc<R>,
        registry: Arc<ExecRegistry>,
        stream_id: &str,
    ) -> (
        mpsc::Sender<Result<IoClientFrame, AgentError>>,
        mpsc::Receiver<IoServerFrame>,
        tokio::task::JoinHandle<()>,
    ) {
        let (client_tx, client_rx) = mpsc::channel::<Result<IoClientFrame, AgentError>>(8);
        let (server_tx, server_rx) = mpsc::channel::<IoServerFrame>(64);
        let client_stream = ReceiverStream::new(client_rx);
        let sandbox_id = SandboxId::new();
        let container_id = ContainerId(format!("mock-{sandbox_id}"));
        let sid = stream_id.to_string();
        let handle = tokio::spawn(async move {
            drive_io_session(
                runtime,
                registry,
                sid,
                sandbox_id,
                container_id,
                client_stream,
                server_tx,
            )
            .await;
        });
        (client_tx, server_rx, handle)
    }

    async fn collect_until_exit(
        mut rx: mpsc::Receiver<IoServerFrame>,
    ) -> Vec<io_server_frame::Payload> {
        let mut out = Vec::new();
        while let Some(frame) = rx.recv().await {
            if let Some(p) = frame.payload {
                let is_terminal = matches!(
                    p,
                    io_server_frame::Payload::Exited(_) | io_server_frame::Payload::Error(_)
                );
                out.push(p);
                if is_terminal {
                    break;
                }
            }
        }
        out
    }

    fn iostart_exec(command: Vec<&str>) -> IoClientFrame {
        IoClientFrame {
            stream_id: "test-stream".into(),
            payload: Some(io_client_frame::Payload::Start(IoStart {
                sandbox_id: "ignored-by-mock".into(),
                params: Some(open_sandbox_contracts::proxy::io_start::Params::Exec(
                    ExecParams {
                        command: command.into_iter().map(String::from).collect(),
                        cwd: String::new(),
                        env: HashMap::new(),
                    },
                )),
            })),
        }
    }

    use std::collections::HashMap;

    #[tokio::test]
    async fn exec_runs_echo() {
        let runtime = Arc::new(MockContainerRuntime::new());
        let registry = Arc::new(ExecRegistry::new());
        let (tx, rx, h) = spawn_session(runtime.clone(), registry.clone(), "s1");

        tx.send(Ok(iostart_exec(vec!["echo", "hello"]))).await.unwrap();
        drop(tx);

        let frames = collect_until_exit(rx).await;
        // Expect: Started, Stdout("hello\n"), Exited(0)
        assert!(matches!(frames.first(), Some(io_server_frame::Payload::Started(_))));
        let stdout_chunks: Vec<_> = frames
            .iter()
            .filter_map(|p| match p {
                io_server_frame::Payload::Stdout(b) => Some(b.clone()),
                _ => None,
            })
            .collect();
        let joined: Vec<u8> = stdout_chunks.into_iter().flatten().collect();
        assert_eq!(joined, b"hello\n");
        assert!(matches!(
            frames.last(),
            Some(io_server_frame::Payload::Exited(IoExited {
                exit_code: 0,
                command_not_found: false,
            }))
        ));
        h.await.unwrap();
        assert!(registry.is_empty(), "registry should be drained after exit");
    }

    #[tokio::test]
    async fn exec_streams_stdin() {
        let runtime = Arc::new(MockContainerRuntime::new());
        let registry = Arc::new(ExecRegistry::new());
        let (tx, rx, h) = spawn_session(runtime, registry, "s2");

        tx.send(Ok(iostart_exec(vec!["cat"]))).await.unwrap();
        tx.send(Ok(IoClientFrame {
            stream_id: "s2".into(),
            payload: Some(io_client_frame::Payload::Stdin(b"hi there".to_vec())),
        }))
        .await
        .unwrap();
        tx.send(Ok(IoClientFrame {
            stream_id: "s2".into(),
            payload: Some(io_client_frame::Payload::Close(IoClose { stdin_eof: true })),
        }))
        .await
        .unwrap();

        let frames = collect_until_exit(rx).await;
        let joined: Vec<u8> = frames
            .iter()
            .filter_map(|p| match p {
                io_server_frame::Payload::Stdout(b) => Some(b.clone()),
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(joined, b"hi there");
        h.await.unwrap();
    }

    #[tokio::test]
    async fn command_not_found_emits_exited_with_flag() {
        let runtime = Arc::new(MockContainerRuntime::new());
        let registry = Arc::new(ExecRegistry::new());
        let (tx, rx, h) = spawn_session(runtime, registry, "s3");

        tx.send(Ok(iostart_exec(vec!["definitely_not_a_binary"])))
            .await
            .unwrap();
        drop(tx);

        let frames = collect_until_exit(rx).await;
        let stderr_joined: Vec<u8> = frames
            .iter()
            .filter_map(|p| match p {
                io_server_frame::Payload::Stderr(b) => Some(b.clone()),
                _ => None,
            })
            .flatten()
            .collect();
        assert!(
            String::from_utf8_lossy(&stderr_joined).contains("executable file not found"),
            "stderr should carry OCI diagnostic, got: {:?}",
            String::from_utf8_lossy(&stderr_joined)
        );
        assert!(matches!(
            frames.last(),
            Some(io_server_frame::Payload::Exited(IoExited {
                exit_code: 127,
                command_not_found: true,
            }))
        ));
        h.await.unwrap();
    }

    #[tokio::test]
    async fn signal_frame_forwarded_to_runtime() {
        let runtime = Arc::new(MockContainerRuntime::new());
        let registry = Arc::new(ExecRegistry::new());
        let (tx, _rx, h) = spawn_session(runtime.clone(), registry, "s4");

        tx.send(Ok(iostart_exec(vec!["sleep", "30"]))).await.unwrap();
        // Give the runtime a moment to start the sleep.
        tokio::time::sleep(Duration::from_millis(50)).await;
        tx.send(Ok(IoClientFrame {
            stream_id: "s4".into(),
            payload: Some(io_client_frame::Payload::Signal(IoSignal { signum: 15 })),
        }))
        .await
        .unwrap();

        // Wait for the signal to register; the mock records it.
        for _ in 0..50 {
            if !runtime.signals_received().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let signals = runtime.signals_received();
        assert!(!signals.is_empty(), "runtime should have received a signal");
        assert_eq!(signals[0].signum, 15);

        // Close the session so the spawned task wraps up.
        drop(tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), h).await;
    }

    #[tokio::test]
    async fn client_disconnect_triggers_cleanup_signals() {
        let runtime = Arc::new(MockContainerRuntime::new());
        let registry = Arc::new(ExecRegistry::new());
        let (tx, mut rx, h) = spawn_session(runtime.clone(), registry.clone(), "s5");

        tx.send(Ok(iostart_exec(vec!["sleep", "30"]))).await.unwrap();
        // Wait for Started so the registry is populated.
        let started = rx.recv().await.unwrap();
        assert!(matches!(started.payload, Some(io_server_frame::Payload::Started(_))));
        assert_eq!(registry.len(), 1, "registry should have one entry");

        // Send Close { stdin_eof: false } to end the session.
        tx.send(Ok(IoClientFrame {
            stream_id: "s5".into(),
            payload: Some(io_client_frame::Payload::Close(IoClose { stdin_eof: false })),
        }))
        .await
        .unwrap();

        // Cleanup runs; signal_exec called with SIGTERM then (after
        // grace) SIGKILL.
        let _ = tokio::time::timeout(Duration::from_secs(10), h).await;

        let signals = runtime.signals_received();
        let signums: Vec<i32> = signals.iter().map(|s| s.signum).collect();
        assert!(signums.contains(&15), "SIGTERM should have been sent");
        assert!(signums.contains(&9), "SIGKILL should have been sent");
        assert!(registry.is_empty(), "registry should be drained after cleanup");
    }

    #[tokio::test]
    async fn read_file_returns_contents() {
        let runtime = Arc::new(
            MockContainerRuntime::new().with_file("/home/data.txt", "file body"),
        );
        let registry = Arc::new(ExecRegistry::new());
        let (tx, rx, h) = spawn_session(runtime, registry, "s6");

        tx.send(Ok(IoClientFrame {
            stream_id: "s6".into(),
            payload: Some(io_client_frame::Payload::Start(IoStart {
                sandbox_id: "ignored".into(),
                params: Some(open_sandbox_contracts::proxy::io_start::Params::ReadFile(
                    ReadFileParams {
                        path: "data.txt".into(),
                        cwd: "/home".into(),
                    },
                )),
            })),
        }))
        .await
        .unwrap();
        drop(tx);

        let frames = collect_until_exit(rx).await;
        let joined: Vec<u8> = frames
            .iter()
            .filter_map(|p| match p {
                io_server_frame::Payload::Stdout(b) => Some(b.clone()),
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(joined, b"file body");
        assert!(matches!(
            frames.last(),
            Some(io_server_frame::Payload::Exited(IoExited {
                exit_code: 0,
                command_not_found: false,
            }))
        ));
        h.await.unwrap();
    }

    #[tokio::test]
    async fn read_file_missing_emits_resolved_path_in_error() {
        let runtime = Arc::new(MockContainerRuntime::new());
        let registry = Arc::new(ExecRegistry::new());
        let (tx, rx, h) = spawn_session(runtime, registry, "s7");

        tx.send(Ok(IoClientFrame {
            stream_id: "s7".into(),
            payload: Some(io_client_frame::Payload::Start(IoStart {
                sandbox_id: "ignored".into(),
                params: Some(open_sandbox_contracts::proxy::io_start::Params::ReadFile(
                    ReadFileParams {
                        path: "nope.py".into(),
                        cwd: "/home".into(),
                    },
                )),
            })),
        }))
        .await
        .unwrap();
        drop(tx);

        let frames = collect_until_exit(rx).await;
        let last = frames.last().expect("at least one frame");
        match last {
            io_server_frame::Payload::Error(IoErrorMsg { code, detail }) => {
                assert_eq!(code, "FILE_NOT_FOUND");
                assert!(
                    detail.contains("/home/nope.py"),
                    "error detail should include resolved path; got {detail:?}"
                );
            }
            other => panic!("expected Error frame, got {other:?}"),
        }
        h.await.unwrap();
    }

    #[tokio::test]
    async fn duplicate_iostart_ends_session_cleanly() {
        let runtime = Arc::new(MockContainerRuntime::new());
        let registry = Arc::new(ExecRegistry::new());
        let (tx, mut rx, h) = spawn_session(runtime.clone(), registry.clone(), "s8");

        tx.send(Ok(iostart_exec(vec!["sleep", "30"]))).await.unwrap();
        let _started = rx.recv().await.unwrap();
        // Now send a second IoStart — protocol error.
        tx.send(Ok(iostart_exec(vec!["echo", "no"]))).await.unwrap();

        // Cleanup should still fire.
        let _ = tokio::time::timeout(Duration::from_secs(10), h).await;
        let signals = runtime.signals_received();
        assert!(!signals.is_empty(), "cleanup should have fired");
    }
}
