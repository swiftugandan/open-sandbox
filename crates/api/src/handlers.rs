use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{FromRequest, Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes as ProstBytes;
use serde::de::DeserializeOwned;
use tokio::sync::mpsc;

use open_sandbox_contracts::error::ApiError;
use open_sandbox_contracts::proxy::{
    DeleteFileParams, IoClientFrame, IoStart, ListDirParams, ReadFileParams,
    WaitPortListeningParams, WriteFileParams, WriteFilesTargzParams, io_client_frame,
    io_server_frame, io_start,
};
use open_sandbox_contracts::types::SandboxId;

use crate::service::{
    CreateRequest, DeleteFileQuery, ListDirEntryJson, ListDirQuery, ListDirResultJson,
    ReadFileQuery, SandboxService, WaitPortListeningRequest, WaitPortListeningResultJson,
    WriteFileRequest, WriteFilesResult,
};
use crate::state::ApiState;

const AUTH_HEADER: &str = "authorization";

pub struct ValidJson<T>(pub T);

impl<S, T> FromRequest<S> for ValidJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match axum::Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(ValidJson(value)),
            Err(rejection) => Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": rejection.body_text(),
                    "error_code": "INVALID_REQUEST",
                })),
            )
                .into_response()),
        }
    }
}

/// Boundary auth: every REST request must carry
/// `Authorization: Bearer <api_key>`.
#[allow(clippy::result_large_err)]
pub fn check_rest_auth<S: SandboxService>(
    headers: &HeaderMap,
    state: &Arc<ApiState<S>>,
) -> Result<(), Response> {
    let got = headers
        .get(AUTH_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "));
    match got {
        // Comp-6: constant-time compare so the API-key string isn't a
        // byte-by-byte timing oracle. v1.0 has a single shared key
        // protecting the entire control plane.
        Some(t) if constant_time_eq(t.as_bytes(), state.api_key.as_bytes()) => Ok(()),
        _ => Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "error": "missing or invalid API key",
                "error_code": "UNAUTHORIZED",
            })),
        )
            .into_response()),
    }
}

/// Comp-6: reject obvious path-traversal shapes at the gateway boundary.
/// The agent re-validates, but defense in depth here means a regression in
/// the agent's resolver doesn't immediately escalate to a tenant escape.
///
/// Rejects: NUL bytes, `..` path components, control characters. Allows
/// absolute paths (the in-sandbox file system uses them) but the segment
/// check prevents climbing out of an intended cwd.
pub(crate) fn validate_sandbox_path(path: &str) -> Result<(), &'static str> {
    if path.contains('\0') {
        return Err("path must not contain NUL bytes");
    }
    if path.bytes().any(|b| b < 0x20 && b != b'\t') {
        return Err("path must not contain control characters");
    }
    for segment in path.split('/') {
        if segment == ".." {
            return Err("path must not contain '..' segments");
        }
    }
    Ok(())
}

// Subprotocol auth constants live in the contracts crate — they're part
// of the v1.x public wire surface that SDK authors need to discover.
pub use open_sandbox_contracts::constants::{
    WS_AUTH_BEARER_PREFIX, WS_AUTH_MAX_OFFERED_PROTOCOLS, WS_AUTH_PROTOCOL_SENTINEL,
};

