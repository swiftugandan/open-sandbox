use std::sync::Arc;

use axum::Router;
use axum::routing::{delete, get, post};

use crate::handlers;
use crate::service::SandboxService;
use crate::state::ApiState;
use crate::ws_exec;
use crate::ws_read_file;

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
        // Streaming WebSocket variant of /files/read. Same
        // semantics as the unary GET, but the response body is
        // delivered as raw-bytes WS Binary frames terminated by
        // a Close frame (1000 EOF / 44xx error). Two-route
        // split (rather than a single path that branches on the
        // Upgrade header) sidesteps a transitive axum 0.7 vs 0.8
        // type-trait collision pulled in by tonic.
        .route(
            "/v1/sandboxes/{id}/files/read-stream",
            get(ws_read_file::ws_read_file::<S>),
        )
        // Streaming exec (WebSocket)
        .route("/v1/sandboxes/{id}/exec", get(ws_exec::ws_exec::<S>))
        .with_state(state)
}
