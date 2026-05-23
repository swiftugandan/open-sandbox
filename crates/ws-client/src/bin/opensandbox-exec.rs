//! `opensandbox-exec` — CLI wrapper around the Open Sandbox
//! streaming exec WebSocket API. Useful for shell-level scripting
//! and as a demonstration of the v1.0 protocol.
//!
//! Usage:
//!   opensandbox-exec --base ws://localhost:8081 \
//!                    --sandbox <uuid> \
//!                    --api-key $KEY \
//!                    -- bash -c 'echo hello && date'
//!
//! Local stdin is forwarded to the remote process when stdin is not
//! a TTY (e.g. when piping or redirecting). EOF on local stdin
//! triggers a half-close of the remote stdin without ending the
//! session — the remote process keeps running until it exits on its
//! own or the WebSocket is closed.

use std::io::{IsTerminal, Read, Write};
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use open_sandbox_ws_client::{ExecParams, ExecSession, ServerFrame};
use tokio::sync::mpsc;

#[derive(Parser, Debug)]
#[command(name = "opensandbox-exec")]
struct Args {
    /// Base URL of the API gateway (ws://... or wss://...).
    #[arg(long)]
    base: String,
    /// Sandbox ID (UUID).
    #[arg(long)]
    sandbox: String,
    /// API key.
    #[arg(long, env = "OPEN_SANDBOX_API_KEY")]
    api_key: String,
    /// Working directory inside the container.
    #[arg(long)]
    cwd: Option<String>,
    /// Read stdout/stderr for at most N seconds then exit cleanly.
    /// Used by idle-keepalive and long-running scenarios that
    /// don't care about waiting for the in-container process to
    /// exit on its own. 0 = no limit (default).
    #[arg(long, default_value_t = 0)]
    read_for_secs: u64,
    /// Control whether local stdin is forwarded to the remote
    /// process. `auto` forwards iff stdin is not a TTY. `always`
    /// forwards unconditionally (useful with `--read-for-secs`).
    /// `never` skips the stdin pump.
    #[arg(long, value_enum, default_value_t = StdinMode::Auto)]
    stdin: StdinMode,
    /// Command and arguments to run.
    #[arg(trailing_var_arg = true, required = true)]
    command: Vec<String>,
}

#[derive(Copy, Clone, Debug, clap::ValueEnum)]
enum StdinMode {
    Auto,
    Always,
    Never,
}

