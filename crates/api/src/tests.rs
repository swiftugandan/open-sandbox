use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use open_sandbox_contracts::error::ApiError;
use open_sandbox_contracts::types::SandboxId;

use crate::router::build_router;
use crate::service::{
    CreateRequest, ExecOutput, ExecRequest, ReadFileRequest, SandboxInfo, SandboxService,
    WriteFileRequest, WriteFilesRequest, WriteFilesResult,
};

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

    async fn exec(
        &self,
        sandbox_id: &SandboxId,
        _request: ExecRequest,
    ) -> Result<ExecOutput, ApiError> {
        if *sandbox_id == self.sandbox.sandbox_id {
            Ok(ExecOutput {
                exit_code: 0,
                stdout: b"hello\n".to_vec(),
                stderr: vec![],
                command_not_found: false,
            })
        } else {
            Err(ApiError::SandboxNotFound {
                sandbox_id: sandbox_id.to_string(),
            })
        }
    }

    async fn write_files(
        &self,
        sandbox_id: &SandboxId,
        _request: WriteFilesRequest,
    ) -> Result<WriteFilesResult, ApiError> {
        if *sandbox_id == self.sandbox.sandbox_id {
            Ok(WriteFilesResult { success: true })
        } else {
            Err(ApiError::SandboxNotFound {
                sandbox_id: sandbox_id.to_string(),
            })
        }
    }

    async fn write_file(
        &self,
        sandbox_id: &SandboxId,
        _request: WriteFileRequest,
    ) -> Result<WriteFilesResult, ApiError> {
        if *sandbox_id == self.sandbox.sandbox_id {
            Ok(WriteFilesResult { success: true })
        } else {
            Err(ApiError::SandboxNotFound {
                sandbox_id: sandbox_id.to_string(),
            })
        }
    }

    async fn read_file(
        &self,
        sandbox_id: &SandboxId,
        request: ReadFileRequest,
    ) -> Result<Vec<u8>, ApiError> {
        if *sandbox_id == self.sandbox.sandbox_id {
            Ok(format!("contents of {}", request.path).into_bytes())
        } else {
            Err(ApiError::SandboxNotFound {
                sandbox_id: sandbox_id.to_string(),
            })
        }
    }
}

