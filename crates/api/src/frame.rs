//! WebSocket binary frame envelope: `| 1 byte kind | payload |`.
//!
//! One WebSocket binary message = one application frame. The
//! payload length is implied by the WS frame length — no inner
//! length prefix needed. Frame kinds map 1:1 to
//! IoClient/IoServer frame variants.
//!
//! All non-byte frames (start, signal, exited, error, started) are
//! proto-encoded into the payload — same encoding family as the
//! gRPC client uses. No JSON.

use bytes::Bytes;
use prost::Message;

use open_sandbox_contracts::proxy::{
    FileMeta, IoClientFrame, IoError, IoExited, IoServerFrame, IoSignal, IoStart, IoStarted,
    ListDirResult, WaitPortListeningResult, io_client_frame, io_server_frame,
};

// Frame envelope kinds live in the contracts crate so the gateway
// encoder and the ws-client decoder cannot drift. Re-exported here
// with the historical local names so the rest of this module reads
// the same as before the refactor.
pub use open_sandbox_contracts::constants::{
    FRAME_KIND_ERROR as KIND_ERROR, FRAME_KIND_EXITED as KIND_EXITED,
    FRAME_KIND_FILE_META as KIND_FILE_META,
    FRAME_KIND_LIST_DIR_RESULT as KIND_LIST_DIR_RESULT, FRAME_KIND_SIGNAL as KIND_SIGNAL,
    FRAME_KIND_START as KIND_START, FRAME_KIND_STARTED as KIND_STARTED,
    FRAME_KIND_STDERR as KIND_STDERR, FRAME_KIND_STDIN as KIND_STDIN,
    FRAME_KIND_STDIN_EOF as KIND_STDIN_EOF, FRAME_KIND_STDOUT as KIND_STDOUT,
    FRAME_KIND_WAIT_PORT_LISTENING_RESULT as KIND_WAIT_PORT_LISTENING_RESULT,
};

#[derive(Debug)]
pub enum FrameError {
    Empty,
    UnknownKind(u8),
    DecodeFailed { kind: u8, detail: String },
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::Empty => write!(f, "empty WebSocket frame"),
            FrameError::UnknownKind(k) => write!(f, "unknown frame kind 0x{:02x}", k),
            FrameError::DecodeFailed { kind, detail } => {
                write!(f, "decode failed for kind 0x{:02x}: {}", kind, detail)
            }
        }
    }
}

impl std::error::Error for FrameError {}

// ===== client → server =====

/// Decode a WebSocket binary frame into an `IoClientFrame` payload.
/// The caller fills in `stream_id` (which is allocated by the proxy
/// when the OpenIoStream call starts; not the gateway's concern).
pub fn decode_client(bytes: &[u8]) -> Result<io_client_frame::Payload, FrameError> {
    if bytes.is_empty() {
        return Err(FrameError::Empty);
    }
    let kind = bytes[0];
    let payload = &bytes[1..];
    match kind {
        KIND_START => {
            let start = IoStart::decode(payload).map_err(|e| FrameError::DecodeFailed {
                kind,
                detail: e.to_string(),
            })?;
            Ok(io_client_frame::Payload::Start(start))
        }
        KIND_STDIN => Ok(io_client_frame::Payload::Stdin(payload.to_vec())),
        KIND_SIGNAL => {
            let sig = IoSignal::decode(payload).map_err(|e| FrameError::DecodeFailed {
                kind,
                detail: e.to_string(),
            })?;
            Ok(io_client_frame::Payload::Signal(sig))
        }
        KIND_STDIN_EOF => Ok(io_client_frame::Payload::Close(
            open_sandbox_contracts::proxy::IoClose { stdin_eof: true },
        )),
        _ => Err(FrameError::UnknownKind(kind)),
    }
}

