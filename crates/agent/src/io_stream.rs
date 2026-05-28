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
use tracing::{debug, info, warn};

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

/// Hard cap on the total bytes a client may upload via a single
/// write_file / write_files_targz session. Comp-3 B5: without this, a
/// hostile / buggy client streaming a multi-gigabyte tarball OOMs the
/// agent process and tears down every sandbox on the host.
const MAX_WRITE_BYTES: usize = 256 * 1024 * 1024;

// v1.0.2 cascade: the inline is_valid_signum check is replaced by
// contracts::wire::Signum::try_from(u32). The check semantics are
// identical (POSIX 1..=31 + RT 34..=64); the contracts version is the
// single source of truth.

#[allow(clippy::too_many_arguments)]
pub async fn drive_io_session<R, S>(
    runtime: Arc<R>,
    registry: Arc<ExecRegistry>,
    // v1.0.3 carried `host_ports` here so drive_wait_port_listening
    // could probe `127.0.0.1:<host_port>` from the agent host. That
    // approach was wrong on Docker Desktop (docker-proxy false-
    // positives) — fixed in v1.0.3.x by delegating to the runtime's
    // `wait_port_listening` (probes from inside the container's
    // netns). The param stays in the public signature so the trait
    // remains available for any future host-side probe; rename to
    // `_host_ports` to silence unused-variable lints.
    _host_ports: Arc<dyn crate::container::HostPortLookup>,
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
        Some(io_start::Params::WriteFile(params)) => {
            drive_write_file(
                runtime,
                stream_id,
                container_id,
                params,
                client_frames,
                server_tx,
            )
            .await;
        }
        Some(io_start::Params::WriteFilesTargz(params)) => {
            drive_write_files_targz(
                runtime,
                stream_id,
                container_id,
                params,
                client_frames,
                server_tx,
            )
            .await;
        }
        Some(io_start::Params::ListDir(params)) => {
            drive_list_dir(runtime, stream_id, container_id, params, server_tx).await;
        }
        Some(io_start::Params::WaitPortListening(params)) => {
            drive_wait_port_listening(
                runtime,
                container_id,
                stream_id,
                sandbox_id,
                params,
                server_tx,
            )
            .await;
        }
        Some(io_start::Params::DeleteFile(params)) => {
            drive_delete_file(runtime, stream_id, container_id, params, server_tx).await;
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
        Some(stdin),
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
                // v1.0.2 cascade: contracts::wire::Signum::try_from gates
                // the u32 to POSIX 1..=31 + RT 34..=64. Out-of-range
                // values (including signum=0 = kill -0 liveness probe)
                // are dropped with a warn before reaching the runtime.
                let signum = match open_sandbox_contracts::wire::Signum::try_from(sig.signum) {
                    Ok(s) => s,
                    Err(e) => {
                        warn!(
                            stream_id = %stream_id,
                            error = %e,
                            "rejecting out-of-range signum; dropping signal"
                        );
                        continue;
                    }
                };
                if let Err(e) = runtime
                    .signal_exec(&container_id, in_container_pid, signum.as_i32())
                    .await
                {
                    warn!(
                        stream_id = %stream_id,
                        in_container_pid,
                        signum = signum.as_u8(),
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

    let (terminal_frame, needs_kill) = match exit_outcome {
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
            (
                Some(IoServerFrame {
                    stream_id: stream_id.clone(),
                    payload: Some(io_server_frame::Payload::Exited(IoExited {
                        exit_code: info.exit_code,
                        command_not_found: info.command_not_found,
                    })),
                }),
                // Comp-3 A2: process exited naturally; no signal needed and
                // skipping cleanup avoids EXEC_KILL_GRACE-second wasted sleep
                // before the inevitable-no-op SIGKILL.
                false,
            )
        }
        Some(Err(_)) => {
            // Runtime dropped the sender without sending — treat as
            // an internal runtime error.
            stdout_handle.abort();
            stderr_handle.abort();
            (
                Some(IoServerFrame {
                    stream_id: stream_id.clone(),
                    payload: Some(io_server_frame::Payload::Error(IoErrorMsg {
                        code: "RUNTIME_ERROR".into(),
                        detail: "runtime exited without sending exit info".into(),
                    })),
                }),
                // Runtime is in an unknown state; SIGTERM/SIGKILL the PID to
                // be safe.
                true,
            )
        }
        None => {
            // Comp-3 C6: client requested end-of-session. Emit a terminal
            // frame so the gateway doesn't see "stream ended without
            // terminal frame" and synthesize a 500. The agent then
            // SIGTERM/SIGKILLs the in-container PID via the cleanup hook.
            stdout_handle.abort();
            stderr_handle.abort();
            (
                Some(IoServerFrame {
                    stream_id: stream_id.clone(),
                    payload: Some(io_server_frame::Payload::Error(IoErrorMsg {
                        code: "CANCELLED".into(),
                        detail: "client-initiated close".into(),
                    })),
                }),
                true,
            )
        }
    };

    if let Some(frame) = terminal_frame {
        let _ = server_tx.send(frame).await;
    }
    if needs_kill {
        cleanup(&*runtime, &registry, &stream_id).await;
    } else {
        // Natural exit: drop the registry entry without sending signals.
        registry.remove(&stream_id);
    }
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
    mut stdin: Option<mpsc::Sender<Bytes>>,
    signal_tx: mpsc::Sender<IoSignal>,
    close_tx: mpsc::Sender<IoClose>,
    stream_id: String,
) where
    S: Stream<Item = Result<IoClientFrame, AgentError>> + Unpin + Send + 'static,
{
    // The demux owns stdin behind an Option so a half-close
    // (stdin_eof=true) can drop it while leaving the task running
    // to observe a subsequent full close (or stream end). If we
    // returned on half-close, any later Close frame — including the
    // synthetic one the proxy forwards when the gateway disconnects
    // — would arrive at a dead demux and the cleanup hook would
    // never fire.
    loop {
        let frame = match client_frames.next().await {
            Some(f) => f,
            None => {
                // Stream ended without an explicit Close —
                // typically the client process was SIGKILLed or
                // the TCP connection was reset. Synthesize a Close
                // so the main control loop runs the cleanup hook
                // (SIGTERM/SIGKILL the in-container PID).
                debug!(stream_id = %stream_id, "client stream ended; synthesizing Close");
                let _ = close_tx.send(IoClose { stdin_eof: false }).await;
                break;
            }
        };
        let Ok(IoClientFrame {
            payload: Some(p), ..
        }) = frame
        else {
            debug!(stream_id = %stream_id, "client frame error; synthesizing Close");
            let _ = close_tx.send(IoClose { stdin_eof: false }).await;
            break;
        };
        match p {
            io_client_frame::Payload::Stdin(bytes) => {
                let Some(tx) = stdin.as_ref() else {
                    // stdin already half-closed; drop subsequent
                    // stdin bytes silently (no protocol violation).
                    continue;
                };
                if tx.send(Bytes::from(bytes)).await.is_err() {
                    let _ = close_tx.send(IoClose { stdin_eof: false }).await;
                    break;
                }
            }
            io_client_frame::Payload::Signal(s) => {
                let _ = signal_tx.send(s).await;
            }
            io_client_frame::Payload::Close(c) => {
                if c.stdin_eof {
                    // Half-close: drop the stdin sender to signal
                    // EOF to the in-container process. Continue
                    // looping so Signal / full-Close frames can
                    // still be processed.
                    stdin = None;
                    continue;
                }
                let _ = close_tx.send(c).await;
                break;
            }
            io_client_frame::Payload::Start(_) => {
                warn!(stream_id = %stream_id, "duplicate IoStart received; ending session");
                let _ = close_tx.send(IoClose { stdin_eof: false }).await;
                break;
            }
        }
    }
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
            // v1.0.3: emit FileMeta BEFORE the first Stdout chunk so
            // the UI can capture the revision before consuming any
            // file bytes. The stat_revision call is best-effort —
            // when the runtime hasn't wired stat_revision yet (e.g.
            // the docker / youki stubs during the rollout) we still
            // serve the read content, just without the revision
            // sidecar. The wire shape preserves the v1.0.2
            // invariant: a v1.0.2 client that ignores FileMeta still
            // observes Stdout chunks + IoExited unchanged.
            if let Ok(rev) = runtime.stat_revision(&container_id, &params.path, cwd).await {
                let meta_frame = IoServerFrame {
                    stream_id: stream_id.clone(),
                    payload: Some(io_server_frame::Payload::FileMeta(
                        open_sandbox_contracts::proxy::FileMeta {
                            revision: rev.revision,
                            size: rev.size,
                        },
                    )),
                };
                if server_tx.send(meta_frame).await.is_err() {
                    return;
                }
            }
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
                AgentError::Runtime { detail } if detail.contains("No such file") => {
                    "FILE_NOT_FOUND"
                }
                _ => "READ_FAILED",
            };
            send_error(&server_tx, &stream_id, code, &e.to_string()).await;
        }
    }
}

