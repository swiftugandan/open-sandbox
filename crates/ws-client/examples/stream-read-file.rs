//! Stream a file out of a sandbox via the WS read endpoint.
//!
//! Wire:
//!
//!   GET ws://gateway/v1/sandboxes/{id}/files/read-stream?path=<urlencoded>
//!   Header: Authorization: Bearer <api-key>
//!
//! Server sends raw file bytes as WS Binary frames and closes
//! with code 1000 on EOF or a 44xx-range code on failure.
//!
//! ```bash
//! cargo run -p open-sandbox-ws-client --example stream-read-file \
//!     -- --sandbox "$SB" --path /tmp/foo > /tmp/got
//! ```

use clap::Parser;
use open_sandbox_ws_client::ReadFileSession;
use tokio::io::AsyncWriteExt;

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "ws://localhost:18081")]
    base: String,
    #[arg(long)]
    sandbox: String,
    #[arg(long, default_value = "e2e-api-key")]
    api_key: String,
    #[arg(long)]
    path: String,
    #[arg(long)]
    cwd: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args = Args::parse();
    let mut session = ReadFileSession::connect(
        &args.base,
        &args.sandbox,
        &args.api_key,
        &args.path,
        args.cwd.as_deref(),
    )
    .await?;

    let mut stdout = tokio::io::stdout();
    while let Some(chunk) = session.next_chunk().await? {
        stdout.write_all(&chunk).await?;
    }
    stdout.flush().await?;
    Ok(())
}
