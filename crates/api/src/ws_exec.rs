//! WebSocket exec endpoint.
//!
//! `GET /v1/sandboxes/{id}/exec` with `Upgrade: websocket`.
//!
//! Auth: `Authorization: Bearer <api_key>` on the upgrade request,
//! validated BEFORE the socket is established. Failed auth → HTTP
//! 401, no upgrade.
//!
//! On upgrade, the handler opens an `OpenIoStream` call via the
//! `ProxyClientPool` and pumps frames in both directions until the
//! session terminates.
//!
//! Idle keepalive: gateway-initiated WebSocket Ping every
//! WS_IDLE_PING_INTERVAL (30s); peer-gone after WS_IDLE_PING_TIMEOUT
//! (60s) of no Pong → cleanup (per spike 03).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path, State, WebSocketUpgrade};
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::StreamExt;
use tokio::sync::mpsc;
use tracing::{info, warn};

use open_sandbox_contracts::constants::{WS_IDLE_PING_INTERVAL, WS_IDLE_PING_TIMEOUT};
use open_sandbox_contracts::proxy::{IoClientFrame, IoServerFrame};
use open_sandbox_contracts::types::SandboxId;

use crate::frame;
use crate::proxy_client::SharedProxyClient;
use crate::service::SandboxService;
use crate::state::ApiState;

const AUTH_HEADER: &str = "authorization";

/// Axum handler for `GET /v1/sandboxes/{id}/exec` (upgrade).
pub async fn ws_exec<S: SandboxService>(
    State(state): State<Arc<ApiState<S>>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    // Validate sandbox_id.
    let Ok(uuid) = uuid::Uuid::parse_str(&id) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "INVALID_REQUEST",
            "invalid sandbox_id",
        );
    };
    let sandbox_id = SandboxId::from(uuid);

    // Validate auth BEFORE upgrade.
    if let Err(resp) = check_auth(&headers, &state.api_key) {
        return resp;
    }

    info!(
        sandbox_id = %sandbox_id,
        "ws.upgrade_authorized"
    );

    let proxy = state.proxy.clone();
    ws.on_upgrade(move |socket| run_session(socket, proxy, sandbox_id))
}

#[allow(clippy::result_large_err)]
fn check_auth(headers: &HeaderMap, expected: &str) -> Result<(), Response> {
    let got = headers
        .get(AUTH_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "));
    match got {
        // Comp-6: constant-time compare (see handlers.rs::constant_time_eq).
        Some(token)
            if crate::handlers::constant_time_eq(token.as_bytes(), expected.as_bytes()) =>
        {
            Ok(())
        }
        Some(_) => Err(error_response(
            StatusCode::UNAUTHORIZED,
            "UNAUTHORIZED",
            "invalid API key",
        )),
        None => Err(error_response(
            StatusCode::UNAUTHORIZED,
            "UNAUTHORIZED",
            "missing Authorization: Bearer header",
        )),
    }
}

fn error_response(status: StatusCode, code: &str, msg: &str) -> Response {
    let body = serde_json::json!({"error": msg, "error_code": code});
    (status, axum::Json(body)).into_response()
}

async fn run_session(socket: WebSocket, proxy: SharedProxyClient, sandbox_id: SandboxId) {
    let stream_id_for_log = uuid::Uuid::new_v4();
    info!(
        sandbox_id = %sandbox_id,
        gateway_session_id = %stream_id_for_log,
        "ws.session_started"
    );

    // Build the client-side channel feeding into the proxy.
    let (client_tx, client_rx) = mpsc::channel::<IoClientFrame>(32);

    // Open the proxy I/O stream. The first frame (IoStart) is
    // pushed by the WS-reader task once the client sends `KIND_START`.
    let server_rx = match proxy.open_io_stream(client_rx).await {
        Ok(rx) => rx,
        Err(e) => {
            let close_code = match &e {
                open_sandbox_contracts::error::ApiError::SandboxGone { .. } => 4404,
                open_sandbox_contracts::error::ApiError::ProxyUnavailable { .. } => 4503,
                _ => 4500,
            };
            let mut socket = socket;
            let _ = socket
                .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                    code: close_code,
                    reason: e.to_string().into(),
                })))
                .await;
            return;
        }
    };

    pump_ws_session(socket, client_tx, server_rx, sandbox_id, stream_id_for_log).await;
}

