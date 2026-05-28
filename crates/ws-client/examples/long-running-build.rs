//! Long-running exec: simulates a build by sleeping past the
//! legacy 60s `EXEC_TIMEOUT` ceiling, with periodic progress
//! output. Demonstrates that exec sessions are not bounded by a
//! client-visible timeout in v1.0 — the connection IS the
//! lifetime.
//!
//! ```bash
//! cargo run -p open-sandbox-ws-client --example long-running-build \
//!     -- --sandbox "$SB"
//! ```

use clap::Parser;
use open_sandbox_ws_client::{ExecParams, ExecSession, ServerFrame};

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "ws://localhost:18081")]
    base: String,
    #[arg(long)]
    sandbox: String,
    #[arg(long, default_value = "e2e-api-key")]
    api_key: String,
    /// Total simulated build duration in seconds. Defaults to 70
    /// to land past the legacy 60s ceiling.
    #[arg(long, default_value_t = 70u32)]
    seconds: u32,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    // sh -c '...': emit a progress line every ~10s, then "done".
    let script = format!(
        "for i in $(seq 1 {n}); do \
           echo \"build progress: step $i/{n}\"; \
           sleep 1; \
         done; \
         echo done",
        n = args.seconds
    );
    let params = ExecParams::new(vec!["sh".into(), "-c".into(), script]);
    let mut session =
        ExecSession::connect(&args.base, &args.sandbox, &args.api_key, params).await?;

    eprintln!(
        "# session opened; expecting up to {}s of output",
        args.seconds
    );
    while let Some(frame) = session.next_frame().await? {
        match frame {
            ServerFrame::Stdout(bytes) => print!("{}", String::from_utf8_lossy(&bytes)),
            ServerFrame::Stderr(bytes) => eprint!("{}", String::from_utf8_lossy(&bytes)),
            ServerFrame::Exited { exit_code, .. } => {
                eprintln!("# session ended exit={exit_code}");
                std::process::exit(exit_code);
            }
            ServerFrame::Error { code, detail } => {
                eprintln!("error: {code}: {detail}");
                std::process::exit(1);
            }
            ServerFrame::Started {
                in_container_pid, ..
            } => {
                eprintln!("# started in_container_pid={in_container_pid}");
            }
            // v1.0.3 sidecar frames; never emitted on exec sessions.
            ServerFrame::ListDirResult { .. }
            | ServerFrame::WaitPortListeningResult { .. }
            | ServerFrame::FileMeta { .. } => {}
        }
    }
    Ok(())
}
