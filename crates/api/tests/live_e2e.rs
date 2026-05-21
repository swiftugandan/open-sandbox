use std::sync::{Arc, LazyLock};

use tokio::net::TcpListener;
use tokio::sync::{Mutex as AsyncMutex, mpsc};
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};

static DB_LOCK: LazyLock<AsyncMutex<()>> = LazyLock::new(|| AsyncMutex::new(()));

use open_sandbox_contracts::controller::controller_service_client::ControllerServiceClient;
use open_sandbox_contracts::controller::{
    AgentMessage, AgentResources, ExecResult, RegisterRequest, agent_message, controller_command,
};
use open_sandbox_contracts::types::{AgentId, SandboxId};

use open_sandbox_api::grpc_service::GrpcSandboxService;
use open_sandbox_api::router::build_router;

use open_sandbox_controller::grpc::Controller;
use open_sandbox_controller::management::management_service;
use open_sandbox_controller::pg_store::PgStore;
use open_sandbox_controller::token::TokenValidator;

struct AcceptAllTokens;
impl TokenValidator for AcceptAllTokens {
    fn validate(&self, _token: &str) -> bool {
        true
    }
}

async fn setup() -> (
    String,
    String,
    AgentId,
    SandboxId,
    tokio::sync::MutexGuard<'static, ()>,
) {
    let guard = DB_LOCK.lock().await;
    let db_url = "postgres://postgres:test@127.0.0.1:5433/open_sandbox";
    let pool = sqlx::PgPool::connect(db_url)
        .await
        .expect("cannot connect to test Postgres on port 5433");

    sqlx::query("DROP TABLE IF EXISTS routing_entries")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DROP TABLE IF EXISTS agents")
        .execute(&pool)
        .await
        .unwrap();

    let pg_store = Arc::new(PgStore::new(pool));
    pg_store.migrate().await.unwrap();

    let controller = Arc::new(Controller::new(pg_store, AcceptAllTokens));
    let agent_svc = controller.grpc_service();
    let exec_broker = controller.exec_broker();
    let mgmt_svc = management_service(controller.clone(), exec_broker);

    let grpc_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let grpc_addr = format!("http://{}", grpc_listener.local_addr().unwrap());

    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(agent_svc)
            .add_service(mgmt_svc)
            .serve_with_incoming(TcpListenerStream::new(grpc_listener))
            .await
            .unwrap();
    });

    let agent_id = AgentId::new();
    let agent_id_str = agent_id.to_string();

    let channel = tonic::transport::Channel::from_shared(grpc_addr.clone())
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut client = ControllerServiceClient::new(channel);

    let (agent_tx, agent_rx) = mpsc::channel::<AgentMessage>(32);
    let outbound = ReceiverStream::new(agent_rx);
    let response = client.agent_stream(outbound).await.unwrap();
    let mut inbound = response.into_inner();

    agent_tx
        .send(AgentMessage {
            payload: Some(agent_message::Payload::Register(RegisterRequest {
                agent_id: agent_id_str.clone(),
                join_token: "test-token".into(),
                resources: Some(AgentResources {
                    cpu_cores: 4,
                    memory_bytes: 8_000_000_000,
                    arch: 1,
                    os: "linux".into(),
                }),
            })),
        })
        .await
        .unwrap();

    let msg = inbound.message().await.unwrap().unwrap();
    match msg.payload.unwrap() {
        controller_command::Payload::RegisterResponse(resp) => {
            assert!(resp.accepted, "agent registration should be accepted");
        }
        other => panic!("expected RegisterResponse, got {other:?}"),
    }

    tokio::spawn(async move {
        while let Ok(Some(msg)) = inbound.message().await {
            match msg.payload {
                Some(controller_command::Payload::Exec(exec)) => {
                    let stdout = if exec.command.first().map(|s| s.as_str()) == Some("cat") {
                        let path = exec.command.last().cloned().unwrap_or_default();
                        format!("live-content-of:{path}").into_bytes()
                    } else if exec.command.first().map(|s| s.as_str()) == Some("tar") {
                        vec![]
                    } else {
                        format!("ran: {}", exec.command.join(" ")).into_bytes()
                    };
                    let result = AgentMessage {
                        payload: Some(agent_message::Payload::ExecResult(ExecResult {
                            sandbox_id: exec.sandbox_id,
                            exec_id: exec.exec_id,
                            exit_code: 0,
                            stdout,
                            stderr: vec![],
                        })),
                    };
                    let _ = agent_tx.send(result).await;
                }
                _ => {}
            }
        }
    });

    let api_svc = GrpcSandboxService::connect(&grpc_addr).await.unwrap();
    let router = build_router(Arc::new(api_svc));
    let http_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_url = format!("http://{}", http_listener.local_addr().unwrap());

    tokio::spawn(async move {
        axum::serve(http_listener, router).await.unwrap();
    });

    let sandbox_id = SandboxId::new();
    (api_url, grpc_addr, agent_id, sandbox_id, guard)
}