/// WebSocket boundary auth. Browser `WebSocket` constructors cannot
/// attach an `Authorization` header, so this helper also accepts the
/// standards-friendly fallback of
/// `Sec-WebSocket-Protocol: open-sandbox.v1, bearer.<base64url(key)>`.
///
/// Returns `Ok(None)` if auth came via the `Authorization` header
/// (no subprotocol echo needed) and `Ok(Some(WS_AUTH_PROTOCOL_SENTINEL))`
/// if the caller authenticated via subprotocol — the caller MUST echo
/// that protocol back in the upgrade response or the browser rejects the
/// handshake. The key itself is never returned (and therefore never
/// echoed), so it stays out of response headers / access logs / DevTools.
///
/// Both auth paths are tried; presenting a wrong Authorization header
/// alongside a valid subprotocol still authenticates (proxies that
/// inject stale Authorization headers don't lock out the page).
#[allow(clippy::result_large_err)]
pub fn check_ws_auth(headers: &HeaderMap, expected: &str) -> Result<Option<String>, Response> {
    // Path 1: Authorization: Bearer <key> (programmatic clients).
    let auth_matches = headers
        .get(AUTH_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .is_some_and(|tok| constant_time_eq(tok.as_bytes(), expected.as_bytes()));
    if auth_matches {
        return Ok(None);
    }

    // Path 2: Sec-WebSocket-Protocol (browser clients). Iterate every
    // header value (RFC 7230 permits a sender to split the list across
    // multiple headers) AND every comma-separated entry within a value.
    // The per-request iteration is capped (see WS_AUTH_MAX_OFFERED_PROTOCOLS
    // in the contracts crate) so an unauthenticated attacker can't amplify
    // a single upgrade into thousands of constant_time_eq calls.
    use base64::Engine;
    let expected_b64 =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(expected.as_bytes());
    let mut sentinel_offered = false;
    let mut bearer_ok = false;
    let offered_iter = headers
        .get_all("sec-websocket-protocol")
        .into_iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|raw| raw.split(',').map(|s| s.trim()))
        .take(WS_AUTH_MAX_OFFERED_PROTOCOLS);
    for offered in offered_iter {
        if offered.eq_ignore_ascii_case(WS_AUTH_PROTOCOL_SENTINEL) {
            sentinel_offered = true;
        } else if let Some(b64) = strip_prefix_ascii_ci(offered, WS_AUTH_BEARER_PREFIX)
            && constant_time_eq(b64.as_bytes(), expected_b64.as_bytes())
        {
            bearer_ok = true;
        }
    }
    if bearer_ok {
        // Only echo if the client also offered the sentinel — RFC 6455
        // requires the server's chosen protocol to be one of the offered
        // values. If the client only offered the bearer entry, accept
        // the connection without echoing (browsers tolerate this).
        return Ok(sentinel_offered.then(|| WS_AUTH_PROTOCOL_SENTINEL.to_string()));
    }

    Err((
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({
            "error": "missing or invalid API key",
            "error_code": "UNAUTHORIZED",
        })),
    )
        .into_response())
}

