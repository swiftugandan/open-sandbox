//! Lifecycle handler unit tests. v0.7 exec tests deleted along
//! with the message-shaped exec surface. WebSocket-streaming exec
//! coverage lives in `crates/api/tests/ws_streaming.rs` (12.4's
//! integration suite) and 12.6's e2e scenarios.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use open_sandbox_contracts::error::ApiError;
use open_sandbox_contracts::types::SandboxId;

use crate::proxy_client::ProxyClientPool;
use crate::router::build_router;
use crate::service::{CreateRequest, SandboxInfo, SandboxService};
use crate::state::ApiState;

const TEST_API_KEY: &str = "test-secret-1234";

struct MockService {
    sandbox: SandboxInfo,
}

impl MockService {
    fn new() -> Self {
        let sandbox_id = SandboxId::new();
        let subdomain = sandbox_id.subdomain();
        Self {
            sandbox: SandboxInfo {
                sandbox_id,
                subdomain,
                agent_id: "agent-1".into(),
                status: "running".into(),
            },
        }
    }
}

impl SandboxService for MockService {
    async fn create(&self, _request: CreateRequest) -> Result<SandboxInfo, ApiError> {
        Ok(self.sandbox.clone())
    }

    async fn get(&self, sandbox_id: &SandboxId) -> Result<SandboxInfo, ApiError> {
        if *sandbox_id == self.sandbox.sandbox_id {
            Ok(self.sandbox.clone())
        } else {
            Err(ApiError::SandboxNotFound {
                sandbox_id: sandbox_id.to_string(),
            })
        }
    }

    async fn list(&self) -> Result<Vec<SandboxInfo>, ApiError> {
        Ok(vec![self.sandbox.clone()])
    }

    async fn delete(&self, sandbox_id: &SandboxId) -> Result<(), ApiError> {
        if *sandbox_id == self.sandbox.sandbox_id {
            Ok(())
        } else {
            Err(ApiError::SandboxNotFound {
                sandbox_id: sandbox_id.to_string(),
            })
        }
    }
}

/// Build a stubbed proxy pool that fails on any open_io_stream
/// call — used by lifecycle tests that don't exercise the proxy.
async fn stub_proxy() -> Arc<ProxyClientPool> {
    // Bind a no-op listener so ProxyClientPool::connect succeeds.
    // The pool will never be invoked by lifecycle tests.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = format!("http://{}", listener.local_addr().unwrap());
    // Keep the listener alive in a task so connections don't get
    // refused during the test lifetime.
    tokio::spawn(async move {
        loop {
            let _ = listener.accept().await;
        }
    });
    Arc::new(
        ProxyClientPool::connect(&addr, 1, None)
            .await
            .expect("stub pool connect"),
    )
}

fn json_request(method: &str, uri: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {TEST_API_KEY}"))
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

fn empty_request(method: &str, uri: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {TEST_API_KEY}"))
        .body(Body::empty())
        .unwrap()
}

fn empty_request_no_auth(method: &str, uri: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .unwrap()
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

async fn build_app() -> (Arc<MockService>, axum::Router) {
    let svc = Arc::new(MockService::new());
    let proxy = stub_proxy().await;
    let state = Arc::new(ApiState {
        lifecycle: svc.clone(),
        proxy,
        api_key: TEST_API_KEY.into(),
    });
    let app = build_router(state);
    (svc, app)
}

#[tokio::test]
async fn create_sandbox_returns_201() {
    let (_, app) = build_app().await;
    let req = json_request(
        "POST",
        "/v1/sandboxes",
        serde_json::json!({"image": "nginx:alpine"}),
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn create_sandbox_without_auth_returns_401() {
    let (_, app) = build_app().await;
    let req = Request::builder()
        .method("POST")
        .uri("/v1/sandboxes")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&serde_json::json!({"image": "x"})).unwrap(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body = body_json(resp).await;
    assert_eq!(body["error_code"], "UNAUTHORIZED");
}

#[tokio::test]
async fn list_sandboxes_returns_array() {
    let (svc, app) = build_app().await;
    let req = empty_request("GET", "/v1/sandboxes");
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let arr = body["sandboxes"].as_array().expect("array");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["sandbox_id"], svc.sandbox.sandbox_id.to_string());
}

#[tokio::test]
async fn get_sandbox_returns_200() {
    let (svc, app) = build_app().await;
    let id = svc.sandbox.sandbox_id.to_string();
    let req = empty_request("GET", &format!("/v1/sandboxes/{id}"));
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn get_sandbox_returns_404_for_unknown() {
    let (_, app) = build_app().await;
    let unknown = SandboxId::new().to_string();
    let req = empty_request("GET", &format!("/v1/sandboxes/{unknown}"));
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = body_json(resp).await;
    assert_eq!(body["error_code"], "SANDBOX_NOT_FOUND");
}

#[tokio::test]
async fn delete_sandbox_returns_204() {
    let (svc, app) = build_app().await;
    let id = svc.sandbox.sandbox_id.to_string();
    let req = empty_request("DELETE", &format!("/v1/sandboxes/{id}"));
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn delete_returns_404_for_unknown() {
    let (_, app) = build_app().await;
    let unknown = SandboxId::new().to_string();
    let req = empty_request("DELETE", &format!("/v1/sandboxes/{unknown}"));
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn get_sandbox_returns_400_for_invalid_uuid() {
    let (_, app) = build_app().await;
    let req = empty_request("GET", "/v1/sandboxes/not-a-uuid");
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(body["error_code"], "INVALID_REQUEST");
}

#[tokio::test]
async fn no_auth_header_returns_401() {
    let (svc, app) = build_app().await;
    let id = svc.sandbox.sandbox_id.to_string();
    let req = empty_request_no_auth("GET", &format!("/v1/sandboxes/{id}"));
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn write_files_with_empty_body_returns_invalid_upload() {
    let (svc, app) = build_app().await;
    let id = svc.sandbox.sandbox_id.to_string();
    let req = Request::builder()
        .method("POST")
        .uri(format!("/v1/sandboxes/{id}/files/write_files"))
        .header("authorization", format!("Bearer {TEST_API_KEY}"))
        .header("content-type", "application/gzip")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(body["error_code"], "INVALID_UPLOAD");
}

#[tokio::test]
async fn write_file_rejects_both_content_and_content_b64() {
    let (svc, app) = build_app().await;
    let id = svc.sandbox.sandbox_id.to_string();
    let req = json_request(
        "POST",
        &format!("/v1/sandboxes/{id}/files/write_file"),
        serde_json::json!({"path": "a.txt", "content": "x", "content_b64": "eA=="}),
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(body["error_code"], "INVALID_REQUEST");
}
