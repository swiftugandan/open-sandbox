use std::sync::Arc;

use axum::routing::{delete, get, post};
use axum::Router;

use crate::handlers;
use crate::service::SandboxService;

pub fn build_router<S: SandboxService>(service: Arc<S>) -> Router {
    Router::new()
        .route("/v1/sandboxes", post(handlers::create_sandbox::<S>))
        .route("/v1/sandboxes/{id}", get(handlers::get_sandbox::<S>))
        .route("/v1/sandboxes/{id}", delete(handlers::delete_sandbox::<S>))
        .route("/v1/sandboxes/{id}/exec", post(handlers::exec_sandbox::<S>))
        .with_state(service)
}
