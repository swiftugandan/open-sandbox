use std::sync::{Arc, LazyLock};
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::sync::Mutex as AsyncMutex;
use tokio_stream::wrappers::TcpListenerStream;

static DB_LOCK: LazyLock<AsyncMutex<()>> = LazyLock::new(|| AsyncMutex::new(()));

use open_sandbox_agent_docker::DockerRuntime;
use open_sandbox::http_client::ReqwestHttpClient;
use open_sandbox_agent::container::ContainerRuntime;
use open_sandbox_agent::controller_client::ControllerConnection;
use open_sandbox_agent::sandbox::SandboxManager;
use open_sandbox_agent::tunnel::TunnelForwarder;
use open_sandbox_contracts::controller::AgentResources;
use open_sandbox_contracts::types::{AgentId, JoinToken};

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

struct TestEnv {
    api_url: String,
    runtime: Arc<DockerRuntime>,
    sandbox_manager: Arc<SandboxManager<DockerRuntime>>,
    _guard: tokio::sync::MutexGuard<'static, ()>,
}

async fn setup() -> TestEnv {
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

    let runtime = Arc::new(DockerRuntime::connect().unwrap());
    let sandbox_manager = Arc::new(SandboxManager::new(runtime.clone()));
    let http_client = Arc::new(ReqwestHttpClient::new());
    let forwarder = Arc::new(TunnelForwarder::new(sandbox_manager.clone(), http_client));

    let agent_id = AgentId::new();
    let resources = AgentResources {
        cpu_cores: 4,
        memory_bytes: 8_000_000_000,
        arch: 1,
        os: std::env::consts::OS.into(),
    };

    let conn = ControllerConnection::new(
        agent_id,
        JoinToken::new("test-token".into()),
        sandbox_manager.clone(),
        resources,
    );

    let controller_addr = grpc_addr.clone();
    tokio::spawn(async move { conn.run(&controller_addr).await });

    // Wait for agent to register and be available for scheduling
    tokio::time::sleep(Duration::from_secs(2)).await;

    let api_svc = GrpcSandboxService::connect(&grpc_addr).await.unwrap();
    let router = build_router(Arc::new(api_svc));
    let http_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_url = format!("http://{}", http_listener.local_addr().unwrap());

    tokio::spawn(async move {
        axum::serve(http_listener, router).await.unwrap();
    });

    // ReqwestHttpClient / forwarder kept alive via proxy — not needed here
    // but the Arc prevents the TunnelForwarder from being dropped
    let _keep = forwarder;

    TestEnv {
        api_url,
        runtime,
        sandbox_manager,
        _guard: guard,
    }
}