/// Build a complete `IoClientFrame` ready to feed into the
/// ProxyClient. `stream_id` is left empty; the proxy assigns it
/// when it bridges into the agent's tunnel.
pub fn build_client_frame(payload: io_client_frame::Payload) -> IoClientFrame {
    IoClientFrame {
        stream_id: String::new(),
        payload: Some(payload),
    }
}

// ===== server → client =====

/// Encode an `IoServerFrame` for the wire. Returns the bytes ready
/// for a WebSocket binary message.
pub fn encode_server(frame: &IoServerFrame) -> Result<Bytes, FrameError> {
    let Some(payload) = &frame.payload else {
        return Err(FrameError::Empty);
    };
    let (kind, body): (u8, Bytes) = match payload {
        io_server_frame::Payload::Stdout(b) => (KIND_STDOUT, Bytes::from(b.clone())),
        io_server_frame::Payload::Stderr(b) => (KIND_STDERR, Bytes::from(b.clone())),
        io_server_frame::Payload::Exited(e) => {
            let mut buf = Vec::with_capacity(e.encoded_len());
            e.encode(&mut buf).map_err(|err| FrameError::DecodeFailed {
                kind: KIND_EXITED,
                detail: err.to_string(),
            })?;
            (KIND_EXITED, Bytes::from(buf))
        }
        io_server_frame::Payload::Error(err) => {
            let mut buf = Vec::with_capacity(err.encoded_len());
            err.encode(&mut buf).map_err(|e| FrameError::DecodeFailed {
                kind: KIND_ERROR,
                detail: e.to_string(),
            })?;
            (KIND_ERROR, Bytes::from(buf))
        }
        io_server_frame::Payload::Started(s) => {
            let mut buf = Vec::with_capacity(s.encoded_len());
            s.encode(&mut buf).map_err(|e| FrameError::DecodeFailed {
                kind: KIND_STARTED,
                detail: e.to_string(),
            })?;
            (KIND_STARTED, Bytes::from(buf))
        }
        io_server_frame::Payload::ListDirResult(r) => {
            let mut buf = Vec::with_capacity(r.encoded_len());
            r.encode(&mut buf).map_err(|e| FrameError::DecodeFailed {
                kind: KIND_LIST_DIR_RESULT,
                detail: e.to_string(),
            })?;
            (KIND_LIST_DIR_RESULT, Bytes::from(buf))
        }
        io_server_frame::Payload::WaitPortListeningResult(r) => {
            let mut buf = Vec::with_capacity(r.encoded_len());
            r.encode(&mut buf).map_err(|e| FrameError::DecodeFailed {
                kind: KIND_WAIT_PORT_LISTENING_RESULT,
                detail: e.to_string(),
            })?;
            (KIND_WAIT_PORT_LISTENING_RESULT, Bytes::from(buf))
        }
        io_server_frame::Payload::FileMeta(m) => {
            let mut buf = Vec::with_capacity(m.encoded_len());
            m.encode(&mut buf).map_err(|e| FrameError::DecodeFailed {
                kind: KIND_FILE_META,
                detail: e.to_string(),
            })?;
            (KIND_FILE_META, Bytes::from(buf))
        }
    };
    let mut out = Vec::with_capacity(1 + body.len());
    out.push(kind);
    out.extend_from_slice(&body);
    Ok(Bytes::from(out))
}