#[tokio::test(flavor = "multi_thread")]
async fn live_create_sandbox_through_api_to_real_controller() {
    let (api_url, _, _, _, _guard) = setup().await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{api_url}/v1/sandboxes"))
        .json(&serde_json::json!({
            "image": "nginx:alpine",
            "cpu_millicores": 1000,
            "memory_bytes": 512000000
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 201);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["sandbox_id"].is_string());
    assert!(body["subdomain"].is_string());
    assert_eq!(body["status"], "running");
}

#[tokio::test(flavor = "multi_thread")]
async fn live_create_then_get_sandbox() {
    let (api_url, _, _, _, _guard) = setup().await;

    let client = reqwest::Client::new();
    let create_resp = client
        .post(format!("{api_url}/v1/sandboxes"))
        .json(&serde_json::json!({"image": "python:3.12"}))
        .send()
        .await
        .unwrap();
    assert_eq!(create_resp.status(), 201);
    let created: serde_json::Value = create_resp.json().await.unwrap();
    let sandbox_id = created["sandbox_id"].as_str().unwrap();

    let get_resp = client
        .get(format!("{api_url}/v1/sandboxes/{sandbox_id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(get_resp.status(), 200);
    let body: serde_json::Value = get_resp.json().await.unwrap();
    assert_eq!(body["sandbox_id"], sandbox_id);
    assert_eq!(body["status"], "running");
}

#[tokio::test(flavor = "multi_thread")]
async fn live_create_then_delete_sandbox() {
    let (api_url, _, _, _, _guard) = setup().await;

    let client = reqwest::Client::new();
    let create_resp = client
        .post(format!("{api_url}/v1/sandboxes"))
        .json(&serde_json::json!({"image": "alpine:latest"}))
        .send()
        .await
        .unwrap();
    assert_eq!(create_resp.status(), 201);
    let created: serde_json::Value = create_resp.json().await.unwrap();
    let sandbox_id = created["sandbox_id"].as_str().unwrap();

    let del_resp = client
        .delete(format!("{api_url}/v1/sandboxes/{sandbox_id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(del_resp.status(), 204);

    let get_resp = client
        .get(format!("{api_url}/v1/sandboxes/{sandbox_id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(get_resp.status(), 404);
}

#[tokio::test(flavor = "multi_thread")]
async fn live_write_files_through_exec_pipeline() {
    let (api_url, _, _, _, _guard) = setup().await;

    let client = reqwest::Client::new();
    let create_resp = client
        .post(format!("{api_url}/v1/sandboxes"))
        .json(&serde_json::json!({"image": "alpine:latest"}))
        .send()
        .await
        .unwrap();
    assert_eq!(create_resp.status(), 201);
    let created: serde_json::Value = create_resp.json().await.unwrap();
    let sandbox_id = created["sandbox_id"].as_str().unwrap();

    let tar_data = vec![0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00];
    let resp = client
        .post(format!("{api_url}/v1/sandboxes/{sandbox_id}/files/write"))
        .header("content-type", "application/gzip")
        .header("x-cwd", "/app")
        .body(tar_data)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
}

#[tokio::test(flavor = "multi_thread")]
async fn live_read_file_through_exec_pipeline() {
    let (api_url, _, _, _, _guard) = setup().await;

    let client = reqwest::Client::new();
    let create_resp = client
        .post(format!("{api_url}/v1/sandboxes"))
        .json(&serde_json::json!({"image": "alpine:latest"}))
        .send()
        .await
        .unwrap();
    assert_eq!(create_resp.status(), 201);
    let created: serde_json::Value = create_resp.json().await.unwrap();
    let sandbox_id = created["sandbox_id"].as_str().unwrap();

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
    assert_eq!(body.as_ref(), b"live-content-of:/etc/hostname");
}
