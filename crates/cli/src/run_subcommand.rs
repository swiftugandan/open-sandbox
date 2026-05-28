//! `open-sandbox run` — `docker run --rm` for the sandbox fleet.
//!
//! Flow: POST /v1/sandboxes → poll until `running` → open
//! WS /v1/sandboxes/{id}/exec → stream stdin/stdout/stderr → on
//! IoExited, propagate the exit code → DELETE /v1/sandboxes/{id}.
//!
//! The delete runs in every exit path (clean exit, remote error,
//! local Ctrl-C, signal). The api gateway's exec layer already
//! SIGTERMs the in-container process on WS close per the v1.0
//! exec-streaming design, so dropping the WS is sufficient to stop
//! the workload — the explicit DELETE then frees the sandbox slot.

use std::io::{Read, Write};
use std::process::ExitCode;
use std::time::Duration;

use open_sandbox_ws_client::{ExecParams, ExecSession, ServerFrame, WsClientError};
use tokio::sync::mpsc;

use crate::cli::RunArgs;

// CLI-level exit codes, kept in the 120..=127 / 128+ band so they
// don't collide with legitimate sandbox process exit codes
// (timeout(1)/sudo/env convention; matches opensandbox-exec).
const EXIT_CREATE_FAILED: u8 = 120;
const EXIT_START_TIMEOUT: u8 = 121;
const EXIT_SANDBOX_FAILED: u8 = 122;
const EXIT_CONNECT_FAILED: u8 = 124;
const EXIT_REMOTE_ERROR: u8 = 125;
const EXIT_SESSION_BROKEN: u8 = 126;
const EXIT_IO_ERROR: u8 = 127;

#[derive(Debug, serde::Serialize)]
struct CreateBody {
    image: String,
    cpu_millicores: u32,
    memory_bytes: u64,
    env_vars: std::collections::HashMap<String, String>,
    exposed_port: u32,
    pull_policy: open_sandbox_contracts::types::PullPolicy,
}

#[derive(Debug, serde::Deserialize)]
struct CreateResp {
    sandbox_id: String,
}

#[derive(Debug, serde::Deserialize)]
struct GetResp {
    #[serde(default)]
    status: String,
    #[serde(default)]
    error: Option<String>,
}

pub async fn run(args: RunArgs) -> ExitCode {
    let api_base = args.api_base.trim_end_matches('/').to_string();
    let api_key = args.api_key.into_inner();
    if api_key.is_empty() {
        eprintln!("error: OPEN_SANDBOX_API_KEY (or --api-key) must be non-empty");
        return ExitCode::from(EXIT_CREATE_FAILED);
    }

    let http = match reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(30))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: build http client: {e}");
            return ExitCode::from(EXIT_CREATE_FAILED);
        }
    };

    let env_vars: std::collections::HashMap<String, String> = args.env.into_iter().collect();
    let create_body = CreateBody {
        image: args.image,
        cpu_millicores: args.cpu_millicores,
        memory_bytes: args.memory_bytes,
        env_vars,
        // `run` workloads are one-off commands, not HTTP servers. Don't
        // claim a public subdomain (saves the proxy a routing entry and
        // sidesteps the cold-pull-then-readiness-probe path).
        exposed_port: 0,
        pull_policy: args.pull_policy.unwrap_or_default(),
    };

    let sandbox_id = match create_sandbox(&http, &api_base, &api_key, &create_body).await {
        Ok(id) => id,
        Err(e) => {
            eprintln!("error: create sandbox: {e}");
            return ExitCode::from(EXIT_CREATE_FAILED);
        }
    };
    eprintln!("# sandbox {sandbox_id} created; waiting for running");

    match wait_for_running(
        &http,
        &api_base,
        &api_key,
        &sandbox_id,
        Duration::from_secs(args.start_timeout_secs),
    )
    .await
    {
        Ok(()) => {}
        Err(WaitError::Timeout) => {
            eprintln!(
                "error: sandbox {sandbox_id} did not reach `running` within {}s",
                args.start_timeout_secs,
            );
            cleanup(&http, &api_base, &api_key, &sandbox_id).await;
            return ExitCode::from(EXIT_START_TIMEOUT);
        }
        Err(WaitError::Failed(detail)) => {
            eprintln!("error: sandbox {sandbox_id} failed: {detail}");
            cleanup(&http, &api_base, &api_key, &sandbox_id).await;
            return ExitCode::from(EXIT_SANDBOX_FAILED);
        }
        Err(WaitError::Http(e)) => {
            eprintln!("error: polling sandbox status: {e}");
            cleanup(&http, &api_base, &api_key, &sandbox_id).await;
            return ExitCode::from(EXIT_CREATE_FAILED);
        }
    }

    let exit = stream_exec(&api_base, &api_key, &sandbox_id, args.command, args.cwd).await;
    cleanup(&http, &api_base, &api_key, &sandbox_id).await;
    exit
}

async fn create_sandbox(
    http: &reqwest::Client,
    api_base: &str,
    api_key: &str,
    body: &CreateBody,
) -> Result<String, String> {
    let resp = http
        .post(format!("{api_base}/v1/sandboxes"))
        .bearer_auth(api_key)
        .json(body)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("HTTP {status}: {text}"));
    }
    let parsed: CreateResp = serde_json::from_str(&text)
        .map_err(|e| format!("malformed create response: {e}: {text}"))?;
    Ok(parsed.sandbox_id)
}