fn json_request(method: &str, uri: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

fn empty_request(method: &str, uri: &str) -> Request<Body> {
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

#[tokio::test]
async fn create_sandbox_returns_201_with_sandbox_info() {
    let svc = Arc::new(MockService::new());
    let app = build_router(svc.clone());

    let req = json_request(
        "POST",
        "/v1/sandboxes",
        serde_json::json!({
            "image": "nginx:alpine",
            "exposed_port": 80
        }),
    );

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let body = body_json(resp).await;
    assert!(body.get("sandbox_id").is_some());
    assert!(body.get("subdomain").is_some());
    assert_eq!(body["status"], "running");
}

#[tokio::test]
async fn get_sandbox_returns_200_for_existing_sandbox() {
    let svc = Arc::new(MockService::new());
    let id = svc.sandbox.sandbox_id.to_string();
    let app = build_router(svc);

    let req = empty_request("GET", &format!("/v1/sandboxes/{id}"));
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = body_json(resp).await;
    assert_eq!(body["sandbox_id"], id);
}

#[tokio::test]
async fn get_sandbox_returns_404_for_unknown_sandbox() {
    let svc = Arc::new(MockService::new());
    let app = build_router(svc);
    let unknown_id = SandboxId::new().to_string();

    let req = empty_request("GET", &format!("/v1/sandboxes/{unknown_id}"));
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = body_json(resp).await;
    assert_eq!(body["error_code"], "SANDBOX_NOT_FOUND");
}

#[tokio::test]
async fn get_sandbox_returns_400_for_invalid_uuid() {
    let svc = Arc::new(MockService::new());
    let app = build_router(svc);

    let req = empty_request("GET", "/v1/sandboxes/not-a-uuid");
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(body["error_code"], "INVALID_REQUEST");
}

#[tokio::test]
async fn delete_sandbox_returns_204_for_existing_sandbox() {
    let svc = Arc::new(MockService::new());
    let id = svc.sandbox.sandbox_id.to_string();
    let app = build_router(svc);

    let req = empty_request("DELETE", &format!("/v1/sandboxes/{id}"));
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn delete_sandbox_returns_404_for_unknown_sandbox() {
    let svc = Arc::new(MockService::new());
    let app = build_router(svc);
    let unknown_id = SandboxId::new().to_string();

    let req = empty_request("DELETE", &format!("/v1/sandboxes/{unknown_id}"));
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn exec_returns_stdout_and_exit_code() {
    use base64::Engine;
    let engine = base64::engine::general_purpose::STANDARD;
    let svc = Arc::new(MockService::new());
    let id = svc.sandbox.sandbox_id.to_string();
    let app = build_router(svc);

    let req = json_request(
        "POST",
        &format!("/v1/sandboxes/{id}/exec"),
        serde_json::json!({
            "command": ["echo", "hello"]
        }),
    );

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = body_json(resp).await;
    assert_eq!(body["exit_code"], 0);
    assert_eq!(body["stdout_b64"], engine.encode(b"hello\n"));
    assert_eq!(body["stderr_b64"], engine.encode(b""));
    assert!(body.get("error_code").is_none());
}

#[tokio::test]
async fn exec_returns_404_for_unknown_sandbox() {
    let svc = Arc::new(MockService::new());
    let app = build_router(svc);
    let unknown_id = SandboxId::new().to_string();

    let req = json_request(
        "POST",
        &format!("/v1/sandboxes/{unknown_id}/exec"),
        serde_json::json!({
            "command": ["echo", "hello"]
        }),
    );

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn create_sandbox_uses_defaults_for_omitted_fields() {
    use std::sync::Mutex;

    struct CapturingService {
        captured: Mutex<Option<CreateRequest>>,
        info: SandboxInfo,
    }

    impl SandboxService for CapturingService {
        async fn create(&self, request: CreateRequest) -> Result<SandboxInfo, ApiError> {
            *self.captured.lock().unwrap() = Some(request);
            Ok(self.info.clone())
        }

        async fn get(&self, _: &SandboxId) -> Result<SandboxInfo, ApiError> {
            unreachable!()
        }

        async fn list(&self) -> Result<Vec<SandboxInfo>, ApiError> {
            unreachable!()
        }

        async fn delete(&self, _: &SandboxId) -> Result<(), ApiError> {
            unreachable!()
        }

        async fn exec(&self, _: &SandboxId, _: ExecRequest) -> Result<ExecOutput, ApiError> {
            unreachable!()
        }

        async fn write_files(
            &self,
            _: &SandboxId,
            _: WriteFilesRequest,
        ) -> Result<WriteFilesResult, ApiError> {
            unreachable!()
        }

        async fn write_file(
            &self,
            _: &SandboxId,
            _: WriteFileRequest,
        ) -> Result<WriteFilesResult, ApiError> {
            unreachable!()
        }

        async fn read_file(&self, _: &SandboxId, _: ReadFileRequest) -> Result<Vec<u8>, ApiError> {
            unreachable!()
        }
    }

    let sandbox_id = SandboxId::new();
    let svc = Arc::new(CapturingService {
        captured: Mutex::new(None),
        info: SandboxInfo {
            subdomain: sandbox_id.subdomain(),
            sandbox_id,
            agent_id: "agent-1".into(),
            status: "running".into(),
        },
    });
    let app = build_router(svc.clone());

    let req = json_request(
        "POST",
        "/v1/sandboxes",
        serde_json::json!({"image": "python:3.12"}),
    );

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let captured = svc.captured.lock().unwrap().take().unwrap();
    assert_eq!(
        captured.cpu_millicores,
        open_sandbox_contracts::constants::DEFAULT_SANDBOX_CPU_MILLICORES
    );
    assert_eq!(
        captured.memory_bytes,
        open_sandbox_contracts::constants::DEFAULT_SANDBOX_MEMORY_BYTES
    );
    assert!(captured.env_vars.is_empty());
}

#[tokio::test]
async fn write_files_returns_200_with_result() {
    let svc = Arc::new(MockService::new());
    let id = svc.sandbox.sandbox_id.to_string();
    let app = build_router(svc);

    let req = Request::builder()
        .method("POST")
        .uri(format!("/v1/sandboxes/{id}/files/write_files"))
        .header("content-type", "application/gzip")
        .body(Body::from(vec![0x1f, 0x8b, 0x08]))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["success"], true);
}

#[tokio::test]
async fn write_files_returns_404_for_unknown_sandbox() {
    let svc = Arc::new(MockService::new());
    let app = build_router(svc);
    let unknown_id = SandboxId::new().to_string();

    let req = Request::builder()
        .method("POST")
        .uri(format!("/v1/sandboxes/{unknown_id}/files/write_files"))
        .header("content-type", "application/gzip")
        .body(Body::from(vec![0x1f, 0x8b, 0x08]))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn write_files_with_cwd_header() {
    let svc = Arc::new(MockService::new());
    let id = svc.sandbox.sandbox_id.to_string();
    let app = build_router(svc);

    let req = Request::builder()
        .method("POST")
        .uri(format!("/v1/sandboxes/{id}/files/write_files"))
        .header("content-type", "application/gzip")
        .header("x-cwd", "/app/src")
        .body(Body::from(vec![0x1f, 0x8b, 0x08]))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["success"], true);
}

#[tokio::test]
async fn write_files_returns_400_for_empty_body() {
    let svc = Arc::new(MockService::new());
    let id = svc.sandbox.sandbox_id.to_string();
    let app = build_router(svc);

    let req = Request::builder()
        .method("POST")
        .uri(format!("/v1/sandboxes/{id}/files/write_files"))
        .header("content-type", "application/gzip")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(body["error_code"], "INVALID_UPLOAD");
}

#[tokio::test]
async fn write_files_returns_400_for_non_gzip_body() {
    let svc = Arc::new(MockService::new());
    let id = svc.sandbox.sandbox_id.to_string();
    let app = build_router(svc);

    let req = Request::builder()
        .method("POST")
        .uri(format!("/v1/sandboxes/{id}/files/write_files"))
        .header("content-type", "application/gzip")
        .body(Body::from(b"this is not gzip".to_vec()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(body["error_code"], "INVALID_UPLOAD");
}

#[tokio::test]
async fn write_file_returns_200_for_existing_sandbox() {
    let svc = Arc::new(MockService::new());
    let id = svc.sandbox.sandbox_id.to_string();
    let app = build_router(svc);

    let req = json_request(
        "POST",
        &format!("/v1/sandboxes/{id}/files/write_file"),
        serde_json::json!({
            "path": "hello.py",
            "content": "print('hi')\n",
            "cwd": "/home"
        }),
    );

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["success"], true);
}

#[tokio::test]
async fn write_file_rejects_both_content_and_content_b64() {
    let svc = Arc::new(MockService::new());
    let id = svc.sandbox.sandbox_id.to_string();
    let app = build_router(svc);

    let req = json_request(
        "POST",
        &format!("/v1/sandboxes/{id}/files/write_file"),
        serde_json::json!({
            "path": "a.txt",
            "content": "x",
            "content_b64": "eA=="
        }),
    );

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(body["error_code"], "INVALID_REQUEST");
}

#[tokio::test]
async fn write_file_rejects_neither_content_nor_content_b64() {
    let svc = Arc::new(MockService::new());
    let id = svc.sandbox.sandbox_id.to_string();
    let app = build_router(svc);

    let req = json_request(
        "POST",
        &format!("/v1/sandboxes/{id}/files/write_file"),
        serde_json::json!({"path": "a.txt"}),
    );

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(body["error_code"], "INVALID_REQUEST");
}

#[tokio::test]
async fn list_sandboxes_returns_caller_owned_set() {
    let svc = Arc::new(MockService::new());
    let app = build_router(svc.clone());

    let req = empty_request("GET", "/v1/sandboxes");
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let arr = body["sandboxes"].as_array().expect("array");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["sandbox_id"], svc.sandbox.sandbox_id.to_string());
}

#[tokio::test]
async fn exec_rejects_unknown_fields() {
    let svc = Arc::new(MockService::new());
    let id = svc.sandbox.sandbox_id.to_string();
    let app = build_router(svc);

    let req = json_request(
        "POST",
        &format!("/v1/sandboxes/{id}/exec"),
        serde_json::json!({
            "command": ["echo"],
            "totally_unsupported_option": true
        }),
    );

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(body["error_code"], "INVALID_REQUEST");
}

#[tokio::test]
async fn exec_rejects_both_stdin_and_stdin_b64() {
    let svc = Arc::new(MockService::new());
    let id = svc.sandbox.sandbox_id.to_string();
    let app = build_router(svc);

    let req = json_request(
        "POST",
        &format!("/v1/sandboxes/{id}/exec"),
        serde_json::json!({
            "command": ["cat"],
            "stdin": "hi",
            "stdin_b64": "aGk="
        }),
    );

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(body["error_code"], "INVALID_REQUEST");
}

#[tokio::test]
async fn exec_surfaces_command_not_found_in_response_envelope() {
    use base64::Engine;
    use std::sync::Mutex;

    struct NotFoundSvc {
        info: SandboxInfo,
        captured_stdin: Mutex<Vec<u8>>,
    }

    impl SandboxService for NotFoundSvc {
        async fn create(&self, _: CreateRequest) -> Result<SandboxInfo, ApiError> {
            unreachable!()
        }
        async fn get(&self, _: &SandboxId) -> Result<SandboxInfo, ApiError> {
            unreachable!()
        }
        async fn list(&self) -> Result<Vec<SandboxInfo>, ApiError> {
            unreachable!()
        }
        async fn delete(&self, _: &SandboxId) -> Result<(), ApiError> {
            unreachable!()
        }
        async fn exec(
            &self,
            _: &SandboxId,
            request: ExecRequest,
        ) -> Result<ExecOutput, ApiError> {
            *self.captured_stdin.lock().unwrap() = request.stdin_bytes().unwrap();
            Ok(ExecOutput {
                exit_code: 127,
                stdout: vec![],
                stderr: b"exec: nonexistent: not found\n".to_vec(),
                command_not_found: true,
            })
        }
        async fn write_files(
            &self,
            _: &SandboxId,
            _: WriteFilesRequest,
        ) -> Result<WriteFilesResult, ApiError> {
            unreachable!()
        }
        async fn write_file(
            &self,
            _: &SandboxId,
            _: WriteFileRequest,
        ) -> Result<WriteFilesResult, ApiError> {
            unreachable!()
        }
        async fn read_file(&self, _: &SandboxId, _: ReadFileRequest) -> Result<Vec<u8>, ApiError> {
            unreachable!()
        }
    }

    let sandbox_id = SandboxId::new();
    let svc = Arc::new(NotFoundSvc {
        info: SandboxInfo {
            subdomain: sandbox_id.subdomain(),
            sandbox_id: sandbox_id.clone(),
            agent_id: "agent-1".into(),
            status: "running".into(),
        },
        captured_stdin: Mutex::new(Vec::new()),
    });
    let app = build_router(svc.clone());

    let req = json_request(
        "POST",
        &format!("/v1/sandboxes/{}/exec", svc.info.sandbox_id),
        serde_json::json!({
            "command": ["nonexistent_binary"]
        }),
    );

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["exit_code"], 127);
    assert_eq!(body["error_code"], "COMMAND_NOT_FOUND");
    let engine = base64::engine::general_purpose::STANDARD;
    assert_eq!(
        body["stderr_b64"],
        engine.encode(b"exec: nonexistent: not found\n")
    );
}

#[tokio::test]
async fn exec_passes_stdin_through() {
    use std::sync::Mutex;

    struct CapturingExec {
        info: SandboxInfo,
        captured: Mutex<Vec<u8>>,
    }

    impl SandboxService for CapturingExec {
        async fn create(&self, _: CreateRequest) -> Result<SandboxInfo, ApiError> {
            unreachable!()
        }
        async fn get(&self, _: &SandboxId) -> Result<SandboxInfo, ApiError> {
            unreachable!()
        }
        async fn list(&self) -> Result<Vec<SandboxInfo>, ApiError> {
            unreachable!()
        }
        async fn delete(&self, _: &SandboxId) -> Result<(), ApiError> {
            unreachable!()
        }
        async fn exec(
            &self,
            _: &SandboxId,
            request: ExecRequest,
        ) -> Result<ExecOutput, ApiError> {
            let bytes = request.stdin_bytes().unwrap();
            *self.captured.lock().unwrap() = bytes.clone();
            Ok(ExecOutput {
                exit_code: 0,
                stdout: bytes,
                stderr: vec![],
                command_not_found: false,
            })
        }
        async fn write_files(
            &self,
            _: &SandboxId,
            _: WriteFilesRequest,
        ) -> Result<WriteFilesResult, ApiError> {
            unreachable!()
        }
        async fn write_file(
            &self,
            _: &SandboxId,
            _: WriteFileRequest,
        ) -> Result<WriteFilesResult, ApiError> {
            unreachable!()
        }
        async fn read_file(&self, _: &SandboxId, _: ReadFileRequest) -> Result<Vec<u8>, ApiError> {
            unreachable!()
        }
    }

    let sandbox_id = SandboxId::new();
    let svc = Arc::new(CapturingExec {
        info: SandboxInfo {
            subdomain: sandbox_id.subdomain(),
            sandbox_id,
            agent_id: "a".into(),
            status: "running".into(),
        },
        captured: Mutex::new(Vec::new()),
    });
    let app = build_router(svc.clone());

    let req = json_request(
        "POST",
        &format!("/v1/sandboxes/{}/exec", svc.info.sandbox_id),
        serde_json::json!({"command": ["cat"], "stdin": "hello stdin"}),
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(svc.captured.lock().unwrap().as_slice(), b"hello stdin");
}

#[tokio::test]
async fn read_file_returns_octet_stream() {
    let svc = Arc::new(MockService::new());
    let id = svc.sandbox.sandbox_id.to_string();
    let app = build_router(svc);

    let req = empty_request(
        "GET",
        &format!("/v1/sandboxes/{id}/files/read?path=/etc/hostname"),
    );

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "application/octet-stream"
    );
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(body.as_ref(), b"contents of /etc/hostname");
}

#[tokio::test]
async fn read_file_returns_404_for_unknown_sandbox() {
    let svc = Arc::new(MockService::new());
    let app = build_router(svc);
    let unknown_id = SandboxId::new().to_string();

    let req = empty_request(
        "GET",
        &format!("/v1/sandboxes/{unknown_id}/files/read?path=/etc/hostname"),
    );

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn read_file_with_cwd() {
    let svc = Arc::new(MockService::new());
    let id = svc.sandbox.sandbox_id.to_string();
    let app = build_router(svc);

    let req = empty_request(
        "GET",
        &format!("/v1/sandboxes/{id}/files/read?path=main.rs&cwd=/app/src"),
    );

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(body.as_ref(), b"contents of main.rs");
}

#[tokio::test]
async fn read_file_404_includes_resolved_path() {
    struct MissingSvc {
        info: SandboxInfo,
    }
    impl SandboxService for MissingSvc {
        async fn create(&self, _: CreateRequest) -> Result<SandboxInfo, ApiError> {
            unreachable!()
        }
        async fn get(&self, _: &SandboxId) -> Result<SandboxInfo, ApiError> {
            unreachable!()
        }
        async fn list(&self) -> Result<Vec<SandboxInfo>, ApiError> {
            unreachable!()
        }
        async fn delete(&self, _: &SandboxId) -> Result<(), ApiError> {
            unreachable!()
        }
        async fn exec(&self, _: &SandboxId, _: ExecRequest) -> Result<ExecOutput, ApiError> {
            unreachable!()
        }
        async fn write_files(
            &self,
            _: &SandboxId,
            _: WriteFilesRequest,
        ) -> Result<WriteFilesResult, ApiError> {
            unreachable!()
        }
        async fn write_file(
            &self,
            _: &SandboxId,
            _: WriteFileRequest,
        ) -> Result<WriteFilesResult, ApiError> {
            unreachable!()
        }
        async fn read_file(&self, _: &SandboxId, _: ReadFileRequest) -> Result<Vec<u8>, ApiError> {
            Err(ApiError::FileNotFound {
                resolved_path: "/home/missing.py".into(),
            })
        }
    }
    let sandbox_id = SandboxId::new();
    let svc = Arc::new(MissingSvc {
        info: SandboxInfo {
            subdomain: sandbox_id.subdomain(),
            sandbox_id,
            agent_id: "a".into(),
            status: "running".into(),
        },
    });
    let app = build_router(svc.clone());

    let req = empty_request(
        "GET",
        &format!(
            "/v1/sandboxes/{}/files/read?path=missing.py",
            svc.info.sandbox_id
        ),
    );

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = body_json(resp).await;
    assert_eq!(body["error_code"], "FILE_NOT_FOUND");
    let msg = body["error"].as_str().unwrap();
    assert!(
        msg.contains("/home/missing.py"),
        "error message should include resolved path, got: {msg}"
    );
}