async fn pump_ws_session(
    socket: WebSocket,
    client_tx: mpsc::Sender<IoClientFrame>,
    mut server_rx: mpsc::Receiver<Result<IoServerFrame, open_sandbox_contracts::error::ApiError>>,
    sandbox_id: SandboxId,
    gateway_session_id: uuid::Uuid,
) {
    let (mut sender, mut receiver) = socket.split();
    let last_pong = Arc::new(Mutex::new(Instant::now()));

    // Task A: WS → client_tx (decode WS binary frames, push to proxy).
    let client_tx_task = client_tx.clone();
    let last_pong_recv = last_pong.clone();
    let recv_task = tokio::spawn(async move {
        while let Some(msg) = receiver.next().await {
            let Ok(msg) = msg else { break };
            match msg {
                Message::Binary(bytes) => match frame::decode_client(&bytes) {
                    Ok(payload) => {
                        let frame = frame::build_client_frame(payload);
                        if client_tx_task.send(frame).await.is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        warn!(gateway_session_id = %gateway_session_id, error = %e, "ws: client sent malformed frame");
                        return;
                    }
                },
                Message::Text(_) => {
                    warn!(gateway_session_id = %gateway_session_id, "ws: text frame received (binary expected)");
                    return;
                }
                Message::Close(_) => return,
                Message::Pong(_) => {
                    *last_pong_recv.lock().unwrap() = Instant::now();
                }
                Message::Ping(_) => {
                    // axum auto-responds to Pings on its own.
                }
            }
        }
    });

    // Task B: server_rx → WS (encode IoServerFrame to WS binary).
    //         Also runs the keepalive ping/pong timer concurrently.
    let mut ping_interval = tokio::time::interval(WS_IDLE_PING_INTERVAL);
    ping_interval.reset();

    // Wrap in Option so the JoinHandle is consumed exactly once.
    // Polling a completed JoinHandle panics ("JoinHandle polled
    // after completion"), so the select arm pulls the handle out
    // of the Option when it fires, leaving subsequent select
    // iterations and the post-loop wait both with `None`.
    let mut recv_task_handle: Option<tokio::task::JoinHandle<()>> = Some(recv_task);
    loop {
        tokio::select! {
            biased;
            // If the WS recv task ends (client closed the socket
            // or transport error) bring the session down
            // immediately — don't wait up to 30s for the next
            // keepalive ping to fail. Without this the agent
            // doesn't see the synthetic IoClose until after the
            // next ping → its ExecRegistry cleanup is delayed
            // accordingly.
            res = async { recv_task_handle.as_mut().unwrap().await }, if recv_task_handle.is_some() => {
                let _ = res;
                recv_task_handle = None;
                break;
            }
            frame = server_rx.recv() => match frame {
                None => break,
                Some(Err(e)) => {
                    use futures_util::SinkExt;
                    warn!(
                        gateway_session_id = %gateway_session_id,
                        error = %e,
                        "ws: proxy stream errored"
                    );
                    let _ = sender.send(Message::Close(Some(axum::extract::ws::CloseFrame {
                        code: 4500,
                        reason: e.to_string().into(),
                    }))).await;
                    break;
                }
                Some(Ok(server_frame)) => {
                    use futures_util::SinkExt;
                    let bytes = match frame::encode_server(&server_frame) {
                        Ok(b) => b,
                        Err(e) => {
                            warn!(gateway_session_id = %gateway_session_id, error = %e, "ws: failed to encode server frame");
                            break;
                        }
                    };
                    if sender.send(Message::Binary(bytes)).await.is_err() {
                        break;
                    }

                    // If this was a terminal frame (Exited/Error),
                    // we can close cleanly.
                    if matches!(
                        server_frame.payload,
                        Some(open_sandbox_contracts::proxy::io_server_frame::Payload::Exited(_))
                          | Some(open_sandbox_contracts::proxy::io_server_frame::Payload::Error(_))
                    ) {
                        let _ = sender.send(Message::Close(None)).await;
                        break;
                    }
                }
            },
            _ = ping_interval.tick() => {
                use futures_util::SinkExt;
                if sender.send(Message::Ping(Bytes::new())).await.is_err() {
                    break;
                }
                let elapsed = last_pong.lock().unwrap().elapsed();
                if elapsed > WS_IDLE_PING_TIMEOUT {
                    warn!(
                        gateway_session_id = %gateway_session_id,
                        elapsed_ms = elapsed.as_millis() as u64,
                        "ws.idle_ping_timeout"
                    );
                    let _ = sender.send(Message::Close(Some(axum::extract::ws::CloseFrame {
                        code: 4408,
                        reason: "ping timeout".into(),
                    }))).await;
                    break;
                }
            }
        }
    }

    // Close the upstream by dropping client_tx → proxy sees end of
    // client stream → agent cleans up.
    drop(client_tx);
    // Wait briefly for the recv task to finish (it observes WS close).
    // Only awaitable if it hasn't already been consumed in the loop.
    if let Some(h) = recv_task_handle {
        let _ = tokio::time::timeout(Duration::from_secs(2), h).await;
    }

    info!(
        sandbox_id = %sandbox_id,
        gateway_session_id = %gateway_session_id,
        "ws.client_disconnected"
    );
}
