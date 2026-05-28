use std::sync::Arc;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::http::{HeaderName, HeaderValue, Method};
use axum::routing::{delete, get, post};
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::handlers;
use crate::service::SandboxService;
use crate::state::ApiState;
use crate::ws_exec;
use crate::ws_read_file;

/// Body-size cap on file-upload routes. Comp-6: previously axum's default
/// 2 MiB cap blocked legitimate uploads (gzip tarballs of project trees
/// commonly exceed it), but raising the global cap also opened
/// memory-pressure DoS on JSON-body lifecycle routes. Per-route override
/// is the correct fix.
const FILE_UPLOAD_BODY_LIMIT_BYTES: usize = 64 * 1024 * 1024; // 64 MiB

/// Optional CORS layer for development consoles served from a different
/// origin (e.g. `python3 -m http.server 8090`). Enabled by setting
/// `OPEN_SANDBOX_API_CORS_ORIGINS` to a comma-separated list of allowed
/// origins, or `*` to allow any origin. Unset → no CORS headers added,
/// which is the correct production default (single-origin deployments
/// don't need preflight).
fn dev_cors_layer() -> Option<CorsLayer> {
    let raw = std::env::var("OPEN_SANDBOX_API_CORS_ORIGINS").ok()?;
    // Strip whitespace and surrounding ASCII quotes from each entry so
    // values copy-pasted from YAML / Helm / shell-quoted env files
    // (e.g. `'"*"'`, `"https://foo"`) match the same as their unquoted
    // forms. Drop blanks.
    let entries: Vec<&str> = raw
        .split(',')
        .map(|s| {
            let s = s.trim();
            s.strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .or_else(|| s.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
                .unwrap_or(s)
        })
        .filter(|s| !s.is_empty())
        .collect();
    if entries.is_empty() {
        return None;
    }
    let layer = CorsLayer::new()
        .allow_methods([Method::GET, Method::POST, Method::DELETE, Method::OPTIONS])
        .allow_headers([
            HeaderName::from_static("authorization"),
            HeaderName::from_static("content-type"),
            HeaderName::from_static("x-cwd"),
        ])
        .expose_headers([HeaderName::from_static("content-type")]);
    // Wildcard is only honored when `*` is the sole entry — silent
    // escalation when an operator writes `'*, https://foo'` (a docs-
    // style mixed value) would broaden the allowlist beyond intent.
    let has_wildcard = entries.iter().any(|e| *e == "*");
    let explicit: Vec<&&str> = entries.iter().filter(|e| **e != "*").collect();
    if has_wildcard && explicit.is_empty() {
        tracing::info!("api: CORS enabled, allowing any origin");
        return Some(layer.allow_origin(AllowOrigin::any()));
    }
    if has_wildcard {
        tracing::warn!(
            explicit = ?explicit,
            "api: OPEN_SANDBOX_API_CORS_ORIGINS contains `*` mixed with explicit origins; \
             ignoring `*` and using the explicit allowlist. Set the value to just `*` for wildcard."
        );
    }
    let parsed: Vec<HeaderValue> = explicit
        .iter()
        .filter_map(|s| match HeaderValue::from_str(s) {
            Ok(v) => Some(v),
            Err(_) => {
                tracing::warn!(origin = %s, "api: CORS origin rejected (not a valid HTTP header value)");
                None
            }
        })
        .collect();
    if parsed.is_empty() {
        // Every entry was invalid; rather than install a layer that
        // silently rejects every cross-origin request, fail loudly.
        tracing::error!(
            "api: OPEN_SANDBOX_API_CORS_ORIGINS contained no valid origins; CORS layer disabled"
        );
        return None;
    }
    tracing::info!(origins = ?parsed, "api: CORS enabled");
    Some(layer.allow_origin(AllowOrigin::list(parsed)))
}

pub fn build_router<S: SandboxService>(state: Arc<ApiState<S>>) -> Router {
    let router = Router::new()
        // Liveness probe. Unauthenticated by design — readiness checks
        // (k8s, dev-up.sh, future `open-sandbox dev`) need to confirm
        // the listener is up before any credential is in play.
        .route("/healthz", get(|| async { "ok" }))
        // Lifecycle (JSON bodies, kept at axum's default cap)
        .route(
            "/v1/sandboxes",
            post(handlers::create_sandbox::<S>).get(handlers::list_sandboxes::<S>),
        )
        .route("/v1/sandboxes/{id}", get(handlers::get_sandbox::<S>))
        .route("/v1/sandboxes/{id}", delete(handlers::delete_sandbox::<S>))
        // v1.0.2: pause / resume. POST so they're semantically write-like
        // and survive the same `:80/v1` proxy caches that DELETE does.
        // Returns 202 Accepted with the optimistic transition state;
        // clients poll GET /v1/sandboxes/{id} for the steady-state.
        .route(
            "/v1/sandboxes/{id}/pause",
            post(handlers::pause_sandbox::<S>),
        )
        .route(
            "/v1/sandboxes/{id}/unpause",
            post(handlers::unpause_sandbox::<S>),
        )
        // File ops (REST, unary, backed by proxy OpenIoStream).
        // Comp-6: per-route body-size cap raised to 64 MiB so realistic
        // uploads succeed; the JSON routes above keep the conservative
        // 2 MiB default.
        .route(
            "/v1/sandboxes/{id}/files/write_files",
            post(handlers::write_files::<S>)
                .layer(DefaultBodyLimit::max(FILE_UPLOAD_BODY_LIMIT_BYTES)),
        )
        .route(
            "/v1/sandboxes/{id}/files/write_file",
            post(handlers::write_file::<S>)
                .layer(DefaultBodyLimit::max(FILE_UPLOAD_BODY_LIMIT_BYTES)),
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
        // v1.0.3: one-level directory listing for the UI file
        // tree. JSON response; caps at LIST_DIR_MAX_ENTRIES with
        // a `truncated` flag. Default JSON body cap is fine —
        // listings are tightly bounded.
        .route(
            "/v1/sandboxes/{id}/files/list",
            get(handlers::list_dir::<S>),
        )
        // v1.0.3: TCP-probe the sandbox's host port until the
        // in-container dev-server is listening or timeout_ms
        // elapses. Gates the UI's preview-iframe refresh on
        // watchexec-restart completion.
        .route(
            "/v1/sandboxes/{id}/wait_port_listening",
            post(handlers::wait_port_listening::<S>),
        )
        // Streaming exec (WebSocket)
        .route("/v1/sandboxes/{id}/exec", get(ws_exec::ws_exec::<S>))
        .with_state(state);
    match dev_cors_layer() {
        Some(cors) => router.layer(cors),
        None => router,
    }
}
