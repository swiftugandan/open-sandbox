use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;

use open_sandbox_contracts::types::SandboxId;

use crate::service::{CreateRequest, ExecRequest, SandboxService};

pub type AppState<S> = Arc<S>;

pub async fn create_sandbox<S: SandboxService>(
    State(svc): State<AppState<S>>,
    Json(body): Json<CreateRequest>,
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
    Json(body): Json<ExecRequest>,
) -> Response {
    let sandbox_id = match parse_sandbox_id(&id) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    match svc.exec(&sandbox_id, body).await {
        Ok(output) => Json(output).into_response(),
        Err(e) => api_error_response(e),
    }
}

// axum handlers require Response as the error type; boxing adds allocation for no benefit
#[allow(clippy::result_large_err)]
fn parse_sandbox_id(id: &str) -> Result<SandboxId, Response> {
    uuid::Uuid::parse_str(id)
        .map(SandboxId::from)
        .map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "invalid sandbox_id"})),
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

    (status, Json(serde_json::json!({"error": message}))).into_response()
}
