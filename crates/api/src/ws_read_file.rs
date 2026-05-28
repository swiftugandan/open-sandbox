//! WebSocket file-read endpoint.
//!
//! `GET /v1/sandboxes/{id}/files/read-stream?path=<urlencoded>[&cwd=...]`
//! with `Upgrade: websocket`.
//!
//! Wire shape (server → client only — client sends no frames after
//! the upgrade handshake):
//!
//! - v1.0.3: an optional first WS Text frame carrying a JSON
//!   `{ "revision": "<opaque>", "size": <u64> }` sidecar — emitted
//!   when the agent supports `stat_revision`. Clients that don't
//!   care about revisions (older ws-client builds) ignore Text
//!   frames and consume only Binary; backwards compatible.
//! - WS Binary frames carrying raw file bytes, in the order the
//!   agent emits its 64 KiB stdout chunks.
//! - WS Close on terminal status:
//!   * `1000` — clean EOF (file read complete).
//!   * `4404 FILE_NOT_FOUND` — path resolved but file missing.
//!   * `4404 SANDBOX_GONE` — sandbox no longer exists.
//!   * `4500 READ_FAILED` — other runtime failure (e.g. permission
//!     denied, mount unavailable).
//!   * `4503 PROXY_UNAVAILABLE` — proxy/agent disconnected mid-read.
//!
//! Auth failures are surfaced *before* the upgrade as an HTTP 401 with
//! the standard `{error, error_code: UNAUTHORIZED}` JSON body — they
//! never produce a WS close frame because the WebSocket is never
//! established.
//!
//! Hosted on a distinct path from the unary `GET /files/read`
//! handler because axum (v0.8) can't reliably extract
//! `Option<WebSocketUpgrade>` alongside other extractors while
//! axum v0.7 is also in the dependency graph (pulled transitively
//! by tonic). The two-path split is a build-time workaround, not
//! a wire-protocol decision — both endpoints expose the same
//! `IoStart::ReadFile` flow on the agent.

use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path, Query, State, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use tokio::sync::mpsc;
use tracing::{info, warn};

use open_sandbox_contracts::error::ApiError;
use open_sandbox_contracts::proxy::{
    IoClientFrame, IoStart, ReadFileParams, io_client_frame, io_server_frame,
};
use open_sandbox_contracts::types::SandboxId;

use crate::proxy_client::SharedProxyClient;
use crate::service::{ReadFileQuery, SandboxService};
use crate::state::ApiState;

/// Build the IoStart for a ReadFile session.
fn build_start(sandbox_id: &SandboxId, path: String, cwd: String) -> IoStart {
    IoStart {
        sandbox_id: sandbox_id.to_string(),
        params: Some(open_sandbox_contracts::proxy::io_start::Params::ReadFile(
            ReadFileParams { path, cwd },
        )),
    }
}

/// Axum handler for `GET /v1/sandboxes/{id}/files/read-stream` (WS
/// upgrade). Validates auth + sandbox_id + query before upgrade;
/// returns HTTP 400/401 if any check fails.
pub async fn ws_read_file<S: SandboxService>(
    State(state): State<Arc<ApiState<S>>>,
    Path(id): Path<String>,
    Query(query): Query<ReadFileQuery>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let Ok(uuid) = uuid::Uuid::parse_str(&id) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "INVALID_REQUEST",
            "invalid sandbox_id",
        );
    };
    let sandbox_id = SandboxId::from(uuid);
    if query.path.is_empty() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "INVALID_REQUEST",
            "path query parameter is required",
        );
    }
    // Loop verify-fix iter 6 (2026-05-26): the unary read/write handlers
    // gate path through `handlers::validate_sandbox_path` (NUL, control
    // chars, `..` segments) as defense-in-depth. The WS read-stream
    // handler was missing the same check, so a relative `../../etc/passwd`
    // streamed successfully while the unary equivalent returned 400. The
    // bytes returned are still inside the sandbox container (so it's not
    // a tenant escape today), but the policy was inconsistent and a
    // regression in the agent's resolver would have escalated. Match the
    // unary policy.
    if let Err(msg) = crate::handlers::validate_sandbox_path(&query.path) {
        return error_response(StatusCode::BAD_REQUEST, "INVALID_REQUEST", msg);
    }
    if let Some(cwd) = &query.cwd
        && !cwd.is_empty()
        && let Err(msg) = crate::handlers::validate_sandbox_path(cwd)
    {
        return error_response(StatusCode::BAD_REQUEST, "INVALID_REQUEST", msg);
    }
    let echo_protocol = match check_auth(&headers, &state.api_key) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let proxy = state.proxy.clone();
    let path = query.path;
    let cwd = query.cwd.unwrap_or_default();
    let upgrade = if let Some(p) = echo_protocol {
        ws.protocols([p])
    } else {
        ws
    };
    upgrade.on_upgrade(move |socket| run_session(socket, proxy, sandbox_id, path, cwd))
}

#[allow(clippy::result_large_err)]
fn check_auth(headers: &HeaderMap, expected: &str) -> Result<Option<String>, Response> {
    crate::handlers::check_ws_auth(headers, expected)
}

fn error_response(status: StatusCode, code: &str, msg: &str) -> Response {
    let body = serde_json::json!({"error": msg, "error_code": code});
    (status, axum::Json(body)).into_response()
}

