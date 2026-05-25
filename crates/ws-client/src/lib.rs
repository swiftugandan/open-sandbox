//! Open Sandbox v1.0 streaming exec WebSocket client.
//!
//! Thin wrapper around tokio-tungstenite that speaks the
//! `| 1 byte kind | payload |` envelope and gives callers an
//! `ExecSession` shaped around the typical agent control loop:
//!
//! ```no_run
//! use open_sandbox_ws_client::{ExecSession, ExecParams};
//! # async fn ex() -> Result<(), Box<dyn std::error::Error>> {
//! let mut session = ExecSession::connect(
//!     "ws://localhost:8081",
//!     "00000000-0000-0000-0000-000000000001",
//!     "my-api-key",
//!     ExecParams::new(vec!["echo".into(), "hi".into()]),
//! )
//! .await?;
//! while let Some(frame) = session.next_frame().await? {
//!     match frame {
//!         open_sandbox_ws_client::ServerFrame::Stdout(bytes) => {
//!             print!("{}", String::from_utf8_lossy(&bytes));
//!         }
//!         open_sandbox_ws_client::ServerFrame::Exited { exit_code, .. } => {
//!             let _ = exit_code;
//!             return Ok(());
//!         }
//!         _ => {}
//!     }
//! }
//! # Ok(()) }
//! ```

use std::collections::HashMap;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use prost::Message;
use thiserror::Error;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use open_sandbox_contracts::proxy::{
    IoClose, IoError as IoErrorProto, IoExited, IoSignal, IoStart, IoStarted, io_start,
};

// Frame kinds — must match crates/api/src/frame.rs.
const KIND_START: u8 = 0x00;
const KIND_STDIN: u8 = 0x01;
const KIND_SIGNAL: u8 = 0x02;
const KIND_STDIN_EOF: u8 = 0x03;
const KIND_STDOUT: u8 = 0x11;
const KIND_STDERR: u8 = 0x12;
const KIND_EXITED: u8 = 0x13;
const KIND_ERROR: u8 = 0x14;
const KIND_STARTED: u8 = 0x15;

#[derive(Debug, Error)]
pub enum WsClientError {
    #[error("connect failed: {0}")]
    Connect(String),
    #[error("websocket I/O: {0}")]
    Io(String),
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("session closed by peer")]
    Closed,
    /// Comp-7: returned by next_frame when no server frame arrived
    /// within the configured read timeout.
    #[error("read timeout (no server frame within {timeout:?})")]
    ReadTimeout { timeout: std::time::Duration },
}

#[derive(Debug, Clone)]
pub struct ExecParams {
    pub command: Vec<String>,
    pub cwd: Option<String>,
    pub env: HashMap<String, String>,
}

impl ExecParams {
    pub fn new(command: Vec<String>) -> Self {
        Self {
            command,
            cwd: None,
            env: HashMap::new(),
        }
    }

    pub fn cwd(mut self, cwd: impl Into<String>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }
}

/// Server-sent frame variants. Stdout/Stderr are raw bytes;
/// terminal frames carry structured data.
#[derive(Debug)]
pub enum ServerFrame {
    Started {
        exec_id: String,
        in_container_pid: i32,
    },
    Stdout(Bytes),
    Stderr(Bytes),
    Exited {
        exit_code: i32,
        command_not_found: bool,
    },
    Error {
        code: String,
        detail: String,
    },
}

/// Comp-7: maximum WebSocket frame size the client accepts before
/// closing the session. Default 4 MiB matches typical reverse-proxy
/// limits; bump via [`ExecSession::set_max_frame_bytes`] if the
/// platform needs larger.
pub const DEFAULT_MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// Comp-7: default read timeout for [`ExecSession::next_frame`]. None
/// disables the timeout (legacy behavior). The server pings every 30s
/// per spike-03; setting this to ~60s catches a silently-broken
/// connection (NAT idle, middle-box drop) without false-positives.
pub const DEFAULT_READ_TIMEOUT: Option<std::time::Duration> = None;

pub struct ExecSession {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    read_timeout: Option<std::time::Duration>,
}