/// Case-insensitive ASCII prefix strip. Mirrors HTTP scheme tradition
/// (`Authorization: Bearer` is case-insensitive per RFC 7235) so a
/// client that offers `Bearer.<…>` is treated the same as `bearer.<…>`.
fn strip_prefix_ascii_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.len() >= prefix.len() && s.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes()) {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

/// Constant-time byte compare. Lifted from crates/controller/src/auth.rs
/// to keep crates/api self-contained without a workspace shared module.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

pub async fn create_sandbox<S: SandboxService>(
    State(state): State<Arc<ApiState<S>>>,
    headers: HeaderMap,
    ValidJson(body): ValidJson<CreateRequest>,
) -> Response {
    if let Err(r) = check_rest_auth(&headers, &state) {
        return r;
    }
    match state.lifecycle.create(body).await {
        Ok(info) => (StatusCode::CREATED, Json(info)).into_response(),
        Err(e) => api_error_response(e),
    }
}

pub async fn list_sandboxes<S: SandboxService>(
    State(state): State<Arc<ApiState<S>>>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = check_rest_auth(&headers, &state) {
        return r;
    }
    match state.lifecycle.list().await {
        Ok(items) => Json(serde_json::json!({ "sandboxes": items })).into_response(),
        Err(e) => api_error_response(e),
    }
}

pub async fn get_sandbox<S: SandboxService>(
    State(state): State<Arc<ApiState<S>>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = check_rest_auth(&headers, &state) {
        return r;
    }
    let sandbox_id = match parse_sandbox_id(&id) {
        Ok(id) => id,
        Err(r) => return r,
    };
    match state.lifecycle.get(&sandbox_id).await {
        Ok(info) => Json(info).into_response(),
        Err(e) => api_error_response(e),
    }
}

pub async fn delete_sandbox<S: SandboxService>(
    State(state): State<Arc<ApiState<S>>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = check_rest_auth(&headers, &state) {
        return r;
    }
    let sandbox_id = match parse_sandbox_id(&id) {
        Ok(id) => id,
        Err(r) => return r,
    };
    match state.lifecycle.delete(&sandbox_id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => api_error_response(e),
    }
}

pub async fn pause_sandbox<S: SandboxService>(
    State(state): State<Arc<ApiState<S>>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = check_rest_auth(&headers, &state) {
        return r;
    }
    let sandbox_id = match parse_sandbox_id(&id) {
        Ok(id) => id,
        Err(r) => return r,
    };
    match state.lifecycle.pause(&sandbox_id).await {
        Ok(result) => (StatusCode::ACCEPTED, Json(result)).into_response(),
        Err(e) => api_error_response(e),
    }
}

pub async fn unpause_sandbox<S: SandboxService>(
    State(state): State<Arc<ApiState<S>>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = check_rest_auth(&headers, &state) {
        return r;
    }
    let sandbox_id = match parse_sandbox_id(&id) {
        Ok(id) => id,
        Err(r) => return r,
    };
    match state.lifecycle.unpause(&sandbox_id).await {
        Ok(result) => (StatusCode::ACCEPTED, Json(result)).into_response(),
        Err(e) => api_error_response(e),
    }
}

// ===== File ops (REST, unary, backed by proxy OpenIoStream) =====

pub async fn write_files<S: SandboxService>(
    State(state): State<Arc<ApiState<S>>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(r) = check_rest_auth(&headers, &state) {
        return r;
    }
    let sandbox_id = match parse_sandbox_id(&id) {
        Ok(id) => id,
        Err(r) => return r,
    };

    // Validate at the boundary: empty body or missing gzip magic →
    // INVALID_UPLOAD (preserves the v0.7 NFR-API-1 contract).
    if body.is_empty() || body.len() < 2 || body[0] != 0x1f || body[1] != 0x8b {
        return api_error_response(ApiError::InvalidUpload {
            detail: "request body is not a gzip stream (expected magic 1f 8b)".into(),
        });
    }

    let cwd = headers
        .get("x-cwd")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let result = unary_via_io_stream(
        &state,
        &sandbox_id,
        IoStart {
            sandbox_id: sandbox_id.to_string(),
            params: Some(io_start::Params::WriteFilesTargz(WriteFilesTargzParams {
                cwd,
            })),
        },
        Some(body.to_vec()),
    )
    .await;

    match result {
        Ok(_) => Json(WriteFilesResult {
            success: true,
            revision: None,
        })
        .into_response(),
        Err(e) => api_error_response(e),
    }
}

pub async fn write_file<S: SandboxService>(
    State(state): State<Arc<ApiState<S>>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    ValidJson(body): ValidJson<WriteFileRequest>,
) -> Response {
    if let Err(r) = check_rest_auth(&headers, &state) {
        return r;
    }
    let sandbox_id = match parse_sandbox_id(&id) {
        Ok(id) => id,
        Err(r) => return r,
    };
    if body.path.is_empty() {
        return invalid_request("path must not be empty");
    }
    // Comp-6: defense-in-depth boundary check. Reject obvious traversal
    // shapes before forwarding to the agent so a regression in the agent's
    // path resolution can't escalate to a tenant escape.
    if let Err(msg) = validate_sandbox_path(&body.path) {
        return invalid_request(msg);
    }
    let content = match body.content_bytes() {
        Ok(b) => b,
        Err(e) => return api_error_response(e),
    };

    let result = unary_via_io_stream(
        &state,
        &sandbox_id,
        IoStart {
            sandbox_id: sandbox_id.to_string(),
            params: Some(io_start::Params::WriteFile(WriteFileParams {
                path: body.path,
                cwd: body.cwd.unwrap_or_default(),
                // v1.0.3: forward the precondition fields through.
                // Default empty `expected_revision` keeps v1.0.2
                // wire-compat for callers that don't supply it.
                expected_revision: body.expected_revision.unwrap_or_default(),
                force: body.force,
            })),
        },
        Some(content),
    )
    .await;

    match result {
        Ok(meta) => Json(WriteFilesResult {
            success: true,
            revision: meta.map(|m| m.revision),
        })
        .into_response(),
        Err(e) => api_error_response(e),
    }
}

pub async fn read_file<S: SandboxService>(
    State(state): State<Arc<ApiState<S>>>,
    Path(id): Path<String>,
    Query(query): Query<ReadFileQuery>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = check_rest_auth(&headers, &state) {
        return r;
    }
    let sandbox_id = match parse_sandbox_id(&id) {
        Ok(id) => id,
        Err(r) => return r,
    };
    if query.path.is_empty() {
        return invalid_request("path query parameter is required");
    }
    // Comp-6: defense-in-depth boundary check (same as write_file).
    if let Err(msg) = validate_sandbox_path(&query.path) {
        return invalid_request(msg);
    }

    let result = stream_via_io_stream(
        &state,
        &sandbox_id,
        IoStart {
            sandbox_id: sandbox_id.to_string(),
            params: Some(io_start::Params::ReadFile(ReadFileParams {
                path: query.path,
                cwd: query.cwd.unwrap_or_default(),
            })),
        },
    )
    .await;

    match result {
        Ok(ReadFileResult { bytes, meta }) => {
            // v1.0.3: surface the revision token as the
            // X-File-Revision response header so the UI can capture
            // it without parsing the body. Missing when the runtime
            // backend hasn't wired stat_revision yet.
            let mut response_headers = vec![(
                axum::http::header::CONTENT_TYPE,
                "application/octet-stream".to_string(),
            )];
            if let Some(m) = meta {
                response_headers.push((
                    axum::http::HeaderName::from_static("x-file-revision"),
                    m.revision,
                ));
            }
            let mut response = (StatusCode::OK, bytes).into_response();
            for (name, value) in response_headers {
                if let Ok(v) = axum::http::HeaderValue::from_str(&value) {
                    response.headers_mut().insert(name, v);
                }
            }
            response
        }
        Err(e) => api_error_response(e),
    }
}

pub async fn list_dir<S: SandboxService>(
    State(state): State<Arc<ApiState<S>>>,
    Path(id): Path<String>,
    Query(query): Query<ListDirQuery>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = check_rest_auth(&headers, &state) {
        return r;
    }
    let sandbox_id = match parse_sandbox_id(&id) {
        Ok(id) => id,
        Err(r) => return r,
    };
    if query.path.is_empty() {
        return invalid_request("path query parameter is required");
    }
    if let Err(msg) = validate_sandbox_path(&query.path) {
        return invalid_request(msg);
    }

    let result = list_dir_via_io_stream(
        &state,
        &sandbox_id,
        IoStart {
            sandbox_id: sandbox_id.to_string(),
            params: Some(io_start::Params::ListDir(ListDirParams {
                path: query.path,
                cwd: query.cwd.unwrap_or_default(),
            })),
        },
    )
    .await;

    match result {
        Ok(listing) => (StatusCode::OK, Json(listing)).into_response(),
        Err(e) => api_error_response(e),
    }
}

pub async fn delete_file<S: SandboxService>(
    State(state): State<Arc<ApiState<S>>>,
    Path(id): Path<String>,
    Query(query): Query<DeleteFileQuery>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = check_rest_auth(&headers, &state) {
        return r;
    }
    let sandbox_id = match parse_sandbox_id(&id) {
        Ok(id) => id,
        Err(r) => return r,
    };
    if query.path.is_empty() {
        return invalid_request("path query parameter is required");
    }
    if let Err(msg) = validate_sandbox_path(&query.path) {
        return invalid_request(msg);
    }

    let result = unary_via_io_stream(
        &state,
        &sandbox_id,
        IoStart {
            sandbox_id: sandbox_id.to_string(),
            params: Some(io_start::Params::DeleteFile(DeleteFileParams {
                path: query.path,
                cwd: query.cwd.unwrap_or_default(),
                recursive: query.recursive,
            })),
        },
        None,
    )
    .await;

    match result {
        Ok(_) => (StatusCode::NO_CONTENT, ()).into_response(),
        Err(e) => api_error_response(e),
    }
}

pub async fn wait_port_listening<S: SandboxService>(
    State(state): State<Arc<ApiState<S>>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    ValidJson(body): ValidJson<WaitPortListeningRequest>,
) -> Response {
    if let Err(r) = check_rest_auth(&headers, &state) {
        return r;
    }
    let sandbox_id = match parse_sandbox_id(&id) {
        Ok(id) => id,
        Err(r) => return r,
    };
    // Belt-and-suspenders gateway-side clamp: the agent ALSO clamps,
    // but enforcing it here too prevents tying up an OpenIoStream
    // session slot for longer than the platform cap. (Defense in
    // depth — closes the D2 design concern from FOLLOWUPS_v1.0.3.)
    let clamped_timeout = body
        .timeout_ms
        .min(open_sandbox_contracts::constants::WAIT_PORT_LISTENING_MAX_TIMEOUT_MS);

    let result = wait_port_via_io_stream(
        &state,
        &sandbox_id,
        IoStart {
            sandbox_id: sandbox_id.to_string(),
            params: Some(io_start::Params::WaitPortListening(WaitPortListeningParams {
                port: body.port,
                timeout_ms: clamped_timeout,
            })),
        },
    )
    .await;

    match result {
        Ok(r) => (StatusCode::OK, Json(r)).into_response(),
        Err(e) => api_error_response(e),
    }
}

// ===== Helpers =====

/// v1.0.3: optional FileMeta sidecar observed during the session.
/// Returned alongside the unary success so REST handlers can surface
/// `X-File-Revision` headers / response-body revision fields without
/// an extra round-trip. `None` when the agent didn't emit FileMeta
/// (legacy v1.0.2 wire, or a runtime stub).
#[derive(Debug, Default, Clone)]
pub struct UnaryFileMeta {
    pub revision: String,
    pub size: u64,
}

/// Open an OpenIoStream, push the first IoStart, optionally push
/// the content as a single Stdin frame, await IoExited (success)
/// or IoError. Returns Ok with the captured FileMeta sidecar (if
/// the agent emitted one before terminating).
async fn unary_via_io_stream<S: SandboxService>(
    state: &Arc<ApiState<S>>,
    _sandbox_id: &SandboxId,
    start: IoStart,
    content: Option<Vec<u8>>,
) -> Result<Option<UnaryFileMeta>, ApiError> {
    // Smoke-test fix: previously this function pushed ALL frames into
    // client_tx (capacity 8) BEFORE calling open_io_stream, deadlocking
    // whenever the content needed >7 Stdin chunks (~448 KiB). The
    // consumer side (the proxy) only starts draining after the
    // open_io_stream call returns. Solution: send the IoStart inline,
    // open the stream, then drive the rest of the frames from a
    // spawned producer task that runs concurrently with the server-
    // frame loop.
    let (client_tx, client_rx) = mpsc::channel::<IoClientFrame>(8);
    client_tx
        .send(IoClientFrame {
            stream_id: String::new(),
            payload: Some(io_client_frame::Payload::Start(start)),
        })
        .await
        .map_err(|_| ApiError::IoStreamFailed {
            detail: "internal channel closed".into(),
        })?;

    // Open the stream NOW so the consumer is ready before we push the
    // body chunks. With the proxy draining concurrently, the spawned
    // producer below can send more than 8 frames without deadlocking.
    let mut server_rx = state.proxy.open_io_stream(client_rx).await?;

    // Comp-6: chunk the upload at STDIN_CHUNK_BYTES so any single tonic
    // message stays well below the 4 MiB default decode cap. 64 KiB
    // matches the read-side chunking the agent emits.
    tokio::spawn(async move {
        if let Some(bytes) = content
            && !bytes.is_empty()
        {
            const STDIN_CHUNK_BYTES: usize = 64 * 1024;
            let mut offset = 0;
            while offset < bytes.len() {
                let end = (offset + STDIN_CHUNK_BYTES).min(bytes.len());
                let chunk = bytes[offset..end].to_vec();
                if client_tx
                    .send(IoClientFrame {
                        stream_id: String::new(),
                        payload: Some(io_client_frame::Payload::Stdin(chunk)),
                    })
                    .await
                    .is_err()
                {
                    return; // proxy dropped its receiver; stop pushing
                }
                offset = end;
            }
        }
        // Signal EOF. Dropping client_tx after this closes the stream.
        let _ = client_tx
            .send(IoClientFrame {
                stream_id: String::new(),
                payload: Some(io_client_frame::Payload::Close(
                    open_sandbox_contracts::proxy::IoClose { stdin_eof: true },
                )),
            })
            .await;
    });
    let mut captured_meta: Option<UnaryFileMeta> = None;
    while let Some(frame_res) = server_rx.recv().await {
        let frame = frame_res?;
        match frame.payload {
            Some(io_server_frame::Payload::Exited(e)) => {
                if e.exit_code == 0 && !e.command_not_found {
                    return Ok(captured_meta);
                }
                return Err(ApiError::IoStreamFailed {
                    detail: format!(
                        "exit={} command_not_found={}",
                        e.exit_code, e.command_not_found
                    ),
                });
            }
            Some(io_server_frame::Payload::Error(err)) => {
                return Err(map_io_error(&err));
            }
            // v1.0.3: capture FileMeta sidecar (emitted by
            // drive_write_file after a successful write) so the
            // REST response can carry the new revision back.
            Some(io_server_frame::Payload::FileMeta(m)) => {
                captured_meta = Some(UnaryFileMeta {
                    revision: m.revision,
                    size: m.size,
                });
            }
            // ignore stdout/stderr/started for unary file writes
            _ => {}
        }
    }
    Err(ApiError::IoStreamFailed {
        detail: "proxy stream ended without terminal frame".into(),
    })
}

/// v1.0.3: ReadFile session result — body bytes plus the optional
/// FileMeta sidecar observed before the first Stdout chunk. The
/// `read_file` REST handler surfaces `meta.revision` as the
/// `X-File-Revision` response header.
pub struct ReadFileResult {
    pub bytes: ProstBytes,
    pub meta: Option<UnaryFileMeta>,
}

/// Open an OpenIoStream for ReadFile and collect all stdout chunks
/// into a single Bytes buffer (unary REST read endpoint).
async fn stream_via_io_stream<S: SandboxService>(
    state: &Arc<ApiState<S>>,
    _sandbox_id: &SandboxId,
    start: IoStart,
) -> Result<ReadFileResult, ApiError> {
    let (client_tx, client_rx) = mpsc::channel::<IoClientFrame>(8);
    client_tx
        .send(IoClientFrame {
            stream_id: String::new(),
            payload: Some(io_client_frame::Payload::Start(start)),
        })
        .await
        .ok();
    drop(client_tx);

    let mut server_rx = state.proxy.open_io_stream(client_rx).await?;
    let mut buf: Vec<u8> = Vec::new();
    let mut captured_meta: Option<UnaryFileMeta> = None;
    while let Some(frame_res) = server_rx.recv().await {
        let frame = frame_res?;
        match frame.payload {
            Some(io_server_frame::Payload::Stdout(chunk)) => buf.extend_from_slice(&chunk),
            Some(io_server_frame::Payload::Stderr(_)) => {}
            Some(io_server_frame::Payload::Exited(e)) => {
                if e.exit_code != 0 {
                    return Err(ApiError::IoStreamFailed {
                        detail: format!("read_file exited with {}", e.exit_code),
                    });
                }
                return Ok(ReadFileResult {
                    bytes: ProstBytes::from(buf),
                    meta: captured_meta,
                });
            }
            Some(io_server_frame::Payload::Error(err)) => {
                return Err(map_io_error(&err));
            }
            // v1.0.3: capture the FileMeta sidecar emitted before
            // the first Stdout chunk. read_file's REST handler
            // surfaces revision via the X-File-Revision header.
            Some(io_server_frame::Payload::FileMeta(m)) => {
                captured_meta = Some(UnaryFileMeta {
                    revision: m.revision,
                    size: m.size,
                });
            }
            _ => {}
        }
    }
    Err(ApiError::IoStreamFailed {
        detail: "proxy stream ended without terminal frame".into(),
    })
}

/// v1.0.3: open an OpenIoStream for ListDir and collect the single
/// ListDirResult sidecar frame; map the proto entry shape into the
/// JSON wire shape used by the REST handler.
async fn list_dir_via_io_stream<S: SandboxService>(
    state: &Arc<ApiState<S>>,
    _sandbox_id: &SandboxId,
    start: IoStart,
) -> Result<ListDirResultJson, ApiError> {
    use open_sandbox_contracts::proxy::ListDirEntryType;

    let (client_tx, client_rx) = mpsc::channel::<IoClientFrame>(2);
    client_tx
        .send(IoClientFrame {
            stream_id: String::new(),
            payload: Some(io_client_frame::Payload::Start(start)),
        })
        .await
        .ok();
    drop(client_tx);

    let mut server_rx = state.proxy.open_io_stream(client_rx).await?;
    let mut captured: Option<ListDirResultJson> = None;
    while let Some(frame_res) = server_rx.recv().await {
        let frame = frame_res?;
        match frame.payload {
            Some(io_server_frame::Payload::ListDirResult(r)) => {
                let entries = r
                    .entries
                    .into_iter()
                    .map(|e| ListDirEntryJson {
                        name: e.name,
                        kind: match ListDirEntryType::try_from(e.r#type)
                            .unwrap_or(ListDirEntryType::Unspecified)
                        {
                            ListDirEntryType::File => "file",
                            ListDirEntryType::Dir => "dir",
                            ListDirEntryType::Symlink => "symlink",
                            ListDirEntryType::Other => "other",
                            ListDirEntryType::Unspecified => "other",
                        },
                        size: e.size,
                        revision: e.revision,
                        mode: e.mode,
                        target: e.target,
                    })
                    .collect();
                captured = Some(ListDirResultJson {
                    path: r.path,
                    entries,
                    truncated: r.truncated,
                    total_entries: r.total_entries,
                });
            }
            Some(io_server_frame::Payload::Exited(e)) => {
                if e.exit_code != 0 {
                    return Err(ApiError::IoStreamFailed {
                        detail: format!("list_dir exited with {}", e.exit_code),
                    });
                }
                return captured.ok_or(ApiError::IoStreamFailed {
                    detail: "list_dir session ended without ListDirResult".into(),
                });
            }
            Some(io_server_frame::Payload::Error(err)) => {
                return Err(map_io_error(&err));
            }
            _ => {}
        }
    }
    Err(ApiError::IoStreamFailed {
        detail: "proxy stream ended without terminal frame".into(),
    })
}

/// v1.0.3: open an OpenIoStream for WaitPortListening and collect
/// the single WaitPortListeningResult sidecar frame.
async fn wait_port_via_io_stream<S: SandboxService>(
    state: &Arc<ApiState<S>>,
    _sandbox_id: &SandboxId,
    start: IoStart,
) -> Result<WaitPortListeningResultJson, ApiError> {
    let (client_tx, client_rx) = mpsc::channel::<IoClientFrame>(2);
    client_tx
        .send(IoClientFrame {
            stream_id: String::new(),
            payload: Some(io_client_frame::Payload::Start(start)),
        })
        .await
        .ok();
    drop(client_tx);

    let mut server_rx = state.proxy.open_io_stream(client_rx).await?;
    let mut captured: Option<WaitPortListeningResultJson> = None;
    while let Some(frame_res) = server_rx.recv().await {
        let frame = frame_res?;
        match frame.payload {
            Some(io_server_frame::Payload::WaitPortListeningResult(r)) => {
                captured = Some(WaitPortListeningResultJson {
                    ready: r.ready,
                    elapsed_ms: r.elapsed_ms,
                });
            }
            Some(io_server_frame::Payload::Exited(e)) => {
                if e.exit_code != 0 {
                    return Err(ApiError::IoStreamFailed {
                        detail: format!("wait_port_listening exited with {}", e.exit_code),
                    });
                }
                return captured.ok_or(ApiError::IoStreamFailed {
                    detail: "wait_port_listening session ended without result".into(),
                });
            }
            Some(io_server_frame::Payload::Error(err)) => {
                return Err(map_io_error(&err));
            }
            _ => {}
        }
    }
    Err(ApiError::IoStreamFailed {
        detail: "proxy stream ended without terminal frame".into(),
    })
}

/// v1.0.3: re-exported under `map_io_error_pub` for crate-siblings
/// (ws_read_file) so the WS streaming endpoint and the unary REST
/// endpoint share a single IoError → ApiError translation table.
pub(crate) fn map_io_error_pub(err: &open_sandbox_contracts::proxy::IoError) -> ApiError {
    map_io_error(err)
}

fn map_io_error(err: &open_sandbox_contracts::proxy::IoError) -> ApiError {
    // v1.0.2 cascade: parse via IoErrorCode so the SANDBOX_NOT_FOUND →
    // SandboxGone alias normalization lives in contracts instead of
    // duplicated as a string match here. New codes Other(_) fall through
    // to the generic IoStreamFailed bucket.
    use open_sandbox_contracts::wire::IoErrorCode;
    match IoErrorCode::from(err.code.as_str()) {
        IoErrorCode::FileNotFound => ApiError::FileNotFound {
            resolved_path: err.detail.clone(),
        },
        IoErrorCode::SandboxGone => ApiError::SandboxGone {
            sandbox_id: err.detail.clone(),
        },
        // v1.0.3: revision mismatch carries the live token in the
        // detail field (or empty when the file didn't exist).
        IoErrorCode::RevisionMismatch => ApiError::RevisionMismatch {
            actual_revision: err.detail.clone(),
        },
        // v1.0.3: runtime backend hasn't wired a v1.0.3 capability.
        // Maps to 501; the agent's detail explains which capability.
        IoErrorCode::NotImplemented => ApiError::NotImplemented {
            detail: err.detail.clone(),
        },
        // v1.0.3: non-recursive delete of a populated directory.
        // Maps to 409 Conflict; UI re-prompts with "delete
        // recursively?".
        IoErrorCode::DirectoryNotEmpty => ApiError::DirectoryNotEmpty {
            detail: err.detail.clone(),
        },
        other => ApiError::IoStreamFailed {
            detail: format!("{other}: {}", err.detail),
        },
    }
}

// axum handlers require Response as the error type; boxing adds allocation for no benefit
#[allow(clippy::result_large_err)]
fn parse_sandbox_id(id: &str) -> Result<SandboxId, Response> {
    uuid::Uuid::parse_str(id).map(SandboxId::from).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "invalid sandbox_id",
                "error_code": "INVALID_REQUEST",
            })),
        )
            .into_response()
    })
}

