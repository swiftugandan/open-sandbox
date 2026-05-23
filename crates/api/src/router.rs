use std::sync::Arc;

use axum::Router;
use axum::routing::{delete, get, post};

use crate::handlers;
use crate::service::SandboxService;
use crate::state::ApiState;
use crate::ws_exec;

pub fn build_router<S: SandboxService>(state: Arc<ApiState<S>>) -> Router {
    Router::new()
        // Lifecycle
        .route(
            "/v1/sandboxes",
            post(handlers::create_sandbox::<S>).get(handlers::list_sandboxes::<S>),
        )
        .route("/v1/sandboxes/{id}", get(handlers::get_sandbox::<S>))
        .route("/v1/sandboxes/{id}", delete(handlers::delete_sandbox::<S>))
        // File ops (REST, unary, backed by proxy OpenIoStream)
        .route(
            "/v1/sandboxes/{id}/files/write_files",
            post(handlers::write_files::<S>),
        )
        .route(
            "/v1/sandboxes/{id}/files/write_file",
            post(handlers::write_file::<S>),
        )
        .route(
            "/v1/sandboxes/{id}/files/read",
            get(handlers::read_file::<S>),
        )
        // Streaming exec (WebSocket)
        .route("/v1/sandboxes/{id}/exec", get(ws_exec::ws_exec::<S>))
        .with_state(state)
}