impl ExecSession {
    /// Open a streaming exec session.
    pub async fn connect(
        base_url: &str,
        sandbox_id: &str,
        api_key: &str,
        params: ExecParams,
    ) -> Result<Self, WsClientError> {
        // base_url is "ws://host:port" or "wss://host:port".
        let url = format!("{base_url}/v1/sandboxes/{sandbox_id}/exec");
        let mut request =
            tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(url)
                .map_err(|e| WsClientError::Connect(e.to_string()))?;
        request.headers_mut().insert(
            "authorization",
            format!("Bearer {api_key}").parse().map_err(
                |e: tokio_tungstenite::tungstenite::http::header::InvalidHeaderValue| {
                    WsClientError::Connect(e.to_string())
                },
            )?,
        );
        // Comp-7: cap WS frame size so a runaway server payload can't
        // grow the client's heap unboundedly.
        let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig {
            max_frame_size: Some(DEFAULT_MAX_FRAME_BYTES),
            max_message_size: Some(DEFAULT_MAX_FRAME_BYTES),
            ..Default::default()
        };
        let (mut ws, _) =
            tokio_tungstenite::connect_async_with_config(request, Some(config), false)
                .await
                .map_err(|e| WsClientError::Connect(e.to_string()))?;

        // Send IoStart immediately.
        let mut env: HashMap<String, String> = HashMap::new();
        env.extend(params.env);
        let start = IoStart {
            sandbox_id: sandbox_id.to_string(),
            params: Some(io_start::Params::Exec(
                open_sandbox_contracts::proxy::ExecParams {
                    command: params.command,
                    cwd: params.cwd.unwrap_or_default(),
                    env,
                },
            )),
        };
        let mut buf = Vec::with_capacity(1 + start.encoded_len());
        buf.push(KIND_START);
        start
            .encode(&mut buf)
            .map_err(|e| WsClientError::Protocol(e.to_string()))?;
        ws.send(WsMessage::Binary(buf))
            .await
            .map_err(|e| WsClientError::Io(e.to_string()))?;

        Ok(Self {
            ws,
            read_timeout: DEFAULT_READ_TIMEOUT,
        })
    }

    /// Comp-7: enable a per-frame read timeout. After this much time
    /// without a server frame, [`next_frame`] returns
    /// `WsClientError::ReadTimeout`. None disables (legacy behavior).
    pub fn set_read_timeout(&mut self, t: Option<std::time::Duration>) {
        self.read_timeout = t;
    }

    /// Send stdin bytes to the process.
    pub async fn send_stdin(&mut self, bytes: impl Into<Bytes>) -> Result<(), WsClientError> {
        let body: Bytes = bytes.into();
        let mut buf = Vec::with_capacity(1 + body.len());
        buf.push(KIND_STDIN);
        buf.extend_from_slice(&body);
        self.ws
            .send(WsMessage::Binary(buf))
            .await
            .map_err(|e| WsClientError::Io(e.to_string()))
    }

    /// Send a POSIX signal (e.g. 15 = SIGTERM).
    pub async fn send_signal(&mut self, signum: u32) -> Result<(), WsClientError> {
        let sig = IoSignal { signum };
        let mut buf = Vec::with_capacity(1 + sig.encoded_len());
        buf.push(KIND_SIGNAL);
        sig.encode(&mut buf)
            .map_err(|e| WsClientError::Protocol(e.to_string()))?;
        self.ws
            .send(WsMessage::Binary(buf))
            .await
            .map_err(|e| WsClientError::Io(e.to_string()))
    }

    /// Close stdin (half-close) without ending the session.
    pub async fn close_stdin(&mut self) -> Result<(), WsClientError> {
        let close = IoClose { stdin_eof: true };
        let _ = close; // tag-only frame; payload doesn't need encoding
        let buf = vec![KIND_STDIN_EOF];
        self.ws
            .send(WsMessage::Binary(buf))
            .await
            .map_err(|e| WsClientError::Io(e.to_string()))
    }

    /// Read the next server-sent frame. Returns `Ok(None)` on
    /// clean close.
    ///
    /// Comp-7: if [`set_read_timeout`] has been called with a Some
    /// value, returns `Err(WsClientError::ReadTimeout)` when no server
    /// frame arrives within that timeout. The session is left intact
    /// so the caller can retry or close gracefully.
    pub async fn next_frame(&mut self) -> Result<Option<ServerFrame>, WsClientError> {
        let timeout = self.read_timeout;
        loop {
            let next = self.ws.next();
            let msg = match timeout {
                Some(t) => match tokio::time::timeout(t, next).await {
                    Ok(None) => return Ok(None),
                    Ok(Some(Err(e))) => return Err(WsClientError::Io(e.to_string())),
                    Ok(Some(Ok(m))) => m,
                    Err(_) => return Err(WsClientError::ReadTimeout { timeout: t }),
                },
                None => match next.await {
                    None => return Ok(None),
                    Some(Err(e)) => return Err(WsClientError::Io(e.to_string())),
                    Some(Ok(m)) => m,
                },
            };
            match msg {
                WsMessage::Binary(bytes) => return Ok(Some(decode_server(&bytes)?)),
                WsMessage::Close(_) => return Ok(None),
                WsMessage::Ping(_) | WsMessage::Pong(_) | WsMessage::Frame(_) => continue,
                WsMessage::Text(_) => {
                    return Err(WsClientError::Protocol(
                        "received text frame; expected binary".into(),
                    ));
                }
            }
        }
    }
}