fn invalid_request(msg: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({
            "error": msg,
            "error_code": "INVALID_REQUEST",
        })),
    )
        .into_response()
}

fn api_error_response(err: ApiError) -> Response {
    let (status, message) = match &err {
        ApiError::Unauthorized { .. } => (StatusCode::UNAUTHORIZED, err.to_string()),
        ApiError::SandboxNotFound { .. } | ApiError::SandboxGone { .. } => {
            (StatusCode::NOT_FOUND, err.to_string())
        }
        ApiError::FileNotFound { .. } => (StatusCode::NOT_FOUND, err.to_string()),
        ApiError::InvalidRequest { .. } | ApiError::InvalidUpload { .. } => {
            (StatusCode::BAD_REQUEST, err.to_string())
        }
        ApiError::ControllerUnavailable { .. } | ApiError::ProxyUnavailable { .. } => {
            (StatusCode::SERVICE_UNAVAILABLE, err.to_string())
        }
        ApiError::IoStreamFailed { .. } => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
        ApiError::InvalidState { .. } => (StatusCode::CONFLICT, err.to_string()),
        // v1.0.3: optimistic-concurrency precondition failed. 409 is
        // the canonical HTTP code for "the resource exists but its
        // state didn't match the caller's expectation" — see also
        // InvalidState above which uses the same status for the
        // lifecycle-transition precondition.
        ApiError::RevisionMismatch { .. } => (StatusCode::CONFLICT, err.to_string()),
        // v1.0.3: runtime capability gap — gateway is fine, agent
        // can't fulfill yet. 501 (not 500) so SDKs can feature-
        // detect and fall back without retrying.
        ApiError::NotImplemented { .. } => (StatusCode::NOT_IMPLEMENTED, err.to_string()),
        // v1.0.3: non-recursive delete on a populated dir. 409
        // matches RevisionMismatch's "precondition not met"
        // shape so SDKs can handle them under one branch.
        ApiError::DirectoryNotEmpty { .. } => (StatusCode::CONFLICT, err.to_string()),
        ApiError::Internal { .. } => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
    };
    let code = err.error_code();
    // v1.0.3: revision-mismatch carries an extra structured field
    // (the live revision token) so the client can refetch + retry
    // without an extra stat round-trip. Other variants stay on the
    // legacy {error, error_code} shape.
    let body = if let ApiError::RevisionMismatch { actual_revision } = &err {
        serde_json::json!({
            "error": message,
            "error_code": code,
            "actual_revision": actual_revision,
        })
    } else {
        serde_json::json!({"error": message, "error_code": code})
    };
    (status, Json(body)).into_response()
}