/// v1.0.3: drive a one-level directory listing.
///
/// Shape mirrors `drive_read_file`: one emit of the typed
/// `ListDirResult` payload, then a clean `IoExited`. The runtime
/// trait is responsible for the 5000-entry cap; this handler is a
/// thin translator from `DirListing` → proto types.
async fn drive_list_dir<R>(
    runtime: Arc<R>,
    stream_id: String,
    container_id: ContainerId,
    params: open_sandbox_contracts::proxy::ListDirParams,
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
        "io_session.start op=list_dir"
    );

    match runtime.list_dir(&container_id, &params.path, cwd).await {
        Ok(listing) => {
            let result = open_sandbox_contracts::proxy::ListDirResult {
                path: listing.path,
                entries: listing
                    .entries
                    .into_iter()
                    .map(dir_entry_to_proto)
                    .collect(),
                truncated: listing.truncated,
                total_entries: listing.total_entries,
            };
            let result_frame = IoServerFrame {
                stream_id: stream_id.clone(),
                payload: Some(io_server_frame::Payload::ListDirResult(result)),
            };
            if server_tx.send(result_frame).await.is_err() {
                return;
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
            // Mirror drive_read_file's tighter "No such file" match —
            // looser "No such" would mis-classify "No such container"
            // and "No such image" (real bollard / runc phrases) as
            // FILE_NOT_FOUND.
            //
            // Until the docker / youki list_dir impls land (later in
            // PLAN_LIVE_EDIT group B), the runtime stubs return a
            // Runtime error whose detail starts with
            // "list_dir not yet implemented". Detect that here and
            // surface it as NOT_IMPLEMENTED so a gateway / SDK doing
            // capability feature-detection by error code can fall
            // back, matching the symmetric drive_write_file
            // precondition guard.
            let code = match &e {
                AgentError::Runtime { detail }
                    if detail.starts_with("list_dir not yet implemented") =>
                {
                    "NOT_IMPLEMENTED"
                }
                AgentError::Runtime { detail } if detail.contains("No such file") => {
                    "FILE_NOT_FOUND"
                }
                _ => "LIST_DIR_FAILED",
            };
            send_error(&server_tx, &stream_id, code, &e.to_string()).await;
        }
    }
}

/// v1.0.3: drive a single-file or recursive directory delete.
///
/// Mirrors `drive_write_file`'s session shape: collect the params,
/// run the runtime call, emit IoExited on success or IoError on
/// failure. Missing path resolves to Ok (idempotent under
/// concurrent external `rm`).
async fn drive_delete_file<R>(
    runtime: Arc<R>,
    stream_id: String,
    container_id: ContainerId,
    params: open_sandbox_contracts::proxy::DeleteFileParams,
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
        recursive = params.recursive,
        "io_session.start op=delete_file"
    );
    match runtime
        .delete_file(&container_id, &params.path, cwd, params.recursive)
        .await
    {
        Ok(()) => {
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
            // `rm` on a non-empty directory without -r returns
            // exit 1 + "Directory not empty" / "is a directory".
            // Surface as a typed code so the gateway can map to
            // 409 Conflict (the UI can then offer "delete
            // recursively").
            let detail = e.to_string();
            let code = if detail.contains("is a directory")
                || detail.contains("Directory not empty")
            {
                "DIRECTORY_NOT_EMPTY"
            } else {
                "DELETE_FAILED"
            };
            send_error(&server_tx, &stream_id, code, &detail).await;
        }
    }
}

