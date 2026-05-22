use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{FromRequest, Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::de::DeserializeOwned;

use open_sandbox_contracts::types::SandboxId;

use crate::service::{
    CreateRequest, ExecRequest, ExecResponseBody, ReadFileRequest, SandboxService,
    WriteFileRequest, WriteFilesRequest,
};

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

pub type AppState<S> = Arc<S>;

pub async fn create_sandbox<S: SandboxService>(
    State(svc): State<AppState<S>>,
    ValidJson(body): ValidJson<CreateRequest>,
) -> Response {
    match svc.create(body).await {
        Ok(info) => (StatusCode::CREATED, Json(info)).into_response(),
        Err(e) => api_error_response(e),
    }
}

pub async fn list_sandboxes<S: SandboxService>(State(svc): State<AppState<S>>) -> Response {
    match svc.list().await {
        Ok(items) => Json(serde_json::json!({ "sandboxes": items })).into_response(),
        Err(e) => api_error_response(e),
    }
}

pub async fn get_sandbox<S: SandboxService>(
    State(svc): State<AppState<S>>,
    Path(id): Path<String>,
) -> Response {
    let sandbox_id = match parse_sandbox_id(&id) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    match svc.get(&sandbox_id).await {
        Ok(info) => Json(info).into_response(),
        Err(e) => api_error_response(e),
    }
}

pub async fn delete_sandbox<S: SandboxService>(
    State(svc): State<AppState<S>>,
    Path(id): Path<String>,
) -> Response {
    let sandbox_id = match parse_sandbox_id(&id) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    match svc.delete(&sandbox_id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => api_error_response(e),
    }
}

pub async fn exec_sandbox<S: SandboxService>(
    State(svc): State<AppState<S>>,
    Path(id): Path<String>,
    ValidJson(body): ValidJson<ExecRequest>,
) -> Response {
    let sandbox_id = match parse_sandbox_id(&id) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    if body.command.is_empty() {
        return invalid_request("command must not be empty");
    }
    // Validate stdin encoding at the boundary so a client that supplies both
    // `stdin` and `stdin_b64` (or malformed b64) gets a 400 before forwarding.
    if let Err(e) = body.stdin_bytes() {
        return api_error_response(e);
    }
    match svc.exec(&sandbox_id, body).await {
        Ok(output) => Json(ExecResponseBody::from(output)).into_response(),
        Err(e) => api_error_response(e),
    }
}

pub async fn write_files<S: SandboxService>(
    State(svc): State<AppState<S>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let sandbox_id = match parse_sandbox_id(&id) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    // NFR-API-1 INVALID_UPLOAD: an empty or non-gzip body must return 400
    // here, not propagate as EXEC_FAILED from a downstream `tar` failure.
    if body.is_empty() || body.len() < 2 || body[0] != 0x1f || body[1] != 0x8b {
        return api_error_response(open_sandbox_contracts::error::ApiError::InvalidUpload {
            detail: "request body is not a gzip stream (expected magic 1f 8b)".into(),
        });
    }
    let cwd = headers
        .get("x-cwd")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let request = WriteFilesRequest {
        content: body.to_vec(),
        cwd,
    };
    match svc.write_files(&sandbox_id, request).await {
        Ok(result) => (StatusCode::OK, Json(result)).into_response(),
        Err(e) => api_error_response(e),
    }
}

pub async fn write_file<S: SandboxService>(
    State(svc): State<AppState<S>>,
    Path(id): Path<String>,
    ValidJson(body): ValidJson<WriteFileRequest>,
) -> Response {
    let sandbox_id = match parse_sandbox_id(&id) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    if body.path.is_empty() {
        return invalid_request("path must not be empty");
    }
    // Validate content encoding at the boundary.
    if let Err(e) = body.content_bytes() {
        return api_error_response(e);
    }
    match svc.write_file(&sandbox_id, body).await {
        Ok(result) => (StatusCode::OK, Json(result)).into_response(),
        Err(e) => api_error_response(e),
    }
}

pub async fn read_file<S: SandboxService>(
    State(svc): State<AppState<S>>,
    Path(id): Path<String>,
    Query(query): Query<ReadFileQuery>,
) -> Response {
    let sandbox_id = match parse_sandbox_id(&id) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    if query.path.is_empty() {
        return invalid_request("path query parameter is required");
    }
    let request = ReadFileRequest {
        path: query.path,
        cwd: query.cwd,
    };
    match svc.read_file(&sandbox_id, request).await {
        Ok(content) => (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
            content,
        )
            .into_response(),
        Err(e) => api_error_response(e),
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct ReadFileQuery {
    pub path: String,
    #[serde(default)]
    pub cwd: Option<String>,
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

fn api_error_response(err: open_sandbox_contracts::error::ApiError) -> Response {
    use open_sandbox_contracts::error::ApiError;

    let (status, message) = match &err {
        ApiError::Unauthorized { .. } => (StatusCode::UNAUTHORIZED, err.to_string()),
        ApiError::SandboxNotFound { .. } => (StatusCode::NOT_FOUND, err.to_string()),
        ApiError::FileNotFound { .. } => (StatusCode::NOT_FOUND, err.to_string()),
        ApiError::InvalidRequest { .. } => (StatusCode::BAD_REQUEST, err.to_string()),
        ApiError::InvalidUpload { .. } => (StatusCode::BAD_REQUEST, err.to_string()),
        ApiError::ControllerUnavailable { .. } => {
            (StatusCode::SERVICE_UNAVAILABLE, err.to_string())
        }
        ApiError::ExecFailed { .. } => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
        // CommandNotFound is surfaced inside the exec response envelope as
        // error_code: COMMAND_NOT_FOUND with HTTP 200 — see NFR-API-1.
        // It should never reach this generic error renderer.
        ApiError::CommandNotFound { .. } => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
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
