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
    IoClientFrame, IoStart, ReadFileParams, WriteFileParams, WriteFilesTargzParams,
    io_client_frame, io_server_frame, io_start,
};
use open_sandbox_contracts::types::SandboxId;

use crate::service::{
    CreateRequest, ReadFileQuery, SandboxService, WriteFileRequest, WriteFilesResult,
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
        Some(t) if t == state.api_key => Ok(()),
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
        Ok(_) => Json(WriteFilesResult { success: true }).into_response(),
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
            })),
        },
        Some(content),
    )
    .await;

    match result {
        Ok(_) => Json(WriteFilesResult { success: true }).into_response(),
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
        Ok(bytes) => (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
            bytes,
        )
            .into_response(),
        Err(e) => api_error_response(e),
    }
}

// ===== Helpers =====

/// Open an OpenIoStream, push the first IoStart, optionally push
/// the content as a single Stdin frame, await IoExited (success)
/// or IoError. Returns Ok with empty bytes on success.
async fn unary_via_io_stream<S: SandboxService>(
    state: &Arc<ApiState<S>>,
    _sandbox_id: &SandboxId,
    start: IoStart,
    content: Option<Vec<u8>>,
) -> Result<(), ApiError> {
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

    if let Some(bytes) = content
        && !bytes.is_empty()
    {
        client_tx
            .send(IoClientFrame {
                stream_id: String::new(),
                payload: Some(io_client_frame::Payload::Stdin(bytes)),
            })
            .await
            .map_err(|_| ApiError::IoStreamFailed {
                detail: "internal channel closed".into(),
            })?;
    }
    // Signal EOF.
    client_tx
        .send(IoClientFrame {
            stream_id: String::new(),
            payload: Some(io_client_frame::Payload::Close(
                open_sandbox_contracts::proxy::IoClose { stdin_eof: true },
            )),
        })
        .await
        .ok();
    drop(client_tx);

    let mut server_rx = state.proxy.open_io_stream(client_rx).await?;
    while let Some(frame_res) = server_rx.recv().await {
        let frame = frame_res?;
        match frame.payload {
            Some(io_server_frame::Payload::Exited(e)) => {
                if e.exit_code == 0 && !e.command_not_found {
                    return Ok(());
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
            // ignore stdout/stderr/started for unary file writes
            _ => {}
        }
    }
    Err(ApiError::IoStreamFailed {
        detail: "proxy stream ended without terminal frame".into(),
    })
}

/// Open an OpenIoStream for ReadFile and collect all stdout chunks
/// into a single Bytes buffer (unary REST read endpoint).
async fn stream_via_io_stream<S: SandboxService>(
    state: &Arc<ApiState<S>>,
    _sandbox_id: &SandboxId,
    start: IoStart,
) -> Result<ProstBytes, ApiError> {
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
                return Ok(ProstBytes::from(buf));
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

fn map_io_error(err: &open_sandbox_contracts::proxy::IoError) -> ApiError {
    match err.code.as_str() {
        "FILE_NOT_FOUND" => ApiError::FileNotFound {
            resolved_path: err.detail.clone(),
        },
        "SANDBOX_GONE" => ApiError::SandboxGone {
            sandbox_id: err.detail.clone(),
        },
        _ => ApiError::IoStreamFailed {
            detail: format!("{}: {}", err.code, err.detail),
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
        ApiError::Internal { .. } => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
    };
    let code = err.error_code();
    (
        status,
        Json(serde_json::json!({"error": message, "error_code": code})),
    )
        .into_response()
}
