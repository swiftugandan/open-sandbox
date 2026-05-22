use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{FromRequest, Path, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::de::DeserializeOwned;

use open_sandbox_contracts::types::SandboxId;

use crate::service::{
    CreateRequest, ExecRequest, ReadFileRequest, SandboxService, WriteFilesRequest,
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
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "command must not be empty",
                "error_code": "INVALID_REQUEST",
            })),
        )
            .into_response();
    }
    match svc.exec(&sandbox_id, body).await {
        Ok(output) => Json(output).into_response(),
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

pub async fn read_file<S: SandboxService>(
    State(svc): State<AppState<S>>,
    Path(id): Path<String>,
    ValidJson(body): ValidJson<ReadFileRequest>,
) -> Response {
    let sandbox_id = match parse_sandbox_id(&id) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    match svc.read_file(&sandbox_id, body).await {
        Ok(content) => (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
            content,
        )
            .into_response(),
        Err(e) => api_error_response(e),
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

fn api_error_response(err: open_sandbox_contracts::error::ApiError) -> Response {
    use open_sandbox_contracts::error::ApiError;

    let (status, message) = match &err {
        ApiError::Unauthorized { .. } => (StatusCode::UNAUTHORIZED, err.to_string()),
        ApiError::SandboxNotFound { .. } => (StatusCode::NOT_FOUND, err.to_string()),
        ApiError::ControllerUnavailable { .. } => {
            (StatusCode::SERVICE_UNAVAILABLE, err.to_string())
        }
        ApiError::ExecFailed { .. } => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
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
