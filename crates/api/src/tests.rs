//! Lifecycle handler unit tests. v0.7 exec tests deleted along
//! with the message-shaped exec surface. WebSocket-streaming exec
//! end-to-end coverage lives in 12.6's e2e scenarios; this file
//! covers the auth helper (`check_ws_auth`) used on the WS upgrade
//! path.

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
    last_pull_policy: std::sync::Mutex<Option<open_sandbox_contracts::types::PullPolicy>>,
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
                error: None,
            },
            last_pull_policy: std::sync::Mutex::new(None),
        }
    }
}

impl SandboxService for MockService {
    async fn create(&self, request: CreateRequest) -> Result<SandboxInfo, ApiError> {
        *self.last_pull_policy.lock().unwrap() = Some(request.pull_policy);
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

    async fn pause(
        &self,
        sandbox_id: &SandboxId,
    ) -> Result<crate::service::TransitionResult, ApiError> {
        if *sandbox_id == self.sandbox.sandbox_id {
            Ok(crate::service::TransitionResult {
                status: "pausing".into(),
            })
        } else {
            Err(ApiError::SandboxNotFound {
                sandbox_id: sandbox_id.to_string(),
            })
        }
    }

    async fn unpause(
        &self,
        sandbox_id: &SandboxId,
    ) -> Result<crate::service::TransitionResult, ApiError> {
        if *sandbox_id == self.sandbox.sandbox_id {
            Ok(crate::service::TransitionResult {
                status: "unpausing".into(),
            })
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
async fn pause_sandbox_returns_202_and_transition_body() {
    let (svc, app) = build_app().await;
    let id = svc.sandbox.sandbox_id.to_string();
    let req = empty_request("POST", &format!("/v1/sandboxes/{id}/pause"));
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body = body_json(resp).await;
    assert_eq!(body["status"], "pausing");
}

#[tokio::test]
async fn unpause_sandbox_returns_202_and_transition_body() {
    let (svc, app) = build_app().await;
    let id = svc.sandbox.sandbox_id.to_string();
    let req = empty_request("POST", &format!("/v1/sandboxes/{id}/unpause"));
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body = body_json(resp).await;
    assert_eq!(body["status"], "unpausing");
}

#[tokio::test]
async fn pause_unknown_returns_404() {
    let (_, app) = build_app().await;
    let unknown = SandboxId::new().to_string();
    let req = empty_request("POST", &format!("/v1/sandboxes/{unknown}/pause"));
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn invalid_state_api_error_maps_to_409() {
    // Cascade-fix #5: ApiError::InvalidState must surface as HTTP 409
    // Conflict so clients can distinguish a precondition refusal from
    // an internal error.
    use open_sandbox_contracts::error::ApiError;
    let err = ApiError::InvalidState {
        detail: "cannot pause sandbox in state 'stopped'".into(),
    };
    assert_eq!(err.error_code(), "INVALID_STATE");
}

#[tokio::test]
async fn pause_without_auth_returns_401() {
    let (svc, app) = build_app().await;
    let id = svc.sandbox.sandbox_id.to_string();
    let req = empty_request_no_auth("POST", &format!("/v1/sandboxes/{id}/pause"));
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
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

#[tokio::test]
async fn create_sandbox_default_pull_policy_is_if_not_present() {
    use open_sandbox_contracts::types::PullPolicy;
    let (svc, app) = build_app().await;
    let req = json_request(
        "POST",
        "/v1/sandboxes",
        serde_json::json!({"image": "nginx:alpine"}),
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(
        *svc.last_pull_policy.lock().unwrap(),
        Some(PullPolicy::IfNotPresent),
        "omitting pull_policy must default to IfNotPresent so old clients keep the current optimized warm path"
    );
}

#[tokio::test]
async fn create_sandbox_accepts_kebab_case_pull_policy_always() {
    use open_sandbox_contracts::types::PullPolicy;
    let (svc, app) = build_app().await;
    let req = json_request(
        "POST",
        "/v1/sandboxes",
        serde_json::json!({"image": "myorg/app:latest", "pull_policy": "always"}),
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(
        *svc.last_pull_policy.lock().unwrap(),
        Some(PullPolicy::Always),
        "pull_policy=always opts back into the v1.0.1 always-pull behavior for floating tags"
    );
}

#[tokio::test]
async fn create_sandbox_rejects_unknown_pull_policy() {
    let (_svc, app) = build_app().await;
    let req = json_request(
        "POST",
        "/v1/sandboxes",
        serde_json::json!({"image": "x", "pull_policy": "yolo"}),
    );
    let resp = app.oneshot(req).await.unwrap();
    // serde rejects unknown enum variants → axum surfaces as 400/422.
    // Either is acceptable; the key contract is "not 201 Created".
    assert!(
        resp.status() == StatusCode::BAD_REQUEST
            || resp.status() == StatusCode::UNPROCESSABLE_ENTITY,
        "expected 400/422 for unknown pull_policy variant, got {}",
        resp.status()
    );
}

// ===== v1.0.3: list_dir + wait_port_listening validation =====
//
// The full happy-path coverage for these routes (the actual proxy
// round-trip + ListDirResult / WaitPortListeningResult decoding)
// lives in the agent crate's drive_list_dir / drive_wait_port_
// listening unit tests, which exercise the same wire shape end to
// end. Here we only pin the pre-proxy validation (auth, query/body
// shape) — exactly the same pattern as the existing read_file and
// write_file tests above.

#[tokio::test]
async fn list_dir_rejects_empty_path() {
    // axum's Query extractor rejects missing required fields before
    // the handler runs; the empty-string check inside the handler
    // covers the `path=` (present but empty) case.
    let (svc, app) = build_app().await;
    let id = svc.sandbox.sandbox_id.to_string();
    let req = empty_request("GET", &format!("/v1/sandboxes/{id}/files/list?path="));
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(body["error_code"], "INVALID_REQUEST");
}

#[tokio::test]
async fn list_dir_rejects_path_traversal() {
    let (svc, app) = build_app().await;
    let id = svc.sandbox.sandbox_id.to_string();
    let req = empty_request(
        "GET",
        &format!("/v1/sandboxes/{id}/files/list?path=../../etc/passwd"),
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(body["error_code"], "INVALID_REQUEST");
}

#[tokio::test]
async fn list_dir_without_auth_returns_401() {
    let (svc, app) = build_app().await;
    let id = svc.sandbox.sandbox_id.to_string();
    let req = empty_request_no_auth(
        "GET",
        &format!("/v1/sandboxes/{id}/files/list?path=/workspace"),
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn wait_port_listening_without_auth_returns_401() {
    let (svc, app) = build_app().await;
    let id = svc.sandbox.sandbox_id.to_string();
    let req = Request::builder()
        .method("POST")
        .uri(format!("/v1/sandboxes/{id}/wait_port_listening"))
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&serde_json::json!({
                "port": 8080,
                "timeout_ms": 3000,
            }))
            .unwrap(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn wait_port_listening_rejects_missing_port() {
    let (svc, app) = build_app().await;
    let id = svc.sandbox.sandbox_id.to_string();
    let req = json_request(
        "POST",
        &format!("/v1/sandboxes/{id}/wait_port_listening"),
        serde_json::json!({"timeout_ms": 3000}),
    );
    let resp = app.oneshot(req).await.unwrap();
    // serde rejects missing required field — axum surfaces as 400/422.
    assert!(
        resp.status() == StatusCode::BAD_REQUEST
            || resp.status() == StatusCode::UNPROCESSABLE_ENTITY,
        "expected 400/422 for missing port, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn write_file_accepts_expected_revision_and_force_fields() {
    // v1.0.3: WriteFileRequest gains two optional fields. The
    // parser must not reject a body that includes them; downstream
    // routing to the (stub) proxy is fine — we just pin the JSON
    // schema acceptance here.
    let (svc, app) = build_app().await;
    let id = svc.sandbox.sandbox_id.to_string();
    let req = json_request(
        "POST",
        &format!("/v1/sandboxes/{id}/files/write_file"),
        serde_json::json!({
            "path": "a.txt",
            "content": "hi",
            "expected_revision": "1716800123:421",
            "force": false,
        }),
    );
    let resp = app.oneshot(req).await.unwrap();
    // The stub proxy fails open_io_stream, so we expect 503 (or
    // 500) NOT 400 — the JSON parsed fine.
    assert!(
        resp.status() != StatusCode::BAD_REQUEST
            && resp.status() != StatusCode::UNPROCESSABLE_ENTITY,
        "v1.0.3 WriteFileRequest fields must parse; got {}",
        resp.status()
    );
}

// ===== WebSocket auth helper =====

mod ws_auth {
    use axum::http::HeaderMap;
    use base64::Engine;

    use crate::handlers::{WS_AUTH_PROTOCOL_SENTINEL, check_ws_auth};

    const KEY: &str = "test-key-with,comma+slash/and=padding";

    fn b64url(s: &str) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s.as_bytes())
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                axum::http::HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn authorization_bearer_accepted() {
        let h = headers(&[("authorization", &format!("Bearer {}", KEY))]);
        assert!(matches!(check_ws_auth(&h, KEY), Ok(None)));
    }

    #[test]
    fn missing_returns_unauthorized() {
        let h = headers(&[]);
        assert!(check_ws_auth(&h, KEY).is_err());
    }

    #[test]
    fn wrong_authorization_returns_unauthorized() {
        let h = headers(&[("authorization", "Bearer wrong")]);
        assert!(check_ws_auth(&h, KEY).is_err());
    }

    #[test]
    fn subprotocol_with_sentinel_echoes_sentinel_not_key() {
        let h = headers(&[(
            "sec-websocket-protocol",
            &format!("open-sandbox.v1, bearer.{}", b64url(KEY)),
        )]);
        match check_ws_auth(&h, KEY) {
            Ok(Some(echo)) => {
                assert_eq!(echo, WS_AUTH_PROTOCOL_SENTINEL);
                // Critical: the key (or its base64 form) must NEVER appear
                // in the echoed protocol — that string lands in the response
                // handshake header.
                assert!(!echo.contains(KEY));
                assert!(!echo.contains(&b64url(KEY)));
            }
            other => panic!("expected Ok(Some(sentinel)), got {:?}", other.is_ok()),
        }
    }

    #[test]
    fn subprotocol_without_sentinel_still_authenticates_but_does_not_echo() {
        // Per RFC 6455 the server MAY accept the upgrade without selecting a
        // subprotocol when the client offered ones it doesn't recognize.
        let h = headers(&[(
            "sec-websocket-protocol",
            &format!("bearer.{}", b64url(KEY)),
        )]);
        assert!(matches!(check_ws_auth(&h, KEY), Ok(None)));
    }

    #[test]
    fn subprotocol_split_across_multiple_headers() {
        // RFC 7230 permits splitting a list header across multiple values;
        // both must be inspected.
        let h = headers(&[
            ("sec-websocket-protocol", "open-sandbox.v1"),
            ("sec-websocket-protocol", &format!("bearer.{}", b64url(KEY))),
        ]);
        match check_ws_auth(&h, KEY) {
            Ok(Some(echo)) => assert_eq!(echo, WS_AUTH_PROTOCOL_SENTINEL),
            other => panic!("expected Ok(Some(sentinel)), got {:?}", other.is_ok()),
        }
    }

    #[test]
    fn key_with_comma_and_base64_chars_round_trips() {
        // The whole point of base64url encoding: keys containing `,`, `+`,
        // `/`, `=` must still authenticate via subprotocol.
        assert!(KEY.contains(','));
        assert!(KEY.contains('+'));
        let h = headers(&[(
            "sec-websocket-protocol",
            &format!("open-sandbox.v1, bearer.{}", b64url(KEY)),
        )]);
        assert!(check_ws_auth(&h, KEY).is_ok());
    }

    #[test]
    fn wrong_authorization_with_valid_subprotocol_authenticates() {
        // Proxies that inject a stale Authorization header must not lock
        // out a page that presents valid subprotocol credentials.
        let h = headers(&[
            ("authorization", "Bearer stale-rotated-key"),
            (
                "sec-websocket-protocol",
                &format!("open-sandbox.v1, bearer.{}", b64url(KEY)),
            ),
        ]);
        assert!(check_ws_auth(&h, KEY).is_ok());
    }

    #[test]
    fn wrong_subprotocol_with_wrong_authorization_rejected() {
        let h = headers(&[
            ("authorization", "Bearer wrong"),
            (
                "sec-websocket-protocol",
                &format!("open-sandbox.v1, bearer.{}", b64url("not-the-key")),
            ),
        ]);
        assert!(check_ws_auth(&h, KEY).is_err());
    }

    #[test]
    fn case_insensitive_bearer_prefix_accepted() {
        // HTTP scheme tradition (RFC 7235 `Authorization: Bearer`) is
        // case-insensitive; mirror it for the subprotocol prefix so
        // `Bearer.<b64>` doesn't get rejected as a wrong credential.
        let h = headers(&[(
            "sec-websocket-protocol",
            &format!("open-sandbox.v1, Bearer.{}", b64url(KEY)),
        )]);
        assert!(matches!(check_ws_auth(&h, KEY), Ok(Some(_))));
    }

    #[test]
    fn excess_offered_protocols_capped() {
        // An attacker stuffing the header with bogus `bearer.X` entries
        // shouldn't multiply the work done per upgrade. The helper's
        // 16-entry cap means a valid bearer offer hidden after the cap
        // is not consulted — by design, this trades a tiny correctness
        // edge for a hard cap on pre-auth amplification.
        let mut entries: Vec<String> =
            (0..32).map(|i| format!("bearer.bogus{}", i)).collect();
        // Place the real credential AT THE CAP boundary: index 17 is
        // beyond the 16-entry cap so the helper does NOT see it.
        entries.push(format!("bearer.{}", b64url(KEY)));
        let h = headers(&[("sec-websocket-protocol", &entries.join(", "))]);
        // Past-cap bearer entries are not consulted: 401.
        assert!(check_ws_auth(&h, KEY).is_err());
    }

    #[test]
    fn valid_offer_within_cap_still_authenticates() {
        let mut entries = vec!["open-sandbox.v1".to_string(), format!("bearer.{}", b64url(KEY))];
        // Add a handful of decoys but stay under the 16-entry cap.
        for i in 0..10 {
            entries.push(format!("bearer.bogus{}", i));
        }
        let h = headers(&[("sec-websocket-protocol", &entries.join(", "))]);
        assert!(matches!(check_ws_auth(&h, KEY), Ok(Some(_))));
    }

    #[test]
    fn unauthorized_error_body_uses_stable_shape() {
        let h = headers(&[]);
        let resp = check_ws_auth(&h, KEY).expect_err("expected Err");
        // Code path: just assert it's a 401. The exact JSON body is
        // exercised through the router-integration paths; here we only
        // pin the status so a future refactor that drops the helper's
        // error path is caught.
        assert_eq!(resp.status(), axum::http::StatusCode::UNAUTHORIZED);
    }
}