/// Decode a server frame (used by the ws-client for symmetry).
pub fn decode_server(bytes: &[u8]) -> Result<io_server_frame::Payload, FrameError> {
    if bytes.is_empty() {
        return Err(FrameError::Empty);
    }
    let kind = bytes[0];
    let payload = &bytes[1..];
    match kind {
        KIND_STDOUT => Ok(io_server_frame::Payload::Stdout(payload.to_vec())),
        KIND_STDERR => Ok(io_server_frame::Payload::Stderr(payload.to_vec())),
        KIND_EXITED => {
            let e = IoExited::decode(payload).map_err(|e| FrameError::DecodeFailed {
                kind,
                detail: e.to_string(),
            })?;
            Ok(io_server_frame::Payload::Exited(e))
        }
        KIND_ERROR => {
            let e = IoError::decode(payload).map_err(|e| FrameError::DecodeFailed {
                kind,
                detail: e.to_string(),
            })?;
            Ok(io_server_frame::Payload::Error(e))
        }
        KIND_STARTED => {
            let s = IoStarted::decode(payload).map_err(|e| FrameError::DecodeFailed {
                kind,
                detail: e.to_string(),
            })?;
            Ok(io_server_frame::Payload::Started(s))
        }
        KIND_LIST_DIR_RESULT => {
            let r = ListDirResult::decode(payload).map_err(|e| FrameError::DecodeFailed {
                kind,
                detail: e.to_string(),
            })?;
            Ok(io_server_frame::Payload::ListDirResult(r))
        }
        KIND_WAIT_PORT_LISTENING_RESULT => {
            let r =
                WaitPortListeningResult::decode(payload).map_err(|e| FrameError::DecodeFailed {
                    kind,
                    detail: e.to_string(),
                })?;
            Ok(io_server_frame::Payload::WaitPortListeningResult(r))
        }
        KIND_FILE_META => {
            let m = FileMeta::decode(payload).map_err(|e| FrameError::DecodeFailed {
                kind,
                detail: e.to_string(),
            })?;
            Ok(io_server_frame::Payload::FileMeta(m))
        }
        _ => Err(FrameError::UnknownKind(kind)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use open_sandbox_contracts::proxy::{ExecParams, io_start};
    use std::collections::HashMap;

    #[test]
    fn decode_empty_frame_errors() {
        assert!(matches!(decode_client(&[]), Err(FrameError::Empty)));
    }

    #[test]
    fn unknown_kind_errors() {
        assert!(matches!(
            decode_client(&[0xFF]),
            Err(FrameError::UnknownKind(0xFF))
        ));
    }

    #[test]
    fn stdin_round_trip() {
        let body = b"hello stdin";
        let mut wire = vec![KIND_STDIN];
        wire.extend_from_slice(body);
        let p = decode_client(&wire).unwrap();
        match p {
            io_client_frame::Payload::Stdin(b) => assert_eq!(b, body),
            other => panic!("expected Stdin, got {other:?}"),
        }
    }

    #[test]
    fn start_round_trip() {
        let original = IoStart {
            sandbox_id: "00000000-0000-0000-0000-000000000001".into(),
            params: Some(io_start::Params::Exec(ExecParams {
                command: vec!["echo".into(), "hi".into()],
                cwd: "/home".into(),
                env: HashMap::new(),
            })),
        };
        let mut wire = vec![KIND_START];
        let mut buf = Vec::with_capacity(original.encoded_len());
        original.encode(&mut buf).unwrap();
        wire.extend_from_slice(&buf);

        let decoded = decode_client(&wire).unwrap();
        match decoded {
            io_client_frame::Payload::Start(s) => {
                assert_eq!(s.sandbox_id, original.sandbox_id);
                match s.params {
                    Some(io_start::Params::Exec(e)) => {
                        assert_eq!(e.command, vec!["echo".to_string(), "hi".into()]);
                    }
                    other => panic!("expected Exec params, got {other:?}"),
                }
            }
            other => panic!("expected Start, got {other:?}"),
        }
    }

    #[test]
    fn exited_round_trip() {
        let frame = IoServerFrame {
            stream_id: "io-7".into(),
            payload: Some(io_server_frame::Payload::Exited(IoExited {
                exit_code: 42,
                command_not_found: true,
            })),
        };
        let wire = encode_server(&frame).unwrap();
        assert_eq!(wire[0], KIND_EXITED);
        let decoded = decode_server(&wire).unwrap();
        match decoded {
            io_server_frame::Payload::Exited(e) => {
                assert_eq!(e.exit_code, 42);
                assert!(e.command_not_found);
            }
            other => panic!("expected Exited, got {other:?}"),
        }
    }
}
