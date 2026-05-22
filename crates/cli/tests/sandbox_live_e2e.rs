use std::sync::Arc;
use std::time::Duration;

use tokio_stream::wrappers::TcpListenerStream;

use open_sandbox_agent_docker::DockerRuntime;
use open_sandbox::http_client::ReqwestHttpClient;
use open_sandbox_agent::container::ContainerRuntime;
use open_sandbox_agent::controller_client::ControllerConnection;
use open_sandbox_agent::proxy_client::ProxyConnection;
use open_sandbox_agent::sandbox::SandboxManager;
use open_sandbox_agent::tunnel::{ForwardRequest, TunnelForwarder};
use open_sandbox_contracts::controller::{AgentResources, SandboxState};
use open_sandbox_contracts::types::{AgentId, JoinToken, SandboxId};
use open_sandbox_controller::grpc::{Controller, CreateSandboxRequest};
use open_sandbox_controller::scheduler::SandboxRequirements;
use open_sandbox_controller::testutil::{AcceptAllTokens, InMemoryStore};

use open_sandbox_proxy::grpc::tunnel_service;
use open_sandbox_proxy::stream_mux::StreamMux;
use open_sandbox_proxy::tunnel_pool::TunnelPool;

async fn start_controller() -> (Controller<InMemoryStore>, String) {
    let store = Arc::new(InMemoryStore::new());
    let controller = Controller::new(store, AcceptAllTokens);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = format!("http://{}", listener.local_addr().unwrap());

    let service = controller.grpc_service();
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(service)
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .unwrap();
    });

    (controller, addr)
}

async fn start_proxy() -> String {
    let tunnel_pool = Arc::new(TunnelPool::new());
    let mux = Arc::new(StreamMux::new(tunnel_pool.clone()));
    let service = tunnel_service(mux, tunnel_pool);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = format!("http://{}", listener.local_addr().unwrap());

    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(service)
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .unwrap();
    });

    addr
}

#[tokio::test(flavor = "multi_thread")]
async fn controller_triggers_sandbox_creation_on_real_agent() {
    let (controller, controller_addr) = start_controller().await;
    let proxy_addr = start_proxy().await;

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

    let proxy_conn = ProxyConnection::new(AgentId::new(), forwarder);
    let proxy_addr_clone = proxy_addr.clone();
    tokio::spawn(async move { proxy_conn.run(&proxy_addr_clone).await });

    let controller_addr_clone = controller_addr.clone();
    let agent_handle = tokio::spawn(async move { conn.run(&controller_addr_clone).await });

    // Wait for agent to register
    tokio::time::sleep(Duration::from_secs(2)).await;

    let sandbox_id = SandboxId::new();
    controller
        .create_sandbox(CreateSandboxRequest {
            sandbox_id: sandbox_id.clone(),
            image: "nginx:alpine".into(),
            requirements: SandboxRequirements {
                cpu_millicores: 500,
                memory_bytes: 256 * 1024 * 1024,
            },
            env_vars: std::collections::HashMap::new(),
            exposed_port: 80,
        })
        .await
        .unwrap();

    // Wait for agent to pull image and start container
    tokio::time::sleep(Duration::from_secs(10)).await;

    let entry = sandbox_manager.get_sandbox(&sandbox_id);
    assert!(
        entry.is_some(),
        "sandbox should exist in manager after creation"
    );
    let entry = entry.unwrap();
    assert_eq!(entry.state, SandboxState::Running);
    assert!(entry.host_port > 0);

    // Wait for nginx to accept connections
    let client = reqwest::Client::new();
    let mut ready = false;
    for _ in 0..20 {
        if client
            .get(format!("http://127.0.0.1:{}/", entry.host_port))
            .send()
            .await
            .is_ok()
        {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(ready, "nginx should be serving on the mapped port");

    // Cleanup
    let _ = runtime
        .stop_and_remove(&entry.container_id, Duration::from_secs(5))
        .await;
    agent_handle.abort();
}
