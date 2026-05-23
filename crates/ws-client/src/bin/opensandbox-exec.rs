//! `opensandbox-exec` — CLI wrapper around the Open Sandbox
//! streaming exec WebSocket API. Useful for shell-level scripting
//! and as a demonstration of the v1.0 protocol.
//!
//! Usage:
//!   opensandbox-exec --base ws://localhost:8081 \
//!                    --sandbox <uuid> \
//!                    --api-key $KEY \
//!                    -- bash -c 'echo hello && date'

use std::io::Write;
use std::process::ExitCode;

use clap::Parser;
use open_sandbox_ws_client::{ExecParams, ExecSession, ServerFrame};

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
    /// Command and arguments to run.
    #[arg(trailing_var_arg = true, required = true)]
    command: Vec<String>,
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    let mut params = ExecParams::new(args.command);
    if let Some(cwd) = args.cwd {
        params = params.cwd(cwd);
    }
    let mut session = match ExecSession::connect(&args.base, &args.sandbox, &args.api_key, params)
        .await
    {
        Ok(s) => s,
        Err(e) => {
            eprintln!("connect: {e}");
            return ExitCode::from(2);
        }
    };

    let exit_code: i32;
    loop {
        match session.next_frame().await {
            Ok(Some(ServerFrame::Started { exec_id, in_container_pid })) => {
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
                return ExitCode::from(3);
            }
            Ok(None) => {
                eprintln!("# session closed without exit");
                return ExitCode::from(4);
            }
            Err(e) => {
                eprintln!("# i/o: {e}");
                return ExitCode::from(5);
            }
        }
    }

    ExitCode::from((exit_code & 0xff) as u8)
}