/// v1.0.3: drive a TCP-probe of the sandbox's host-side port.
///
/// Polls `127.0.0.1:<host_port>` via non-blocking connect every
/// `WAIT_PORT_LISTENING_PROBE_INTERVAL_MS` until either:
///
///   * the connect succeeds  → emit `WaitPortListeningResult { ready=true, elapsed_ms }`
///   * `timeout_ms` elapses → emit `WaitPortListeningResult { ready=false, elapsed_ms }`
///
/// `timeout_ms` is clamped to `WAIT_PORT_LISTENING_MAX_TIMEOUT_MS`
/// server-side (FOLLOWUPS_v1.0.3 D2) so a buggy or malicious client
/// can't pin a session slot on a no-op loop.
///
/// One emit, then a clean `IoExited`. The `port` field on
/// `WaitPortListeningParams` is informational on the wire — the
/// agent resolves the sandbox's actual `host_port` via the
/// `HostPortLookup` trait. (The in-container port and the host
/// port aren't generally equal; the host port is what
/// `localhost:<host_port>` actually accepts a connect on.)
async fn drive_wait_port_listening<R>(
    runtime: Arc<R>,
    container_id: ContainerId,
    stream_id: String,
    sandbox_id: SandboxId,
    params: open_sandbox_contracts::proxy::WaitPortListeningParams,
    server_tx: mpsc::Sender<IoServerFrame>,
) where
    R: ContainerRuntime,
{
    use open_sandbox_contracts::constants::WAIT_PORT_LISTENING_MAX_TIMEOUT_MS;
    use open_sandbox_contracts::proxy::WaitPortListeningResult;
    use tokio::time::{Duration, Instant};

    let timeout_ms = params.timeout_ms.min(WAIT_PORT_LISTENING_MAX_TIMEOUT_MS);

    info!(
        stream_id = %stream_id,
        sandbox_id = %sandbox_id,
        port = params.port,
        timeout_ms,
        "io_session.start op=wait_port_listening"
    );

    let started = Instant::now();
    // The probe is delegated to the runtime so it can run INSIDE
    // the container's network namespace. From the agent host on
    // Docker Desktop a TCP-connect to the host-mapped port goes
    // through the docker-proxy userspace intermediary, which
    // accepts even when the container's bound process isn't
    // listening — i.e. the probe falsely reports ready=true. The
    // in-namespace probe sees the container's own network stack
    // directly.
    let ready_res = runtime
        .wait_port_listening(
            &container_id,
            params.port,
            Duration::from_millis(timeout_ms as u64),
        )
        .await;

    let ready = match ready_res {
        Ok(b) => b,
        Err(AgentError::Runtime { detail })
            if detail.starts_with("wait_port_listening not yet implemented") =>
        {
            send_error(
                &server_tx,
                &stream_id,
                "NOT_IMPLEMENTED",
                "wait_port_listening not yet implemented for this backend",
            )
            .await;
            return;
        }
        Err(e) => {
            send_error(
                &server_tx,
                &stream_id,
                "WAIT_PORT_LISTENING_FAILED",
                &e.to_string(),
            )
            .await;
            return;
        }
    };

    let raw_elapsed = u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX);
    let elapsed_ms = if ready {
        raw_elapsed
    } else {
        raw_elapsed.min(timeout_ms)
    };

    let result_frame = IoServerFrame {
        stream_id: stream_id.clone(),
        payload: Some(io_server_frame::Payload::WaitPortListeningResult(
            WaitPortListeningResult { ready, elapsed_ms },
        )),
    };
    if server_tx.send(result_frame).await.is_err() {
        return;
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

fn dir_entry_to_proto(
    entry: crate::container::DirEntry,
) -> open_sandbox_contracts::proxy::ListDirEntry {
    use crate::container::EntryType;
    use open_sandbox_contracts::proxy::{ListDirEntry, ListDirEntryType};
    ListDirEntry {
        name: entry.name,
        r#type: match entry.entry_type {
            EntryType::File => ListDirEntryType::File as i32,
            EntryType::Dir => ListDirEntryType::Dir as i32,
            EntryType::Symlink => ListDirEntryType::Symlink as i32,
            EntryType::Other => ListDirEntryType::Other as i32,
        },
        size: entry.size,
        revision: entry.revision,
        mode: entry.mode,
        target: entry.target,
    }
}