enum StdinMsg {
    Chunk(Vec<u8>),
    Eof,
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    let mut params = ExecParams::new(args.command);
    if let Some(cwd) = args.cwd {
        params = params.cwd(cwd);
    }
    let mut session =
        match ExecSession::connect(&args.base, &args.sandbox, &args.api_key, params).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("connect: {e}");
                // Comp-7: shift CLI-level error codes into the 124..=127 /
                // 128+ range to avoid colliding with legitimate sandbox
                // process exit codes (timeout(1)/sudo/env convention).
                // 124 = client could not connect.
                return ExitCode::from(124);
            }
        };

    let forward_stdin = match args.stdin {
        StdinMode::Always => true,
        StdinMode::Never => false,
        StdinMode::Auto => !std::io::stdin().is_terminal(),
    };

    // mpsc with capacity 8 to backpressure a fast local writer
    // against a slow WebSocket. spawn_blocking is needed because
    // std::io::Stdin is sync.
    let (tx, mut stdin_rx) = mpsc::channel::<StdinMsg>(8);
    if forward_stdin {
        std::thread::spawn(move || {
            let stdin = std::io::stdin();
            let mut handle = stdin.lock();
            let mut buf = [0u8; 8192];
            loop {
                match handle.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx
                            .blocking_send(StdinMsg::Chunk(buf[..n].to_vec()))
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(_) => break,
                }
            }
            let _ = tx.blocking_send(StdinMsg::Eof);
        });
    } else {
        drop(tx);
    }

    let mut stdin_done = !forward_stdin;
    let deadline = if args.read_for_secs > 0 {
        Some(tokio::time::Instant::now() + Duration::from_secs(args.read_for_secs))
    } else {
        None
    };

    // Comp-7: Ctrl-C handler. The CLI previously died on SIGINT without
    // notifying the agent; the in-container PID kept running. We forward
    // each Ctrl-C as a SIGINT to the remote via a dedicated mpsc.
    let (sigint_tx, mut sigint_rx) = mpsc::channel::<()>(4);
    tokio::spawn(async move {
        loop {
            if tokio::signal::ctrl_c().await.is_err() {
                break;
            }
            if sigint_tx.send(()).await.is_err() {
                break;
            }
        }
    });

    #[allow(unused_assignments)]
    let mut exit_code: i32 = 0;
    loop {
        // Decide which futures to race. The borrow checker won't
        // let us race two `&mut session` futures, so stdin pumping
        // goes through the channel and we only call `send_stdin`
        // outside the select — same trick used by tower's mpsc
        // adapters.
        let frame_res = match (stdin_done, deadline) {
            (true, None) => tokio::select! {
                f = session.next_frame() => f,
                Some(()) = sigint_rx.recv() => {
                    let _ = session.send_signal(2).await;
                    continue;
                }
            },
            (true, Some(d)) => tokio::select! {
                f = session.next_frame() => f,
                Some(()) = sigint_rx.recv() => {
                    let _ = session.send_signal(2).await;
                    continue;
                }
                _ = tokio::time::sleep_until(d) => {
                    eprintln!("# read deadline reached after {}s — exiting cleanly", args.read_for_secs);
                    return ExitCode::from(0);
                }
            },
            (false, None) => tokio::select! {
                msg = stdin_rx.recv() => {
                    match msg {
                        Some(StdinMsg::Chunk(c)) => {
                            if let Err(e) = session.send_stdin(c).await {
                                eprintln!("# stdin send: {e}");
                            }
                            continue;
                        }
                        Some(StdinMsg::Eof) | None => {
                            let _ = session.close_stdin().await;
                            stdin_done = true;
                            continue;
                        }
                    }
                }
                Some(()) = sigint_rx.recv() => {
                    let _ = session.send_signal(2).await;
                    continue;
                }
                f = session.next_frame() => f,
            },
            (false, Some(d)) => tokio::select! {
                msg = stdin_rx.recv() => {
                    match msg {
                        Some(StdinMsg::Chunk(c)) => {
                            if let Err(e) = session.send_stdin(c).await {
                                eprintln!("# stdin send: {e}");
                            }
                            continue;
                        }
                        Some(StdinMsg::Eof) | None => {
                            let _ = session.close_stdin().await;
                            stdin_done = true;
                            continue;
                        }
                    }
                }
                Some(()) = sigint_rx.recv() => {
                    let _ = session.send_signal(2).await;
                    continue;
                }
                f = session.next_frame() => f,
                _ = tokio::time::sleep_until(d) => {
                    eprintln!("# read deadline reached after {}s — exiting cleanly", args.read_for_secs);
                    return ExitCode::from(0);
                }
            },
        };

        match frame_res {
            Ok(Some(ServerFrame::Started {
                exec_id,
                in_container_pid,
            })) => {
                eprintln!("# started exec_id={exec_id} pid={in_container_pid}");
            }
            Ok(Some(ServerFrame::Stdout(b))) => {
                let _ = std::io::stdout().write_all(&b);
                let _ = std::io::stdout().flush();
            }
            Ok(Some(ServerFrame::Stderr(b))) => {
                let _ = std::io::stderr().write_all(&b);
                let _ = std::io::stderr().flush();
            }
            Ok(Some(ServerFrame::Exited {
                exit_code: code,
                command_not_found,
            })) => {
                if command_not_found {
                    eprintln!("# command not found");
                }
                exit_code = code;
                break;
            }
            Ok(Some(ServerFrame::Error { code, detail })) => {
                eprintln!("# error code={code} detail={detail}");
                // Comp-7: shifted to 125 (CLI-level remote-error class) to
                // avoid colliding with sandbox process exits 1..=125.
                return ExitCode::from(125);
            }
            Ok(None) => {
                eprintln!("# session closed without exit");
                return ExitCode::from(126);
            }
            Err(e) => {
                eprintln!("# i/o: {e}");
                return ExitCode::from(127);
            }
        }
    }

    ExitCode::from((exit_code & 0xff) as u8)
}