enum WaitError {
    Timeout,
    Failed(String),
    Http(String),
}

async fn wait_for_running(
    http: &reqwest::Client,
    api_base: &str,
    api_key: &str,
    sandbox_id: &str,
    budget: Duration,
) -> Result<(), WaitError> {
    let deadline = tokio::time::Instant::now() + budget;
    let mut delay = Duration::from_millis(250);
    let cap = Duration::from_secs(2);
    loop {
        let resp = http
            .get(format!("{api_base}/v1/sandboxes/{sandbox_id}"))
            .bearer_auth(api_key)
            .send()
            .await
            .map_err(|e| WaitError::Http(e.to_string()))?;
        let s = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| WaitError::Http(e.to_string()))?;
        if !s.is_success() {
            return Err(WaitError::Http(format!("HTTP {s}: {text}")));
        }
        let parsed: GetResp = serde_json::from_str(&text)
            .map_err(|e| WaitError::Http(format!("malformed get response: {e}: {text}")))?;
        match parsed.status.as_str() {
            "running" => return Ok(()),
            "failed" | "stopped" => {
                return Err(WaitError::Failed(
                    parsed.error.unwrap_or_else(|| parsed.status.clone()),
                ));
            }
            _ => {}
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(WaitError::Timeout);
        }
        let remaining = deadline.saturating_duration_since(now);
        tokio::time::sleep(delay.min(remaining)).await;
        delay = (delay * 2).min(cap);
    }
}

async fn cleanup(http: &reqwest::Client, api_base: &str, api_key: &str, sandbox_id: &str) {
    match http
        .delete(format!("{api_base}/v1/sandboxes/{sandbox_id}"))
        .bearer_auth(api_key)
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => {}
        Ok(r) => eprintln!(
            "# warning: delete sandbox {sandbox_id} returned HTTP {}",
            r.status()
        ),
        Err(e) => eprintln!("# warning: delete sandbox {sandbox_id} failed: {e}"),
    }
}

enum StdinMsg {
    Chunk(Vec<u8>),
    Eof,
}

async fn stream_exec(
    api_base: &str,
    api_key: &str,
    sandbox_id: &str,
    command: Vec<String>,
    cwd: Option<String>,
) -> ExitCode {
    let ws_base = http_to_ws(api_base);
    let mut params = ExecParams::new(command);
    if let Some(c) = cwd {
        params = params.cwd(c);
    }

    let mut session = match ExecSession::connect(&ws_base, sandbox_id, api_key, params).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("# connect: {e}");
            return ExitCode::from(EXIT_CONNECT_FAILED);
        }
    };

    // Forward local stdin only when piped/redirected (i.e. not a TTY).
    // Matches the auto-mode default in opensandbox-exec.
    let forward_stdin = !is_stdin_tty();
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

    // Ctrl-C → forward as SIGINT to the remote process. Dropping the
    // WebSocket on a second Ctrl-C is handled by tokio's default
    // SIGINT-terminates-the-process behavior plus the proxy's
    // close-triggers-SIGTERM contract — we don't need an explicit
    // second-handler escalation here.
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

    let exit_code: i32;
    loop {
        let frame_res = if stdin_done {
            tokio::select! {
                f = session.next_frame() => f,
                Some(()) = sigint_rx.recv() => {
                    let _ = session.send_signal(2).await;
                    continue;
                }
            }
        } else {
            tokio::select! {
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
            }
        };

        match frame_res {
            Ok(Some(ServerFrame::Started { .. })) => {}
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
                return ExitCode::from(EXIT_REMOTE_ERROR);
            }
            Ok(Some(other)) => {
                // v1.0.3 sidecar frames (ListDirResult, WaitPortListeningResult,
                // FileMeta) never appear on exec sessions; ignore defensively
                // rather than tear the run down on an out-of-band frame.
                eprintln!("# ignoring out-of-band server frame: {other:?}");
            }
            Ok(None) => {
                eprintln!("# session closed without exit");
                return ExitCode::from(EXIT_SESSION_BROKEN);
            }
            Err(WsClientError::ReadTimeout { timeout }) => {
                eprintln!("# read timeout after {timeout:?}");
                return ExitCode::from(EXIT_IO_ERROR);
            }
            Err(e) => {
                eprintln!("# i/o: {e}");
                return ExitCode::from(EXIT_IO_ERROR);
            }
        }
    }

    ExitCode::from((exit_code & 0xff) as u8)
}

fn http_to_ws(base: &str) -> String {
    if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        // Already ws/wss, or scheme-less — pass through. The ws-client
        // will surface the error on connect if the scheme is wrong.
        base.to_string()
    }
}

fn is_stdin_tty() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_to_ws_swaps_scheme() {
        assert_eq!(http_to_ws("http://localhost:8081"), "ws://localhost:8081");
        assert_eq!(
            http_to_ws("https://api.example.com"),
            "wss://api.example.com"
        );
        assert_eq!(http_to_ws("ws://x"), "ws://x");
        assert_eq!(http_to_ws("wss://x"), "wss://x");
    }
}