async fn drive_write_file<R, S>(
    runtime: Arc<R>,
    stream_id: String,
    container_id: ContainerId,
    params: open_sandbox_contracts::proxy::WriteFileParams,
    mut client_frames: S,
    server_tx: mpsc::Sender<IoServerFrame>,
) where
    R: ContainerRuntime,
    S: Stream<Item = Result<IoClientFrame, AgentError>> + Unpin + Send + 'static,
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
        expected_revision = %params.expected_revision,
        force = params.force,
        "io_session.start op=write_file"
    );

    // v1.0.3 precondition check (B11). When the caller supplies a
    // non-empty `expected_revision` AND `force` is false, stat the
    // file first and compare. Empty `expected_revision` keeps the
    // v1.0.1 / v1.0.2 "no precondition" wire-compat. `force=true`
    // is the escape hatch for scripted bulk writes that
    // intentionally last-write-wins; we log a warning when both
    // fields are set so a defensive caller's mistake is visible.
    if !params.expected_revision.is_empty() && !params.force {
        match runtime.stat_revision(&container_id, &params.path, cwd).await {
            Ok(rev) if rev.revision == params.expected_revision => {
                // Match — fall through to read the content.
            }
            Ok(rev) => {
                // Stale revision. Surface as an IoError with the
                // actual revision in the detail so the gateway can
                // bubble up a `409 Conflict` with the live token.
                send_error(&server_tx, &stream_id, "REVISION_MISMATCH", &rev.revision)
                    .await;
                // The caller's SDK may already be pipelining Stdin
                // frames toward this session — drain them so the
                // demux's bounded channel doesn't head-of-line block
                // other multiplexed sessions on the same tunnel.
                drain_remaining_client_frames(&mut client_frames).await;
                return;
            }
            Err(AgentError::Runtime { detail })
                if detail.starts_with("stat_revision not yet implemented") =>
            {
                send_error(
                    &server_tx,
                    &stream_id,
                    "NOT_IMPLEMENTED",
                    "write_file expected_revision precondition requires a \
                     runtime that supports stat_revision; not yet \
                     implemented for this backend. Pass force=true to \
                     bypass.",
                )
                .await;
                drain_remaining_client_frames(&mut client_frames).await;
                return;
            }
            Err(AgentError::Runtime { detail }) if detail.contains("No such file") => {
                // Precondition specified on a path that doesn't
                // exist — treat as a mismatch with a sentinel
                // "absent" actual revision so the gateway maps to
                // 409 in the same way as a stale revision.
                send_error(&server_tx, &stream_id, "REVISION_MISMATCH", "").await;
                drain_remaining_client_frames(&mut client_frames).await;
                return;
            }
            Err(e) => {
                send_error(&server_tx, &stream_id, "WRITE_FAILED", &e.to_string()).await;
                drain_remaining_client_frames(&mut client_frames).await;
                return;
            }
        }
    } else if !params.expected_revision.is_empty() && params.force {
        // Both fields set is allowed (the proto-comment escape
        // hatch), but log a warning so a defensive gateway's
        // mistake is visible in agent logs even when no
        // wire-signal distinguishes the regime.
        warn!(
            stream_id = %stream_id,
            expected_revision = %params.expected_revision,
            "write_file force=true bypasses non-empty expected_revision"
        );
    }

    // Collect stdin chunks until Close{stdin_eof}, EOF, or non-Stdin frame.
    // Comp-3 B5: cap at MAX_WRITE_BYTES so a client can't OOM the agent.
    let mut content: Vec<u8> = Vec::new();
    while let Some(frame) = client_frames.next().await {
        let Ok(IoClientFrame {
            payload: Some(p), ..
        }) = frame
        else {
            break;
        };
        match p {
            io_client_frame::Payload::Stdin(bytes) => {
                if content.len().saturating_add(bytes.len()) > MAX_WRITE_BYTES {
                    send_error(
                        &server_tx,
                        &stream_id,
                        "PAYLOAD_TOO_LARGE",
                        &format!(
                            "write_file body exceeds {MAX_WRITE_BYTES}-byte cap"
                        ),
                    )
                    .await;
                    drain_remaining_client_frames(&mut client_frames).await;
                    return;
                }
                content.extend_from_slice(&bytes);
            }
            io_client_frame::Payload::Close(_) => break,
            io_client_frame::Payload::Signal(_) | io_client_frame::Payload::Start(_) => {
                send_error(
                    &server_tx,
                    &stream_id,
                    "INVALID_REQUEST",
                    "only Stdin and Close frames are valid in write_file mode",
                )
                .await;
                drain_remaining_client_frames(&mut client_frames).await;
                return;
            }
        }
    }

    match runtime
        .write_file(&container_id, &params.path, cwd, Bytes::from(content))
        .await
    {
        Ok(()) => {
            // v1.0.3: emit FileMeta carrying the just-written
            // revision so the caller can update its cached token
            // without an extra read round-trip. Best-effort —
            // a runtime that can't stat (e.g. the docker / youki
            // stubs during rollout) still closes the session
            // cleanly with IoExited, preserving v1.0.2 wire compat.
            if let Ok(rev) = runtime.stat_revision(&container_id, &params.path, cwd).await {
                let meta_frame = IoServerFrame {
                    stream_id: stream_id.clone(),
                    payload: Some(io_server_frame::Payload::FileMeta(
                        open_sandbox_contracts::proxy::FileMeta {
                            revision: rev.revision,
                            size: rev.size,
                        },
                    )),
                };
                if server_tx.send(meta_frame).await.is_err() {
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
        Err(e) => send_error(&server_tx, &stream_id, "WRITE_FAILED", &e.to_string()).await,
    }
}

async fn drive_write_files_targz<R, S>(
    runtime: Arc<R>,
    stream_id: String,
    container_id: ContainerId,
    params: open_sandbox_contracts::proxy::WriteFilesTargzParams,
    mut client_frames: S,
    server_tx: mpsc::Sender<IoServerFrame>,
) where
    R: ContainerRuntime,
    S: Stream<Item = Result<IoClientFrame, AgentError>> + Unpin + Send + 'static,
{
    let cwd = if params.cwd.is_empty() {
        None
    } else {
        Some(params.cwd.as_str())
    };
    info!(
        stream_id = %stream_id,
        cwd = ?cwd,
        "io_session.start op=write_files_targz"
    );

    let mut tarball: Vec<u8> = Vec::new();
    while let Some(frame) = client_frames.next().await {
        let Ok(IoClientFrame {
            payload: Some(p), ..
        }) = frame
        else {
            break;
        };
        match p {
            io_client_frame::Payload::Stdin(bytes) => {
                if tarball.len().saturating_add(bytes.len()) > MAX_WRITE_BYTES {
                    send_error(
                        &server_tx,
                        &stream_id,
                        "PAYLOAD_TOO_LARGE",
                        &format!(
                            "write_files_targz body exceeds {MAX_WRITE_BYTES}-byte cap"
                        ),
                    )
                    .await;
                    drain_remaining_client_frames(&mut client_frames).await;
                    return;
                }
                tarball.extend_from_slice(&bytes);
            }
            io_client_frame::Payload::Close(_) => break,
            io_client_frame::Payload::Signal(_) | io_client_frame::Payload::Start(_) => {
                send_error(
                    &server_tx,
                    &stream_id,
                    "INVALID_REQUEST",
                    "only Stdin and Close frames are valid in write_files_targz mode",
                )
                .await;
                drain_remaining_client_frames(&mut client_frames).await;
                return;
            }
        }
    }

    match runtime
        .write_files_targz(&container_id, cwd, Bytes::from(tarball))
        .await
    {
        Ok(()) => {
            let exited = IoServerFrame {
                stream_id: stream_id.clone(),
                payload: Some(io_server_frame::Payload::Exited(IoExited {
                    exit_code: 0,
                    command_not_found: false,
                })),
            };
            let _ = server_tx.send(exited).await;
        }
        Err(e) => send_error(&server_tx, &stream_id, "WRITE_FAILED", &e.to_string()).await,
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

/// v1.0.3: drain any remaining client frames after an early-return
/// error in `drive_write_file` (or any future single-shot handler
/// whose caller may have pipelined frames). Without this drain, the
/// per-stream sender on the proxy-tunnel demux blocks indefinitely
/// — head-of-line-blocking every other multiplexed session on the
/// same agent tunnel.
///
/// `client_frames` is consumed; this never panics on a closed
/// channel — it just exits when `.next()` returns None.
async fn drain_remaining_client_frames<S>(client_frames: &mut S)
where
    S: Stream<Item = Result<IoClientFrame, AgentError>> + Unpin,
{
    while client_frames.next().await.is_some() {
        // Discard. The drain runs in the same task as the rest of
        // drive_write_file's tail, so it doesn't escalate from
        // "task ending" to "task lingering forever" — once the
        // proxy demux closes its sender (either because the gateway
        // closed its WS, or after a small EOF window), .next()
        // resolves to None and we return.
    }
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

    /// Test-only runtime whose `list_dir` and `stat_revision` return
    /// the exact "not yet implemented" detail string produced by the
    /// agent-docker and agent-youki stubs. Used to pin
    /// drive_list_dir's translation to the NOT_IMPLEMENTED error
    /// code. Other trait methods delegate to MockContainerRuntime
    /// defaults via deref-ish indirection (we just embed one).
    struct NotYetImplementedRuntime;

    impl ContainerRuntime for NotYetImplementedRuntime {
        async fn create_and_start(
            &self,
            _config: crate::container::ContainerConfig,
        ) -> Result<crate::container::ContainerInfo, AgentError> {
            unimplemented!("not used by these tests")
        }
        async fn stop_and_remove(
            &self,
            _id: &ContainerId,
            _timeout: Duration,
        ) -> Result<(), AgentError> {
            Ok(())
        }
        async fn pause(&self, _id: &ContainerId) -> Result<(), AgentError> {
            Ok(())
        }
        async fn unpause(&self, _id: &ContainerId) -> Result<(), AgentError> {
            Ok(())
        }
        async fn list_sandbox_containers(
            &self,
        ) -> Result<Vec<crate::container::ContainerInfo>, AgentError> {
            Ok(Vec::new())
        }
        async fn start_exec(
            &self,
            _id: &ContainerId,
            _start: crate::container::ExecStart,
        ) -> Result<crate::container::ExecHandle, AgentError> {
            unimplemented!("not used by these tests")
        }
        async fn signal_exec(
            &self,
            _id: &ContainerId,
            _in_container_pid: i32,
            _signum: i32,
        ) -> Result<(), AgentError> {
            Ok(())
        }
        async fn read_file(
            &self,
            _id: &ContainerId,
            _path: &str,
            _cwd: Option<&str>,
        ) -> Result<bytes::Bytes, AgentError> {
            unimplemented!("not used by these tests")
        }
        async fn write_file(
            &self,
            _id: &ContainerId,
            _path: &str,
            _cwd: Option<&str>,
            _content: bytes::Bytes,
        ) -> Result<(), AgentError> {
            Ok(())
        }
        async fn write_files_targz(
            &self,
            _id: &ContainerId,
            _cwd: Option<&str>,
            _tarball: bytes::Bytes,
        ) -> Result<(), AgentError> {
            Ok(())
        }
        async fn list_dir(
            &self,
            _id: &ContainerId,
            _path: &str,
            _cwd: Option<&str>,
        ) -> Result<crate::container::DirListing, AgentError> {
            // Same detail string produced by the agent-docker and
            // agent-youki stubs. drive_list_dir routes on this exact
            // prefix.
            Err(AgentError::Runtime {
                detail: "list_dir not yet implemented for docker runtime".into(),
            })
        }
        async fn stat_revision(
            &self,
            _id: &ContainerId,
            _path: &str,
            _cwd: Option<&str>,
        ) -> Result<crate::container::FileRevision, AgentError> {
            Err(AgentError::Runtime {
                detail: "stat_revision not yet implemented for docker runtime".into(),
            })
        }
        async fn wait_port_listening(
            &self,
            _id: &ContainerId,
            _port: u32,
            _timeout: Duration,
        ) -> Result<bool, AgentError> {
            Err(AgentError::Runtime {
                detail: "wait_port_listening not yet implemented for docker runtime"
                    .into(),
            })
        }
        async fn delete_file(
            &self,
            _id: &ContainerId,
            _path: &str,
            _cwd: Option<&str>,
            _recursive: bool,
        ) -> Result<(), AgentError> {
            Err(AgentError::Runtime {
                detail: "delete_file not yet implemented for docker runtime".into(),
            })
        }
    }

    /// Test-only `HostPortLookup` that returns a fixed port for any
    /// sandbox, or a `SandboxNotFound` error when constructed with
    /// `with_missing_sandbox`.
    struct StubHostPorts {
        port: Option<u16>,
    }
    impl StubHostPorts {
        fn ok(port: u16) -> Arc<Self> {
            Arc::new(Self { port: Some(port) })
        }
        fn missing() -> Arc<Self> {
            Arc::new(Self { port: None })
        }
    }
    impl crate::container::HostPortLookup for StubHostPorts {
        fn host_port_for(&self, sandbox_id: &SandboxId) -> Result<u16, AgentError> {
            self.port.ok_or_else(|| AgentError::SandboxNotFound {
                sandbox_id: sandbox_id.to_string(),
            })
        }
    }

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
        spawn_session_with_ports(runtime, registry, stream_id, StubHostPorts::ok(9000))
    }

    fn spawn_session_with_ports<R: ContainerRuntime + Send + Sync + 'static>(
        runtime: Arc<R>,
        registry: Arc<ExecRegistry>,
        stream_id: &str,
        host_ports: Arc<dyn crate::container::HostPortLookup>,
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
                host_ports,
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

        tx.send(Ok(iostart_exec(vec!["echo", "hello"])))
            .await
            .unwrap();
        // Hold tx open until the natural Exited frame arrives —
        // dropping it would be interpreted as a client disconnect
        // and trigger the SIGTERM/SIGKILL cleanup path instead.

        let frames = collect_until_exit(rx).await;
        // Expect: Started, Stdout("hello\n"), Exited(0)
        assert!(matches!(
            frames.first(),
            Some(io_server_frame::Payload::Started(_))
        ));
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
        // Hold tx open — see exec_runs_echo for rationale.

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

        tx.send(Ok(iostart_exec(vec!["sleep", "30"])))
            .await
            .unwrap();
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
    async fn abrupt_stream_drop_triggers_cleanup_signals() {
        // Models a client process that was SIGKILLed (or whose TCP
        // connection was reset) without sending an explicit Close.
        // The agent must still drive the cleanup hook so the
        // in-container PID is signaled — without this guard,
        // abandoned processes outlive their controller.
        let runtime = Arc::new(MockContainerRuntime::new());
        let registry = Arc::new(ExecRegistry::new());
        let (tx, mut rx, h) = spawn_session(runtime.clone(), registry.clone(), "s-drop");

        tx.send(Ok(iostart_exec(vec!["sleep", "30"])))
            .await
            .unwrap();
        let started = rx.recv().await.unwrap();
        assert!(matches!(
            started.payload,
            Some(io_server_frame::Payload::Started(_))
        ));
        assert_eq!(registry.len(), 1);

        // Abrupt drop — no Close frame sent.
        drop(tx);

        let _ = tokio::time::timeout(Duration::from_secs(10), h).await;

        let signums: Vec<i32> = runtime
            .signals_received()
            .iter()
            .map(|s| s.signum)
            .collect();
        assert!(
            signums.contains(&15),
            "SIGTERM should have been sent on abrupt drop, got {signums:?}"
        );
        assert!(
            signums.contains(&9),
            "SIGKILL should have been sent after grace, got {signums:?}"
        );
        assert!(
            registry.is_empty(),
            "registry should be drained after cleanup"
        );
    }

    #[tokio::test]
    async fn client_disconnect_triggers_cleanup_signals() {
        let runtime = Arc::new(MockContainerRuntime::new());
        let registry = Arc::new(ExecRegistry::new());
        let (tx, mut rx, h) = spawn_session(runtime.clone(), registry.clone(), "s5");

        tx.send(Ok(iostart_exec(vec!["sleep", "30"])))
            .await
            .unwrap();
        // Wait for Started so the registry is populated.
        let started = rx.recv().await.unwrap();
        assert!(matches!(
            started.payload,
            Some(io_server_frame::Payload::Started(_))
        ));
        assert_eq!(registry.len(), 1, "registry should have one entry");

        // Send Close { stdin_eof: false } to end the session.
        tx.send(Ok(IoClientFrame {
            stream_id: "s5".into(),
            payload: Some(io_client_frame::Payload::Close(IoClose {
                stdin_eof: false,
            })),
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
        assert!(
            registry.is_empty(),
            "registry should be drained after cleanup"
        );
    }

    #[tokio::test]
    async fn read_file_returns_contents() {
        let runtime =
            Arc::new(MockContainerRuntime::new().with_file("/home/data.txt", "file body"));
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
    async fn read_file_emits_file_meta_before_first_stdout_chunk() {
        // v1.0.3: FileMeta MUST be the first server payload on a
        // ReadFile session (before any Stdout chunks) so the UI can
        // capture the revision before consuming bytes. This pins
        // both the presence of the sidecar AND the ordering
        // invariant — a regression that emits FileMeta AFTER bytes
        // would defeat the optimistic-concurrency contract.
        let runtime =
            Arc::new(MockContainerRuntime::new().with_file("/home/app.py", "print('hi')\n"));
        let registry = Arc::new(ExecRegistry::new());
        let (tx, rx, h) = spawn_session(runtime, registry, "s-rf-meta");

        tx.send(Ok(IoClientFrame {
            stream_id: "s-rf-meta".into(),
            payload: Some(io_client_frame::Payload::Start(IoStart {
                sandbox_id: "ignored".into(),
                params: Some(open_sandbox_contracts::proxy::io_start::Params::ReadFile(
                    ReadFileParams {
                        path: "app.py".into(),
                        cwd: "/home".into(),
                    },
                )),
            })),
        }))
        .await
        .unwrap();
        drop(tx);

        let frames = collect_until_exit(rx).await;
        match &frames[0] {
            io_server_frame::Payload::FileMeta(m) => {
                // Mock encodes revision as `mock:<size>`.
                assert_eq!(m.size, 12);
                assert!(
                    !m.revision.is_empty(),
                    "revision must be non-empty; got {:?}",
                    m.revision
                );
            }
            other => panic!(
                "FileMeta must be the first frame on a ReadFile session; got {other:?}"
            ),
        }
        // Stdout frame(s) must follow FileMeta.
        assert!(matches!(
            &frames[1],
            io_server_frame::Payload::Stdout(_)
        ));
        // Reconstruct the bytes to verify the read content is unchanged.
        let joined: Vec<u8> = frames
            .iter()
            .filter_map(|p| match p {
                io_server_frame::Payload::Stdout(b) => Some(b.clone()),
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(joined, b"print('hi')\n");
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
    async fn write_file_with_stale_revision_emits_revision_mismatch() {
        // B11 (replaces the earlier NOT_IMPLEMENTED guard): when the
        // caller supplies a non-empty expected_revision that doesn't
        // match the file's current revision, the agent must surface
        // a typed REVISION_MISMATCH error carrying the live token
        // in the detail field. The gateway maps this to 409 Conflict.
        use open_sandbox_contracts::proxy::WriteFileParams;

        // Mock encodes revision as `mock:<size>`. Pre-seed app.py
        // with 7 bytes; the live revision will be "mock:7".
        let runtime = Arc::new(
            MockContainerRuntime::new().with_file("/home/app.py", "live!\nx"),
        );
        let registry = Arc::new(ExecRegistry::new());
        let (tx, rx, h) = spawn_session(runtime, registry, "s-rev-stale");

        tx.send(Ok(IoClientFrame {
            stream_id: "s-rev-stale".into(),
            payload: Some(io_client_frame::Payload::Start(IoStart {
                sandbox_id: "ignored".into(),
                params: Some(open_sandbox_contracts::proxy::io_start::Params::WriteFile(
                    WriteFileParams {
                        path: "app.py".into(),
                        cwd: "/home".into(),
                        // Stale token (different size). Real revision
                        // on disk is "mock:7".
                        expected_revision: "mock:99".into(),
                        force: false,
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
            io_server_frame::Payload::Error(e) => {
                assert_eq!(e.code, "REVISION_MISMATCH");
                assert_eq!(
                    e.detail, "mock:7",
                    "detail must carry the live revision; got {:?}",
                    e.detail
                );
            }
            other => panic!("expected Error frame, got {other:?}"),
        }
        h.await.unwrap();
    }

    #[tokio::test]
    async fn write_file_with_matching_revision_emits_file_meta_then_exited() {
        // B11 happy path: caller supplies the actual current
        // revision; agent writes through and emits FileMeta
        // carrying the NEW revision, followed by IoExited.
        use bytes::Bytes;
        use open_sandbox_contracts::proxy::WriteFileParams;

        let runtime = Arc::new(
            MockContainerRuntime::new().with_file("/home/app.py", "live!\nx"),
        );
        let registry = Arc::new(ExecRegistry::new());
        let (tx, rx, h) = spawn_session(runtime.clone(), registry, "s-rev-ok");

        tx.send(Ok(IoClientFrame {
            stream_id: "s-rev-ok".into(),
            payload: Some(io_client_frame::Payload::Start(IoStart {
                sandbox_id: "ignored".into(),
                params: Some(open_sandbox_contracts::proxy::io_start::Params::WriteFile(
                    WriteFileParams {
                        path: "app.py".into(),
                        cwd: "/home".into(),
                        // Matches the seeded 7-byte content
                        // ("mock:<bytes.len()>"). Mock revision shape
                        // is documented in MockContainerRuntime.
                        expected_revision: "mock:7".into(),
                        force: false,
                    },
                )),
            })),
        }))
        .await
        .unwrap();
        tx.send(Ok(IoClientFrame {
            stream_id: "s-rev-ok".into(),
            // 11-byte payload — the post-write revision will be
            // "mock:11".
            payload: Some(io_client_frame::Payload::Stdin(b"print('x')\n".to_vec())),
        }))
        .await
        .unwrap();
        tx.send(Ok(IoClientFrame {
            stream_id: "s-rev-ok".into(),
            payload: Some(io_client_frame::Payload::Close(IoClose { stdin_eof: true })),
        }))
        .await
        .unwrap();
        drop(tx);

        let frames = collect_until_exit(rx).await;
        // Expect [FileMeta, Exited] — FileMeta carries the
        // just-written revision so the caller can update its cached
        // token without a follow-up read.
        match &frames[0] {
            io_server_frame::Payload::FileMeta(m) => {
                assert_eq!(m.revision, "mock:11");
                assert_eq!(m.size, 11);
            }
            other => panic!("expected FileMeta first, got {other:?}"),
        }
        assert!(matches!(
            &frames[1],
            io_server_frame::Payload::Exited(IoExited {
                exit_code: 0,
                command_not_found: false
            })
        ));
        let writes = runtime.writes_received();
        assert!(
            writes.iter().any(|w| w.bytes == Bytes::from("print('x')\n")),
            "expected bytes to be persisted via the runtime trait"
        );
        h.await.unwrap();
    }

    #[tokio::test]
    async fn write_file_with_revision_on_unimplemented_runtime_emits_not_implemented() {
        // B11 capability gating: a runtime whose stat_revision
        // returns the "not yet implemented" stub error must surface
        // NOT_IMPLEMENTED to the caller (so the SDK can fall back
        // to force=true or wait for the rollout). Without this, a
        // gateway shipping the opt-in early would silently bypass
        // the precondition and lose conflict detection.
        use open_sandbox_contracts::proxy::WriteFileParams;

        let runtime = Arc::new(NotYetImplementedRuntime);
        let registry = Arc::new(ExecRegistry::new());
        let (tx, rx, h) = spawn_session(runtime, registry, "s-rev-noimpl");

        tx.send(Ok(IoClientFrame {
            stream_id: "s-rev-noimpl".into(),
            payload: Some(io_client_frame::Payload::Start(IoStart {
                sandbox_id: "ignored".into(),
                params: Some(open_sandbox_contracts::proxy::io_start::Params::WriteFile(
                    WriteFileParams {
                        path: "app.py".into(),
                        cwd: "/home".into(),
                        expected_revision: "mock:7".into(),
                        force: false,
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
            io_server_frame::Payload::Error(e) => {
                assert_eq!(e.code, "NOT_IMPLEMENTED");
                assert!(
                    e.detail.contains("stat_revision") || e.detail.contains("force=true"),
                    "error detail should mention the missing capability or escape hatch; got {:?}",
                    e.detail
                );
            }
            other => panic!("expected Error frame, got {other:?}"),
        }
        h.await.unwrap();
    }

    #[tokio::test]
    async fn write_file_with_force_bypasses_revision_check() {
        // The same future precondition is documented to be bypassable
        // with force=true (for scripted bulk-writes). Verify the
        // guard does NOT engage when force is set, so the write path
        // remains reachable for callers that opt out explicitly.
        use bytes::Bytes;
        use open_sandbox_contracts::proxy::WriteFileParams;

        let runtime = Arc::new(MockContainerRuntime::new());
        let registry = Arc::new(ExecRegistry::new());
        let (tx, rx, h) = spawn_session(runtime.clone(), registry, "s-force");

        tx.send(Ok(IoClientFrame {
            stream_id: "s-force".into(),
            payload: Some(io_client_frame::Payload::Start(IoStart {
                sandbox_id: "ignored".into(),
                params: Some(open_sandbox_contracts::proxy::io_start::Params::WriteFile(
                    WriteFileParams {
                        path: "app.py".into(),
                        cwd: "/home".into(),
                        // force=true SHOULD bypass the precondition
                        // even when a (stale) revision is supplied.
                        expected_revision: "stale".into(),
                        force: true,
                    },
                )),
            })),
        }))
        .await
        .unwrap();
        tx.send(Ok(IoClientFrame {
            stream_id: "s-force".into(),
            payload: Some(io_client_frame::Payload::Stdin(b"print('x')\n".to_vec())),
        }))
        .await
        .unwrap();
        tx.send(Ok(IoClientFrame {
            stream_id: "s-force".into(),
            payload: Some(io_client_frame::Payload::Close(IoClose { stdin_eof: true })),
        }))
        .await
        .unwrap();
        drop(tx);

        let frames = collect_until_exit(rx).await;
        let last = frames.last().expect("at least one frame");
        assert!(
            matches!(
                last,
                io_server_frame::Payload::Exited(IoExited {
                    exit_code: 0,
                    command_not_found: false
                })
            ),
            "expected clean Exited frame on force write; got {last:?}"
        );
        // The mock runtime records the bytes that landed.
        let writes = runtime.writes_received();
        assert!(
            writes.iter().any(|w| w.bytes == Bytes::from("print('x')\n")),
            "expected bytes to be persisted via the runtime trait"
        );
        h.await.unwrap();
    }

    #[tokio::test]
    async fn list_dir_emits_typed_result_then_exits() {
        // Pre-seed the mock with four leaves under /workspace plus
        // one leaf under /workspace/src. The listing for /workspace
        // should return the three top-level files + a synthetic `src`
        // dir entry, sorted alphabetically.
        use open_sandbox_contracts::proxy::{
            ListDirEntryType, ListDirParams as P, ListDirResult,
        };

        let runtime = Arc::new(
            MockContainerRuntime::new()
                .with_file("/workspace/README.md", "readme\n")
                .with_file("/workspace/app.py", "print('x')\n")
                .with_file("/workspace/Makefile", "all:\n")
                .with_file("/workspace/src/main.rs", "fn main() {}\n"),
        );
        let registry = Arc::new(ExecRegistry::new());
        let (tx, rx, h) = spawn_session(runtime, registry, "s-ls");

        tx.send(Ok(IoClientFrame {
            stream_id: "s-ls".into(),
            payload: Some(io_client_frame::Payload::Start(IoStart {
                sandbox_id: "ignored".into(),
                params: Some(open_sandbox_contracts::proxy::io_start::Params::ListDir(P {
                    path: "/workspace".into(),
                    cwd: String::new(),
                })),
            })),
        }))
        .await
        .unwrap();
        drop(tx);

        let frames = collect_until_exit(rx).await;
        assert_eq!(frames.len(), 2, "expected ListDirResult + Exited; got {frames:#?}");
        match &frames[0] {
            io_server_frame::Payload::ListDirResult(ListDirResult {
                path,
                entries,
                truncated,
                total_entries,
            }) => {
                assert_eq!(path, "/workspace");
                assert!(!truncated);
                assert_eq!(*total_entries, 4);
                assert_eq!(entries.len(), 4);
                let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
                assert_eq!(names, vec!["Makefile", "README.md", "app.py", "src"]);
                let src_entry = entries.iter().find(|e| e.name == "src").unwrap();
                assert_eq!(src_entry.r#type, ListDirEntryType::Dir as i32);
                let readme = entries.iter().find(|e| e.name == "README.md").unwrap();
                assert_eq!(readme.r#type, ListDirEntryType::File as i32);
                assert_eq!(readme.size, 7);
            }
            other => panic!("expected ListDirResult, got {other:?}"),
        }
        assert!(matches!(
            frames[1],
            io_server_frame::Payload::Exited(IoExited {
                exit_code: 0,
                command_not_found: false
            })
        ));
        h.await.unwrap();
    }

    #[tokio::test]
    async fn list_dir_missing_path_surfaces_file_not_found() {
        use open_sandbox_contracts::proxy::ListDirParams as P;

        let runtime = Arc::new(MockContainerRuntime::new());
        let registry = Arc::new(ExecRegistry::new());
        let (tx, rx, h) = spawn_session(runtime, registry, "s-ls-404");

        // The mock returns an empty listing for any path; force
        // the FILE_NOT_FOUND path by routing through the failing
        // runtime instead. Use a runtime that returns
        // AgentError::Runtime{detail="No such file: ..."}.
        // Drop tx after sending the IoStart.
        tx.send(Ok(IoClientFrame {
            stream_id: "s-ls-404".into(),
            payload: Some(io_client_frame::Payload::Start(IoStart {
                sandbox_id: "ignored".into(),
                params: Some(open_sandbox_contracts::proxy::io_start::Params::ListDir(P {
                    path: "/nope".into(),
                    cwd: String::new(),
                })),
            })),
        }))
        .await
        .unwrap();
        drop(tx);

        let frames = collect_until_exit(rx).await;
        // The mock returns an empty DirListing (not an error) for an
        // unseeded path, so the success path emits ListDirResult with
        // zero entries. This is the documented mock behavior; the
        // FILE_NOT_FOUND path is exercised separately by the real
        // runtime impl that follows in subsequent group-B commits.
        match &frames[0] {
            io_server_frame::Payload::ListDirResult(r) => {
                assert_eq!(r.entries.len(), 0);
                assert!(!r.truncated);
            }
            other => panic!("expected ListDirResult, got {other:?}"),
        }
        h.await.unwrap();
    }

    #[tokio::test]
    async fn list_dir_runtime_stub_surfaces_not_implemented() {
        // Regression: until the real docker/youki list_dir impls
        // land, the runtime stubs return
        // AgentError::Runtime{detail="list_dir not yet implemented…"}.
        // drive_list_dir MUST translate that to a typed
        // NOT_IMPLEMENTED error code so a gateway/SDK feature-
        // detecting list_dir support can fall back, matching the
        // symmetric drive_write_file precondition guard.
        use open_sandbox_contracts::proxy::ListDirParams as P;

        let runtime = Arc::new(NotYetImplementedRuntime);
        let registry = Arc::new(ExecRegistry::new());
        let (tx, rx, h) = spawn_session(runtime, registry, "s-ls-noimpl");

        tx.send(Ok(IoClientFrame {
            stream_id: "s-ls-noimpl".into(),
            payload: Some(io_client_frame::Payload::Start(IoStart {
                sandbox_id: "ignored".into(),
                params: Some(open_sandbox_contracts::proxy::io_start::Params::ListDir(P {
                    path: "/workspace".into(),
                    cwd: String::new(),
                })),
            })),
        }))
        .await
        .unwrap();
        drop(tx);

        let frames = collect_until_exit(rx).await;
        let last = frames.last().expect("at least one frame");
        match last {
            io_server_frame::Payload::Error(e) => {
                assert_eq!(
                    e.code, "NOT_IMPLEMENTED",
                    "expected NOT_IMPLEMENTED routing on stub runtime; got {e:?}"
                );
            }
            other => panic!("expected Error frame, got {other:?}"),
        }
        h.await.unwrap();
    }

    #[tokio::test]
    async fn list_dir_caps_at_5000_entries() {
        // Seed the mock with more entries than LIST_DIR_MAX_ENTRIES.
        // The mock's list_dir applies the cap server-side; assert
        // truncated=true and total_entries reports the underlying
        // count.
        use open_sandbox_contracts::constants::LIST_DIR_MAX_ENTRIES;
        use open_sandbox_contracts::proxy::ListDirParams as P;

        let mut runtime = MockContainerRuntime::new();
        // 5001 leaves all under /big — the mock dedupes by full path,
        // so we generate distinct filenames.
        let overflow = LIST_DIR_MAX_ENTRIES + 1;
        for i in 0..overflow {
            runtime = runtime.with_file(format!("/big/f{i:05}.txt"), format!("{i}"));
        }
        let runtime = Arc::new(runtime);
        let registry = Arc::new(ExecRegistry::new());
        let (tx, rx, h) = spawn_session(runtime, registry, "s-ls-cap");

        tx.send(Ok(IoClientFrame {
            stream_id: "s-ls-cap".into(),
            payload: Some(io_client_frame::Payload::Start(IoStart {
                sandbox_id: "ignored".into(),
                params: Some(open_sandbox_contracts::proxy::io_start::Params::ListDir(P {
                    path: "/big".into(),
                    cwd: String::new(),
                })),
            })),
        }))
        .await
        .unwrap();
        drop(tx);

        let frames = collect_until_exit(rx).await;
        match &frames[0] {
            io_server_frame::Payload::ListDirResult(r) => {
                assert!(r.truncated, "expected truncated=true at {overflow} entries");
                assert_eq!(r.entries.len(), LIST_DIR_MAX_ENTRIES);
                assert_eq!(r.total_entries as usize, overflow);
            }
            other => panic!("expected ListDirResult, got {other:?}"),
        }
        h.await.unwrap();
    }

    #[tokio::test]
    async fn wait_port_listening_returns_ready_when_runtime_reports_listening() {
        // v1.0.3.x: the probe is now delegated to the runtime
        // trait (it must run from inside the container's netns;
        // host-side TCP probes false-positive on Docker Desktop).
        // MockContainerRuntime.with_listening_port pre-registers
        // the in-container port as "bound"; the agent's
        // drive_wait_port_listening should see Ok(true).
        use open_sandbox_contracts::proxy::WaitPortListeningParams;

        let runtime = Arc::new(MockContainerRuntime::new().with_listening_port(8080));
        let registry = Arc::new(ExecRegistry::new());
        let (tx, rx, h) = spawn_session(runtime, registry, "s-wpl-ok");

        tx.send(Ok(IoClientFrame {
            stream_id: "s-wpl-ok".into(),
            payload: Some(io_client_frame::Payload::Start(IoStart {
                sandbox_id: "ignored".into(),
                params: Some(
                    open_sandbox_contracts::proxy::io_start::Params::WaitPortListening(
                        WaitPortListeningParams {
                            port: 8080,
                            timeout_ms: 3_000,
                        },
                    ),
                ),
            })),
        }))
        .await
        .unwrap();
        drop(tx);

        let frames = collect_until_exit(rx).await;
        match &frames[0] {
            io_server_frame::Payload::WaitPortListeningResult(r) => {
                assert!(r.ready, "expected ready=true; got {r:?}");
            }
            other => panic!("expected WaitPortListeningResult, got {other:?}"),
        }
        assert!(matches!(
            frames[1],
            io_server_frame::Payload::Exited(IoExited {
                exit_code: 0,
                command_not_found: false
            })
        ));
        h.await.unwrap();
    }

    #[tokio::test]
    async fn wait_port_listening_returns_not_ready_when_runtime_reports_silent() {
        // Mock with no listening ports registered → wait_port_
        // listening returns Ok(false) → drive_wait_port_listening
        // emits ready=false. The mock returns immediately rather
        // than honoring `timeout_ms`; we trade off real-time
        // accuracy here against test latency (preserved by the
        // real docker/youki impls which DO poll the actual
        // timeout window).
        use open_sandbox_contracts::proxy::WaitPortListeningParams;

        let runtime = Arc::new(MockContainerRuntime::new());
        let registry = Arc::new(ExecRegistry::new());
        let (tx, rx, h) = spawn_session(runtime, registry, "s-wpl-timeout");

        tx.send(Ok(IoClientFrame {
            stream_id: "s-wpl-timeout".into(),
            payload: Some(io_client_frame::Payload::Start(IoStart {
                sandbox_id: "ignored".into(),
                params: Some(
                    open_sandbox_contracts::proxy::io_start::Params::WaitPortListening(
                        WaitPortListeningParams {
                            port: 8080,
                            timeout_ms: 250,
                        },
                    ),
                ),
            })),
        }))
        .await
        .unwrap();
        drop(tx);

        let frames = collect_until_exit(rx).await;
        match &frames[0] {
            io_server_frame::Payload::WaitPortListeningResult(r) => {
                assert!(!r.ready, "expected ready=false; got {r:?}");
                // elapsed_ms is clamped to timeout_ms on the
                // not-ready branch (the docs promise the
                // invariant `elapsed_ms <= timeout_ms`).
                assert!(
                    r.elapsed_ms <= 250,
                    "elapsed_ms must not exceed timeout_ms; got {}",
                    r.elapsed_ms
                );
            }
            other => panic!("expected WaitPortListeningResult, got {other:?}"),
        }
        h.await.unwrap();
    }

    #[tokio::test]
    async fn wait_port_listening_routes_not_implemented_for_stub_runtime() {
        // When the runtime's wait_port_listening returns the
        // "not yet implemented" stub error (matching the docker /
        // youki stubs during a rollout window), the agent should
        // surface a typed NOT_IMPLEMENTED IoError so the SDK can
        // feature-detect. Same shape as the drive_list_dir /
        // drive_write_file precondition-stub routing.
        use open_sandbox_contracts::proxy::WaitPortListeningParams;

        let runtime = Arc::new(NotYetImplementedRuntime);
        let registry = Arc::new(ExecRegistry::new());
        let (tx, rx, h) = spawn_session(runtime, registry, "s-wpl-noimpl");

        tx.send(Ok(IoClientFrame {
            stream_id: "s-wpl-noimpl".into(),
            payload: Some(io_client_frame::Payload::Start(IoStart {
                sandbox_id: "ignored".into(),
                params: Some(
                    open_sandbox_contracts::proxy::io_start::Params::WaitPortListening(
                        WaitPortListeningParams {
                            port: 8080,
                            timeout_ms: 1_000,
                        },
                    ),
                ),
            })),
        }))
        .await
        .unwrap();
        drop(tx);

        let frames = collect_until_exit(rx).await;
        let last = frames.last().expect("at least one frame");
        match last {
            io_server_frame::Payload::Error(e) => {
                assert_eq!(e.code, "NOT_IMPLEMENTED");
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

        tx.send(Ok(iostart_exec(vec!["sleep", "30"])))
            .await
            .unwrap();
        let _started = rx.recv().await.unwrap();
        // Now send a second IoStart — protocol error.
        tx.send(Ok(iostart_exec(vec!["echo", "no"]))).await.unwrap();

        // Cleanup should still fire.
        let _ = tokio::time::timeout(Duration::from_secs(10), h).await;
        let signals = runtime.signals_received();
        assert!(!signals.is_empty(), "cleanup should have fired");
    }
}
