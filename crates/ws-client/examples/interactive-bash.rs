//! Interactive shell example. Opens an exec session running
//! `sh` with stdin attached, pumps the local process's stdin
//! into the session, and streams the sandbox's stdout/stderr
//! back to the local terminal. Half-closes stdin via the
//! `IoClose{stdin_eof=true}` half-close frame when local stdin
//! ends.
//!
//! Two modes:
//!   - default: read from local stdin until EOF (Ctrl-D), then
//!     half-close, then wait for the shell to exit.
//!   - `--once`: send a single canned command + exit, useful for
//!     CI smoke testing.
//!
//! ```bash
//! # Live, interactive:
//! cargo run -p open-sandbox-ws-client --example interactive-bash \
//!     -- --sandbox "$SB"
//!
//! # One-shot smoke:
//! cargo run -p open-sandbox-ws-client --example interactive-bash \
//!     -- --sandbox "$SB" --once
//! ```

use clap::Parser;
use open_sandbox_ws_client::{ExecParams, ExecSession, ServerFrame};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "ws://localhost:18081")]
    base: String,
    #[arg(long)]
    sandbox: String,
    #[arg(long, default_value = "e2e-api-key")]
    api_key: String,
    /// One-shot mode: send a fixed sequence of commands + exit.
    /// Use this in CI; without it the example blocks on stdin.
    #[arg(long)]
    once: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args = Args::parse();
    let params = ExecParams::new(vec!["sh".into()]);
    let mut session =
        ExecSession::connect(&args.base, &args.sandbox, &args.api_key, params).await?;

    // Spawn a stdin → session pump task. Each newline-delimited
    // local line becomes one stdin frame. When local stdin
    // closes, half-close the session's stdin.
    if args.once {
        // Canned script: echo, do a tiny pipe, then exit 0.
        let canned = b"echo opens-andbox-shell-hello\nuname -a\nexit 0\n";
        session.send_stdin(&canned[..]).await?;
        session.close_stdin().await?;
    } else {
        // Live tty bridge. Forward local stdin line-by-line
        // until EOF, then half-close.
        let stdin = BufReader::new(tokio::io::stdin());
        let mut lines = stdin.lines();
        // Spawn pump so we can also poll session frames
        // concurrently. The pump owns a clone-free move of the
        // line reader; on local EOF it half-closes the session.
        // We use a oneshot to surface fatal errors back to main.
        let (err_tx, err_rx) = tokio::sync::oneshot::channel::<Box<dyn std::error::Error + Send + Sync>>();
        // ExecSession isn't Clone — but send_stdin / close_stdin
        // only need &mut self. We need to share access, so we
        // route stdin lines through a channel that main pulls
        // from in the main select loop below.
        let (line_tx, mut line_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        tokio::spawn(async move {
            loop {
                match lines.next_line().await {
                    Ok(Some(mut s)) => {
                        s.push('\n');
                        if line_tx.send(s.into_bytes()).await.is_err() {
                            return;
                        }
                    }
                    Ok(None) => return, // EOF
                    Err(e) => {
                        let _ = err_tx.send(Box::new(e));
                        return;
                    }
                }
            }
        });

        let mut stdin_half_closed = false;
        let mut err_rx = err_rx;
        loop {
            tokio::select! {
                line = line_rx.recv() => match line {
                    Some(bytes) => session.send_stdin(bytes).await?,
                    None => {
                        if !stdin_half_closed {
                            session.close_stdin().await?;
                            stdin_half_closed = true;
                        }
                    }
                },
                e = &mut err_rx => {
                    if let Ok(boxed) = e { return Err(boxed); }
                }
                frame = session.next_frame() => {
                    match frame? {
                        None => return Ok(()),
                        Some(f) => if handle_frame(f) {
                            return Ok(());
                        }
                    }
                }
            }
        }
    }

    // --once path: drain frames until terminal.
    let mut stdout = tokio::io::stdout();
    while let Some(frame) = session.next_frame().await? {
        match frame {
            ServerFrame::Stdout(bytes) => stdout.write_all(&bytes).await?,
            ServerFrame::Stderr(bytes) => {
                let mut stderr = tokio::io::stderr();
                stderr.write_all(&bytes).await?;
            }
            ServerFrame::Exited { exit_code, .. } => std::process::exit(exit_code),
            ServerFrame::Error { code, detail } => {
                eprintln!("error: {code}: {detail}");
                std::process::exit(1);
            }
            ServerFrame::Started { .. } => {}
        }
    }
    Ok(())
}

/// Returns true if the frame is terminal and the caller should
/// exit the loop.
fn handle_frame(frame: ServerFrame) -> bool {
    match frame {
        ServerFrame::Stdout(bytes) => {
            print!("{}", String::from_utf8_lossy(&bytes));
            false
        }
        ServerFrame::Stderr(bytes) => {
            eprint!("{}", String::from_utf8_lossy(&bytes));
            false
        }
        ServerFrame::Exited { exit_code, .. } => {
            std::process::exit(exit_code);
        }
        ServerFrame::Error { code, detail } => {
            eprintln!("error: {code}: {detail}");
            std::process::exit(1);
        }
        ServerFrame::Started { .. } => false,
    }
}
