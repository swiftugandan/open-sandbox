use std::sync::Arc;

use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::{Request, Response, Status};

use open_sandbox_contracts::api::{
    CreateSandboxRequest, CreateSandboxResponse, DeleteSandboxRequest, DeleteSandboxResponse,
    ExecSandboxRequest, ExecSandboxResponse, GetSandboxRequest, GetSandboxResponse,
    sandbox_management_service_server::{SandboxManagementService, SandboxManagementServiceServer},
};
use open_sandbox_contracts::types::SandboxId;

use open_sandbox_api::grpc_service::GrpcSandboxService;
use open_sandbox_api::router::build_router;

struct MockController {
    sandbox_id: String,
    subdomain: String,
}

#[tonic::async_trait]
impl SandboxManagementService for MockController {
    async fn create_sandbox(
        &self,
        _request: Request<CreateSandboxRequest>,
    ) -> Result<Response<CreateSandboxResponse>, Status> {
        Ok(Response::new(CreateSandboxResponse {
            sandbox_id: self.sandbox_id.clone(),
            subdomain: self.subdomain.clone(),
            agent_id: "agent-mock".into(),
            status: "creating".into(),
        }))
    }

    async fn get_sandbox(
        &self,
        request: Request<GetSandboxRequest>,
    ) -> Result<Response<GetSandboxResponse>, Status> {
        let req = request.into_inner();
        if req.sandbox_id == self.sandbox_id {
            Ok(Response::new(GetSandboxResponse {
                sandbox_id: self.sandbox_id.clone(),
                agent_id: "agent-mock".into(),
                subdomain: self.subdomain.clone(),
                status: "running".into(),
            }))
        } else {
            Err(Status::not_found(format!(
                "sandbox {} not found",
                req.sandbox_id
            )))
        }
    }

    async fn delete_sandbox(
        &self,
        request: Request<DeleteSandboxRequest>,
    ) -> Result<Response<DeleteSandboxResponse>, Status> {
        let req = request.into_inner();
        if req.sandbox_id == self.sandbox_id {
            Ok(Response::new(DeleteSandboxResponse { deleted: true }))
        } else {
            Err(Status::not_found(format!(
                "sandbox {} not found",
                req.sandbox_id
            )))
        }
    }

    async fn exec_sandbox(
        &self,
        request: Request<ExecSandboxRequest>,
    ) -> Result<Response<ExecSandboxResponse>, Status> {
        let req = request.into_inner();
        if req.sandbox_id != self.sandbox_id {
            return Err(Status::not_found(format!(
                "sandbox {} not found",
                req.sandbox_id
            )));
        }
        if req.command.first().map(|s| s.as_str()) == Some("tar") && !req.stdin.is_empty() {
            return Ok(Response::new(ExecSandboxResponse {
                exit_code: 0,
                stdout: vec![],
                stderr: vec![],
            }));
        }
        if req.command.first().map(|s| s.as_str()) == Some("cat") {
            let path = req.command.last().cloned().unwrap_or_default();
            return Ok(Response::new(ExecSandboxResponse {
                exit_code: 0,
                stdout: format!("file-content-of:{path}").into_bytes(),
                stderr: vec![],
            }));
        }
        Ok(Response::new(ExecSandboxResponse {
            exit_code: 0,
            stdout: format!("ran: {}", req.command.join(" ")).into_bytes(),
            stderr: vec![],
        }))
    }
}

async fn start_mock_controller(sandbox_id: &SandboxId) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = format!("http://{}", listener.local_addr().unwrap());

    let svc = SandboxManagementServiceServer::new(MockController {
        sandbox_id: sandbox_id.to_string(),
        subdomain: sandbox_id.subdomain(),
    });

    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(svc)
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .unwrap();
    });

    addr
}

