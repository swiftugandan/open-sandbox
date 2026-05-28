//! Minimal exec example: run `echo hello` in the sandbox, print
//! stdout, exit with the in-container process's exit code.
//!
//! Run against the docker-compose.full.yml stack:
//!
//! ```bash
//! SB=$(curl -s -X POST http://localhost:18081/v1/sandboxes \
//!        -H 'Authorization: Bearer e2e-api-key' \
//!        -H 'content-type: application/json' \
//!        -d '{"image":"alpine"}' | jq -r .sandbox_id)
//! # wait ~32s for the proxy routing cache to refresh
//! cargo run -p open-sandbox-ws-client --example echo -- --sandbox "$SB"
//! ```

use clap::Parser;
use open_sandbox_ws_client::{ExecParams, ExecSession, ServerFrame};

#[derive(Parser)]
struct Args {
    /// API gateway base URL (ws:// or wss://).
    #[arg(long, default_value = "ws://localhost:18081")]
    base: String,
    /// Sandbox UUID returned by POST /v1/sandboxes.
    #[arg(long)]
    sandbox: String,
    /// API key for the Authorization: Bearer header.
    #[arg(long, default_value = "e2e-api-key")]
    api_key: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let params = ExecParams::new(vec!["echo".into(), "hello from the sandbox".into()]);
    let mut session =
        ExecSession::connect(&args.base, &args.sandbox, &args.api_key, params).await?;

    while let Some(frame) = session.next_frame().await? {
        match frame {
            ServerFrame::Stdout(bytes) => print!("{}", String::from_utf8_lossy(&bytes)),
            ServerFrame::Stderr(bytes) => eprint!("{}", String::from_utf8_lossy(&bytes)),
            ServerFrame::Exited { exit_code, .. } => std::process::exit(exit_code),
            ServerFrame::Error { code, detail } => {
                eprintln!("error: {code}: {detail}");
                std::process::exit(1);
            }
            ServerFrame::Started { .. } => {}
            // v1.0.3 sidecar frames; never emitted on exec sessions.
            ServerFrame::ListDirResult { .. }
            | ServerFrame::WaitPortListeningResult { .. }
            | ServerFrame::FileMeta { .. } => {}
        }
    }
    Ok(())
}