fn decode_server(bytes: &[u8]) -> Result<ServerFrame, WsClientError> {
    if bytes.is_empty() {
        return Err(WsClientError::Protocol("empty frame".into()));
    }
    let kind = bytes[0];
    let payload = &bytes[1..];
    match kind {
        KIND_STDOUT => Ok(ServerFrame::Stdout(Bytes::copy_from_slice(payload))),
        KIND_STDERR => Ok(ServerFrame::Stderr(Bytes::copy_from_slice(payload))),
        KIND_EXITED => {
            let e =
                IoExited::decode(payload).map_err(|e| WsClientError::Protocol(e.to_string()))?;
            Ok(ServerFrame::Exited {
                exit_code: e.exit_code,
                command_not_found: e.command_not_found,
            })
        }
        KIND_ERROR => {
            let e = IoErrorProto::decode(payload)
                .map_err(|e| WsClientError::Protocol(e.to_string()))?;
            Ok(ServerFrame::Error {
                code: e.code,
                detail: e.detail,
            })
        }
        KIND_STARTED => {
            let s =
                IoStarted::decode(payload).map_err(|e| WsClientError::Protocol(e.to_string()))?;
            Ok(ServerFrame::Started {
                exec_id: s.exec_id,
                in_container_pid: s.in_container_pid,
            })
        }
        k => Err(WsClientError::Protocol(format!(
            "unknown frame kind 0x{k:02x}"
        ))),
    }
}

/// Streaming file-read session over WebSocket.
///
/// Wire: `GET ws://gateway/v1/sandboxes/{id}/files/read-stream?path=<urlencoded>[&cwd=...]`
/// with `Authorization: Bearer <api-key>`. Server emits raw file
/// bytes as WS Binary frames and closes with code 1000 on EOF or a
/// 44xx-range code on failure.
pub struct ReadFileSession {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    closed_ok: bool,
}

impl ReadFileSession {
    /// Open a streaming file read.
    pub async fn connect(
        base_url: &str,
        sandbox_id: &str,
        api_key: &str,
        path: &str,
        cwd: Option<&str>,
    ) -> Result<Self, WsClientError> {
        let encoded_path = urlencoding::encode(path);
        let mut url =
            format!("{base_url}/v1/sandboxes/{sandbox_id}/files/read-stream?path={encoded_path}");
        if let Some(c) = cwd {
            url.push_str(&format!("&cwd={}", urlencoding::encode(c)));
        }
        let mut request =
            tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(url)
                .map_err(|e| WsClientError::Connect(e.to_string()))?;
        request.headers_mut().insert(
            "authorization",
            format!("Bearer {api_key}").parse().map_err(
                |e: tokio_tungstenite::tungstenite::http::header::InvalidHeaderValue| {
                    WsClientError::Connect(e.to_string())
                },
            )?,
        );
        let (ws, _) = connect_async(request)
            .await
            .map_err(|e| WsClientError::Connect(e.to_string()))?;
        Ok(Self {
            ws,
            closed_ok: false,
        })
    }

    /// Read the next chunk of file bytes, or `Ok(None)` on EOF
    /// (server closed with WS code 1000). On any non-1000 close
    /// code returns `Err(WsClientError::Protocol)` with the
    /// server-supplied reason.
    pub async fn next_chunk(&mut self) -> Result<Option<Bytes>, WsClientError> {
        loop {
            let msg = match self.ws.next().await {
                None if self.closed_ok => return Ok(None),
                None => return Err(WsClientError::Closed),
                Some(Err(e)) => return Err(WsClientError::Io(e.to_string())),
                Some(Ok(m)) => m,
            };
            match msg {
                WsMessage::Binary(bytes) => return Ok(Some(Bytes::from(bytes))),
                WsMessage::Close(frame) => {
                    if let Some(f) = frame {
                        let code = u16::from(f.code);
                        if code == 1000 {
                            self.closed_ok = true;
                            return Ok(None);
                        }
                        return Err(WsClientError::Protocol(format!(
                            "server closed with code {} reason: {}",
                            code, f.reason
                        )));
                    }
                    self.closed_ok = true;
                    return Ok(None);
                }
                WsMessage::Ping(_) | WsMessage::Pong(_) | WsMessage::Frame(_) => continue,
                WsMessage::Text(_) => {
                    return Err(WsClientError::Protocol(
                        "received text frame; expected binary".into(),
                    ));
                }
            }
        }
    }
}