async fn start_api_server(controller_url: &str) -> String {
    let service = GrpcSandboxService::connect(controller_url).await.unwrap();
    let router = build_router(Arc::new(service));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = format!("http://{}", listener.local_addr().unwrap());

    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    addr
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_create_then_get_then_delete() {
    let sandbox_id = SandboxId::new();
    let controller_url = start_mock_controller(&sandbox_id).await;
    let api_url = start_api_server(&controller_url).await;

    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{api_url}/v1/sandboxes"))
        .json(&serde_json::json!({"image": "nginx:alpine"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let body: serde_json::Value = resp.json().await.unwrap();
    let created_id = body["sandbox_id"].as_str().unwrap().to_string();
    assert_eq!(created_id, sandbox_id.to_string());
    assert_eq!(body["status"], "creating");

    let resp = client
        .get(format!("{api_url}/v1/sandboxes/{created_id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["sandbox_id"], created_id);
    assert_eq!(body["status"], "running");

    let resp = client
        .delete(format!("{api_url}/v1/sandboxes/{created_id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_exec_returns_stdout() {
    let sandbox_id = SandboxId::new();
    let controller_url = start_mock_controller(&sandbox_id).await;
    let api_url = start_api_server(&controller_url).await;

    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{api_url}/v1/sandboxes/{}/exec", sandbox_id))
        .json(&serde_json::json!({"command": ["echo", "hello"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["exit_code"], 0);
    assert_eq!(body["stdout"], "ran: echo hello");
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_get_unknown_sandbox_returns_404() {
    let sandbox_id = SandboxId::new();
    let controller_url = start_mock_controller(&sandbox_id).await;
    let api_url = start_api_server(&controller_url).await;

    let client = reqwest::Client::new();
    let unknown_id = SandboxId::new();

    let resp = client
        .get(format!("{api_url}/v1/sandboxes/{unknown_id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_invalid_uuid_returns_400() {
    let sandbox_id = SandboxId::new();
    let controller_url = start_mock_controller(&sandbox_id).await;
    let api_url = start_api_server(&controller_url).await;

    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{api_url}/v1/sandboxes/not-a-uuid"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_write_files_returns_200() {
    let sandbox_id = SandboxId::new();
    let controller_url = start_mock_controller(&sandbox_id).await;
    let api_url = start_api_server(&controller_url).await;

    let client = reqwest::Client::new();

    let tar_data = vec![0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00];
    let resp = client
        .post(format!("{api_url}/v1/sandboxes/{sandbox_id}/files/write"))
        .header("content-type", "application/gzip")
        .header("x-cwd", "/app")
        .body(tar_data)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["success"], true);
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_read_file_returns_content() {
    let sandbox_id = SandboxId::new();
    let controller_url = start_mock_controller(&sandbox_id).await;
    let api_url = start_api_server(&controller_url).await;

    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{api_url}/v1/sandboxes/{sandbox_id}/files/read"))
        .json(&serde_json::json!({"path": "/etc/hostname"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap(),
        "application/octet-stream"
    );
    let body = resp.bytes().await.unwrap();
    assert_eq!(body.as_ref(), b"file-content-of:/etc/hostname");
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_read_file_with_cwd_resolves_path() {
    let sandbox_id = SandboxId::new();
    let controller_url = start_mock_controller(&sandbox_id).await;
    let api_url = start_api_server(&controller_url).await;

    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{api_url}/v1/sandboxes/{sandbox_id}/files/read"))
        .json(&serde_json::json!({"path": "main.rs", "cwd": "/app/src"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.bytes().await.unwrap();
    assert_eq!(body.as_ref(), b"file-content-of:/app/src/main.rs");
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_write_files_unknown_sandbox_returns_404() {
    let sandbox_id = SandboxId::new();
    let controller_url = start_mock_controller(&sandbox_id).await;
    let api_url = start_api_server(&controller_url).await;

    let client = reqwest::Client::new();
    let unknown_id = SandboxId::new();

    let resp = client
        .post(format!(
            "{api_url}/v1/sandboxes/{unknown_id}/files/write"
        ))
        .header("content-type", "application/gzip")
        .body(vec![0x1f, 0x8b])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}
