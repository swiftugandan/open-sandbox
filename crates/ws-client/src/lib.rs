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
//!         open_sandbox_ws_client::ServerFrame::Exited(exit_code) => {
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

pub struct ExecSession {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
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
        let (mut ws, _) = connect_async(request)
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

        Ok(Self { ws })
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
    pub async fn next_frame(&mut self) -> Result<Option<ServerFrame>, WsClientError> {
        loop {
            let msg = match self.ws.next().await {
                None => return Ok(None),
                Some(Err(e)) => return Err(WsClientError::Io(e.to_string())),
                Some(Ok(m)) => m,
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