async fn run_session(
    socket: WebSocket,
    proxy: SharedProxyClient,
    sandbox_id: SandboxId,
    path: String,
    cwd: String,
) {
    let gateway_session_id = uuid::Uuid::new_v4();
    info!(
        sandbox_id = %sandbox_id,
        gateway_session_id = %gateway_session_id,
        path = %path,
        "ws_read_file.session_started"
    );

    // Open the proxy stream and send the IoStart upfront. ReadFile
    // is a server-driven flow: no further client frames expected.
    let (client_tx, client_rx) = mpsc::channel::<IoClientFrame>(8);
    let start = build_start(&sandbox_id, path, cwd);
    if client_tx
        .send(IoClientFrame {
            stream_id: String::new(),
            payload: Some(io_client_frame::Payload::Start(start)),
        })
        .await
        .is_err()
    {
        send_close(socket, 4500, "internal channel closed").await;
        return;
    }
    drop(client_tx);

    let server_rx = match proxy.open_io_stream(client_rx).await {
        Ok(rx) => rx,
        Err(e) => {
            let (code, reason) = close_for_api_error(&e);
            send_close(socket, code, &reason).await;
            return;
        }
    };

    pump(socket, server_rx, sandbox_id, gateway_session_id).await;
}

async fn pump(
    mut socket: WebSocket,
    mut server_rx: mpsc::Receiver<Result<open_sandbox_contracts::proxy::IoServerFrame, ApiError>>,
    sandbox_id: SandboxId,
    gateway_session_id: uuid::Uuid,
) {
    let mut bytes_sent: u64 = 0;
    while let Some(frame_res) = server_rx.recv().await {
        let frame = match frame_res {
            Ok(f) => f,
            Err(e) => {
                let (code, reason) = close_for_api_error(&e);
                send_close_via(&mut socket, code, &reason).await;
                return;
            }
        };
        match frame.payload {
            Some(io_server_frame::Payload::Stdout(chunk)) => {
                bytes_sent += chunk.len() as u64;
                if socket
                    .send(Message::Binary(Bytes::copy_from_slice(&chunk)))
                    .await
                    .is_err()
                {
                    // Client gone — nothing left to do.
                    return;
                }
            }
            // v1.0.3: surface the revision sidecar as a single
            // JSON Text frame ahead of the first Binary chunk. The
            // agent guarantees FileMeta arrives before the first
            // Stdout chunk (CONTRACTS.md), so we don't need to
            // buffer-and-prepend on the gateway.
            Some(io_server_frame::Payload::FileMeta(m)) => {
                let body = serde_json::json!({
                    "revision": m.revision,
                    "size": m.size,
                });
                if socket
                    .send(Message::Text(body.to_string().into()))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            // The read_file path on the agent never emits stderr,
            // but defensively drop it if it ever appears.
            Some(io_server_frame::Payload::Stderr(_)) => continue,
            Some(io_server_frame::Payload::Exited(e)) => {
                if e.exit_code == 0 {
                    info!(
                        sandbox_id = %sandbox_id,
                        gateway_session_id = %gateway_session_id,
                        bytes_sent,
                        "ws_read_file.eof"
                    );
                    send_close_via(&mut socket, 1000, "EOF").await;
                } else {
                    warn!(
                        sandbox_id = %sandbox_id,
                        exit_code = e.exit_code,
                        "ws_read_file.exited_nonzero"
                    );
                    send_close_via(
                        &mut socket,
                        4500,
                        &format!("read exited with {}", e.exit_code),
                    )
                    .await;
                }
                return;
            }
            Some(io_server_frame::Payload::Error(err)) => {
                let api_err = match err.code.as_str() {
                    "FILE_NOT_FOUND" => ApiError::FileNotFound {
                        resolved_path: err.detail.clone(),
                    },
                    "SANDBOX_GONE" => ApiError::SandboxGone {
                        sandbox_id: err.detail.clone(),
                    },
                    _ => ApiError::IoStreamFailed {
                        detail: format!("{}: {}", err.code, err.detail),
                    },
                };
                let (code, reason) = close_for_api_error(&api_err);
                send_close_via(&mut socket, code, &reason).await;
                return;
            }
            _ => {}
        }
    }
    // Proxy stream ended without a terminal frame.
    warn!(
        sandbox_id = %sandbox_id,
        gateway_session_id = %gateway_session_id,
        "ws_read_file.proxy_stream_ended_early"
    );
    send_close_via(
        &mut socket,
        4503,
        "proxy stream ended without terminal frame",
    )
    .await;
}

fn close_for_api_error(e: &ApiError) -> (u16, String) {
    match e {
        ApiError::FileNotFound { resolved_path } => {
            (4404, format!("FILE_NOT_FOUND: {resolved_path}"))
        }
        ApiError::SandboxGone { sandbox_id } => (4404, format!("SANDBOX_GONE: {sandbox_id}")),
        ApiError::ProxyUnavailable { .. } => (4503, e.to_string()),
        _ => (4500, e.to_string()),
    }
}

async fn send_close(mut socket: WebSocket, code: u16, reason: &str) {
    send_close_via(&mut socket, code, reason).await;
}

async fn send_close_via(socket: &mut WebSocket, code: u16, reason: &str) {
    let _ = socket
        .send(Message::Close(Some(axum::extract::ws::CloseFrame {
            code,
            reason: reason.to_string().into(),
        })))
        .await;
}
