use std::sync::Arc;

use axum::Router;
use axum::routing::{delete, get, post};

use crate::handlers;
use crate::service::SandboxService;

pub fn build_router<S: SandboxService>(service: Arc<S>) -> Router {
    Router::new()
        .route(
            "/v1/sandboxes",
            post(handlers::create_sandbox::<S>).get(handlers::list_sandboxes::<S>),
        )
        .route("/v1/sandboxes/{id}", get(handlers::get_sandbox::<S>))
        .route("/v1/sandboxes/{id}", delete(handlers::delete_sandbox::<S>))
        .route("/v1/sandboxes/{id}/exec", post(handlers::exec_sandbox::<S>))
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
        .with_state(service)
}
