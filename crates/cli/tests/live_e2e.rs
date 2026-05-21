use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};

use open_sandbox_contracts::controller::agent_message;
use open_sandbox_contracts::controller::controller_command;
use open_sandbox_contracts::controller::controller_service_client::ControllerServiceClient;
use open_sandbox_contracts::controller::{
    AgentMessage, AgentResources, Heartbeat, RegisterRequest,
};
use open_sandbox_contracts::types::AgentId;

use open_sandbox_controller::grpc::Controller;
use open_sandbox_controller::pg_store::PgStore;
use open_sandbox_controller::token::TokenValidator;

use open_sandbox_proxy::grpc::tunnel_service;
use open_sandbox_proxy::stream_mux::StreamMux;
use open_sandbox_proxy::tunnel_pool::TunnelPool;

const TEST_DB_BASE: &str = "postgres://postgres:test@localhost:5433";
const TEST_TOKEN: &str = "live-e2e-token";

struct TestTokenValidator;

impl TokenValidator for TestTokenValidator {
    fn validate(&self, token: &str) -> bool {
        token == TEST_TOKEN
    }
}

async fn create_test_db(name: &str) -> sqlx::PgPool {
    let admin_pool = sqlx::PgPool::connect(&format!("{TEST_DB_BASE}/postgres"))
        .await
        .unwrap();
    let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS {name}"))
        .execute(&admin_pool)
        .await;
    sqlx::query(&format!("CREATE DATABASE {name}"))
        .execute(&admin_pool)
        .await
        .unwrap();
    admin_pool.close().await;

    sqlx::PgPool::connect(&format!("{TEST_DB_BASE}/{name}"))
        .await
        .unwrap()
}

async fn start_live_controller(db_name: &str) -> (Controller<PgStore>, String) {
    let pool = create_test_db(db_name).await;

    let pg_store = Arc::new(PgStore::new(pool));
    pg_store.migrate().await.unwrap();

    let controller = Controller::new(pg_store, TestTokenValidator);

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

async fn start_live_proxy() -> String {
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
async fn agent_registers_with_live_controller_and_proxy() {
    let (_controller, controller_addr) = start_live_controller("cli_e2e_register").await;
    let proxy_addr = start_live_proxy().await;

    let agent_id = AgentId::new();

    let channel = tonic::transport::Channel::from_shared(controller_addr.clone())
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut client = ControllerServiceClient::new(channel);

    let (outbound_tx, outbound_rx) = mpsc::channel(32);
    let outbound = ReceiverStream::new(outbound_rx);
    let response = client.agent_stream(outbound).await.unwrap();
    let mut inbound = response.into_inner();

    let register_msg = AgentMessage {
        payload: Some(agent_message::Payload::Register(RegisterRequest {
            agent_id: agent_id.to_string(),
            join_token: TEST_TOKEN.to_string(),
            resources: Some(AgentResources {
                cpu_cores: 4,
                memory_bytes: 8_000_000_000,
                arch: 1,
                os: "linux".into(),
            }),
        })),
    };
    outbound_tx.send(register_msg).await.unwrap();

    let msg = inbound.message().await.unwrap().unwrap();
    match msg.payload.unwrap() {
        controller_command::Payload::RegisterResponse(resp) => {
            assert!(resp.accepted, "agent should be accepted with valid token");
        }
        other => panic!("expected RegisterResponse, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_heartbeat_acknowledged_after_registration() {
    let (_controller, controller_addr) = start_live_controller("cli_e2e_heartbeat").await;
    let agent_id = AgentId::new();

    let channel = tonic::transport::Channel::from_shared(controller_addr)
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut client = ControllerServiceClient::new(channel);

    let (outbound_tx, outbound_rx) = mpsc::channel(32);
    let outbound = ReceiverStream::new(outbound_rx);
    let response = client.agent_stream(outbound).await.unwrap();
    let mut inbound = response.into_inner();

    let register_msg = AgentMessage {
        payload: Some(agent_message::Payload::Register(RegisterRequest {
            agent_id: agent_id.to_string(),
            join_token: TEST_TOKEN.to_string(),
            resources: Some(AgentResources {
                cpu_cores: 4,
                memory_bytes: 8_000_000_000,
                arch: 1,
                os: "linux".into(),
            }),
        })),
    };
    outbound_tx.send(register_msg).await.unwrap();
    let _ = inbound.message().await.unwrap().unwrap();

    let heartbeat_msg = AgentMessage {
        payload: Some(agent_message::Payload::Heartbeat(Heartbeat {
            agent_id: agent_id.to_string(),
            timestamp: Some(prost_types::Timestamp::from(std::time::SystemTime::now())),
        })),
    };
    outbound_tx.send(heartbeat_msg).await.unwrap();

    let msg = inbound.message().await.unwrap().unwrap();
    match msg.payload.unwrap() {
        controller_command::Payload::HeartbeatAck(ack) => {
            assert!(ack.timestamp.is_some());
        }
        other => panic!("expected HeartbeatAck, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn cli_binary_version_and_help() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_open-sandbox"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("0.1.0"));

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_open-sandbox"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("controller"));
    assert!(stdout.contains("proxy"));
    assert!(stdout.contains("agent"));
}