fn create_tar_gz(file_name: &str, content: &[u8]) -> Vec<u8> {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;

    let gz_buf = Vec::new();
    let gz = GzEncoder::new(gz_buf, Compression::fast());
    let mut archive = tar::Builder::new(gz);

    let mut header = tar::Header::new_gnu();
    header.set_path(file_name).unwrap();
    header.set_size(content.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();

    archive.append(&header, content).unwrap();
    let gz = archive.into_inner().unwrap();
    gz.finish().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn live_create_sandbox_through_api_to_real_agent() {
    let env = setup().await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{}/v1/sandboxes", env.api_url))
        .json(&serde_json::json!({
            "image": "nginx:alpine",
            "cpu_millicores": 500,
            "memory_bytes": 268435456
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 201);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["sandbox_id"].is_string());
    assert_eq!(body["status"], "running");

    // Wait for agent to process StartSandbox
    tokio::time::sleep(Duration::from_secs(3)).await;

    let sandbox_id: open_sandbox_contracts::types::SandboxId =
        uuid::Uuid::parse_str(body["sandbox_id"].as_str().unwrap())
            .unwrap()
            .into();
    let entry = env.sandbox_manager.get_sandbox(&sandbox_id);
    assert!(entry.is_some(), "container should be running on agent");

    // Cleanup
    let entry = entry.unwrap();
    let _ = env
        .runtime
        .stop_and_remove(&entry.container_id, Duration::from_secs(5))
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn live_exec_runs_command_in_real_container() {
    let env = setup().await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{}/v1/sandboxes", env.api_url))
        .json(&serde_json::json!({"image": "nginx:alpine"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let body: serde_json::Value = resp.json().await.unwrap();
    let sandbox_id = body["sandbox_id"].as_str().unwrap();

    // Wait for container to be running
    tokio::time::sleep(Duration::from_secs(3)).await;

    let resp = client
        .post(format!("{}/v1/sandboxes/{sandbox_id}/exec", env.api_url))
        .json(&serde_json::json!({"command": ["echo", "hello-from-exec"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["exit_code"], 0);
    assert!(
        body["stdout"].as_str().unwrap().contains("hello-from-exec"),
        "stdout should contain the echoed text"
    );

    // Cleanup
    let sid: open_sandbox_contracts::types::SandboxId =
        uuid::Uuid::parse_str(sandbox_id).unwrap().into();
    if let Some(entry) = env.sandbox_manager.get_sandbox(&sid) {
        let _ = env
            .runtime
            .stop_and_remove(&entry.container_id, Duration::from_secs(5))
            .await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn live_read_file_from_real_container() {
    let env = setup().await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{}/v1/sandboxes", env.api_url))
        .json(&serde_json::json!({"image": "nginx:alpine"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let body: serde_json::Value = resp.json().await.unwrap();
    let sandbox_id = body["sandbox_id"].as_str().unwrap();

    tokio::time::sleep(Duration::from_secs(3)).await;

    // Write a known file via exec
    let resp = client
        .post(format!("{}/v1/sandboxes/{sandbox_id}/exec", env.api_url))
        .json(&serde_json::json!({
            "command": ["sh", "-c", "echo -n 'read-test-content' > /tmp/read-test.txt"]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Read it back via the file read endpoint
    let resp = client
        .post(format!(
            "{}/v1/sandboxes/{sandbox_id}/files/read",
            env.api_url
        ))
        .json(&serde_json::json!({"path": "/tmp/read-test.txt"}))
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
    assert_eq!(body.as_ref(), b"read-test-content");

    // Cleanup
    let sid: open_sandbox_contracts::types::SandboxId =
        uuid::Uuid::parse_str(sandbox_id).unwrap().into();
    if let Some(entry) = env.sandbox_manager.get_sandbox(&sid) {
        let _ = env
            .runtime
            .stop_and_remove(&entry.container_id, Duration::from_secs(5))
            .await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn live_write_files_to_real_container() {
    let env = setup().await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{}/v1/sandboxes", env.api_url))
        .json(&serde_json::json!({"image": "nginx:alpine"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let body: serde_json::Value = resp.json().await.unwrap();
    let sandbox_id = body["sandbox_id"].as_str().unwrap();

    tokio::time::sleep(Duration::from_secs(3)).await;

    // Write a file via tar.gz upload
    let tar_data = create_tar_gz("written-by-api.txt", b"tar-archive-content");
    let resp = client
        .post(format!(
            "{}/v1/sandboxes/{sandbox_id}/files/write",
            env.api_url
        ))
        .header("content-type", "application/gzip")
        .header("x-cwd", "/tmp")
        .body(tar_data)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Verify the file exists via exec
    let resp = client
        .post(format!("{}/v1/sandboxes/{sandbox_id}/exec", env.api_url))
        .json(&serde_json::json!({"command": ["cat", "/tmp/written-by-api.txt"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["exit_code"], 0);
    assert_eq!(body["stdout"], "tar-archive-content");

    // Cleanup
    let sid: open_sandbox_contracts::types::SandboxId =
        uuid::Uuid::parse_str(sandbox_id).unwrap().into();
    if let Some(entry) = env.sandbox_manager.get_sandbox(&sid) {
        let _ = env
            .runtime
            .stop_and_remove(&entry.container_id, Duration::from_secs(5))
            .await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn live_full_file_roundtrip() {
    let env = setup().await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{}/v1/sandboxes", env.api_url))
        .json(&serde_json::json!({"image": "nginx:alpine"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let body: serde_json::Value = resp.json().await.unwrap();
    let sandbox_id = body["sandbox_id"].as_str().unwrap();

    tokio::time::sleep(Duration::from_secs(3)).await;

    // Write via tar
    let tar_data = create_tar_gz("roundtrip.txt", b"full-circle");
    let resp = client
        .post(format!(
            "{}/v1/sandboxes/{sandbox_id}/files/write",
            env.api_url
        ))
        .header("content-type", "application/gzip")
        .header("x-cwd", "/tmp")
        .body(tar_data)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Read back via file read endpoint
    let resp = client
        .post(format!(
            "{}/v1/sandboxes/{sandbox_id}/files/read",
            env.api_url
        ))
        .json(&serde_json::json!({"path": "/tmp/roundtrip.txt"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.bytes().await.unwrap();
    assert_eq!(body.as_ref(), b"full-circle");

    // Delete sandbox
    let resp = client
        .delete(format!("{}/v1/sandboxes/{sandbox_id}", env.api_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);

    // Cleanup container (delete sends StopSandbox but agent may not process it in time)
    let sid: open_sandbox_contracts::types::SandboxId =
        uuid::Uuid::parse_str(sandbox_id).unwrap().into();
    if let Some(entry) = env.sandbox_manager.get_sandbox(&sid) {
        let _ = env
            .runtime
            .stop_and_remove(&entry.container_id, Duration::from_secs(5))
            .await;
    }
}
