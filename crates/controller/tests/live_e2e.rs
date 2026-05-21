use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::Streaming;

use open_sandbox_contracts::controller::{
    AgentMessage, AgentResources, ControllerCommand, Heartbeat, RegisterRequest, agent_message,
    controller_command, controller_service_client::ControllerServiceClient,
};
use open_sandbox_contracts::types::{AgentId, SandboxId};

use open_sandbox_controller::grpc::{Controller, CreateSandboxRequest};
use open_sandbox_controller::pg_store::PgStore;
use open_sandbox_controller::scheduler::SandboxRequirements;
use open_sandbox_controller::token::TokenValidator;

struct StaticToken(&'static str);
impl TokenValidator for StaticToken {
    fn validate(&self, token: &str) -> bool {
        token == self.0
    }
}

const TEST_TOKEN: &str = "live-e2e-test-token";

struct TestPg {
    container: String,
    pool: sqlx::PgPool,
}

impl TestPg {
    async fn start() -> Self {
        let container = format!(
            "osb-test-pg-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        );

        let output = std::process::Command::new("docker")
            .args([
                "run",
                "-d",
                "--name",
                &container,
                "-e",
                "POSTGRES_PASSWORD=test",
                "-e",
                "POSTGRES_DB=open_sandbox_test",
                "-p",
                "0:5432",
                "postgres:16-alpine",
            ])
            .output()
            .expect("docker not available");
        assert!(
            output.status.success(),
            "docker run failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let port = Self::get_host_port(&container);
        let url = format!(
            "postgres://postgres:test@127.0.0.1:{}/open_sandbox_test",
            port
        );

        let pool = Self::wait_for_ready(&url).await;
        Self { container, pool }
    }

    fn get_host_port(container: &str) -> u16 {
        let output = std::process::Command::new("docker")
            .args(["port", container, "5432"])
            .output()
            .expect("docker port failed");
        let stdout = String::from_utf8_lossy(&output.stdout);
        stdout
            .lines()
            .next()
            .expect("no port output")
            .rsplit(':')
            .next()
            .expect("no port in output")
            .trim()
            .parse()
            .expect("invalid port number")
    }

    async fn wait_for_ready(url: &str) -> sqlx::PgPool {
        for attempt in 0..30 {
            if let Ok(pool) = sqlx::PgPool::connect(url).await {
                if sqlx::query("SELECT 1").execute(&pool).await.is_ok() {
                    return pool;
                }
            }
            if attempt == 29 {
                panic!("postgres not ready after 15s");
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        unreachable!()
    }
}

impl Drop for TestPg {
    fn drop(&mut self) {
        let _ = std::process::Command::new("docker")
            .args(["rm", "-f", &self.container])
            .output();
    }
}

async fn start_controller_with_pg(
    pg: &TestPg,
    validator: impl TokenValidator + 'static,
) -> (Controller<PgStore>, String) {
    let pg_store = PgStore::new(pg.pool.clone());
    pg_store.migrate().await.expect("migration failed");

    let store = Arc::new(pg_store);
    let controller = Controller::new(store, validator);
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

async fn connect_agent(addr: &str) -> (mpsc::Sender<AgentMessage>, Streaming<ControllerCommand>) {
    let channel = tonic::transport::Channel::from_shared(addr.to_string())
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut client = ControllerServiceClient::new(channel);

    let (tx, rx) = mpsc::channel(32);
    let outbound = ReceiverStream::new(rx);
    let response = client.agent_stream(outbound).await.unwrap();
    (tx, response.into_inner())
}

fn register_message(agent_id: &AgentId, token: &str) -> AgentMessage {
    AgentMessage {
        payload: Some(agent_message::Payload::Register(RegisterRequest {
            agent_id: agent_id.to_string(),
            join_token: token.into(),
            resources: Some(AgentResources {
                cpu_cores: 4,
                memory_bytes: 8_000_000_000,
                arch: 1,
                os: "linux".into(),
            }),
        })),
    }
}

fn heartbeat_message(agent_id: &AgentId) -> AgentMessage {
    AgentMessage {
        payload: Some(agent_message::Payload::Heartbeat(Heartbeat {
            agent_id: agent_id.to_string(),
            timestamp: prost_types::Timestamp::try_from(std::time::SystemTime::now()).ok(),
        })),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn live_registration_and_heartbeat() {
    let pg = TestPg::start().await;
    let (_controller, addr) = start_controller_with_pg(&pg, StaticToken(TEST_TOKEN)).await;
    let (tx, mut inbound) = connect_agent(&addr).await;
    let agent_id = AgentId::new();

    tx.send(register_message(&agent_id, TEST_TOKEN))
        .await
        .unwrap();
    let msg = inbound.message().await.unwrap().unwrap();
    match msg.payload.unwrap() {
        controller_command::Payload::RegisterResponse(resp) => {
            assert!(resp.accepted, "registration should be accepted");
        }
        other => panic!("expected RegisterResponse, got {other:?}"),
    }

    tx.send(heartbeat_message(&agent_id)).await.unwrap();
    let msg = inbound.message().await.unwrap().unwrap();
    match msg.payload.unwrap() {
        controller_command::Payload::HeartbeatAck(ack) => {
            assert!(ack.timestamp.is_some());
        }
        other => panic!("expected HeartbeatAck, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn live_sandbox_creation() {
    let pg = TestPg::start().await;
    let (controller, addr) = start_controller_with_pg(&pg, StaticToken(TEST_TOKEN)).await;
    let (tx, mut inbound) = connect_agent(&addr).await;
    let agent_id = AgentId::new();

    tx.send(register_message(&agent_id, TEST_TOKEN))
        .await
        .unwrap();
    let _ = inbound.message().await.unwrap().unwrap();

    let sandbox_id = SandboxId::new();
    controller
        .create_sandbox(CreateSandboxRequest {
            sandbox_id: sandbox_id.clone(),
            image: "nginx:latest".into(),
            requirements: SandboxRequirements {
                cpu_millicores: 1000,
                memory_bytes: 512_000_000,
            },
        })
        .await
        .unwrap();

    let msg = inbound.message().await.unwrap().unwrap();
    match msg.payload.unwrap() {
        controller_command::Payload::StartSandbox(cmd) => {
            assert_eq!(cmd.sandbox_id, sandbox_id.to_string());
            assert_eq!(cmd.image, "nginx:latest");
        }
        other => panic!("expected StartSandbox, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn live_invalid_token_rejected() {
    let pg = TestPg::start().await;
    let (_controller, addr) = start_controller_with_pg(&pg, StaticToken(TEST_TOKEN)).await;
    let (tx, mut inbound) = connect_agent(&addr).await;

    tx.send(register_message(&AgentId::new(), "wrong-token"))
        .await
        .unwrap();
    let msg = inbound.message().await.unwrap().unwrap();
    match msg.payload.unwrap() {
        controller_command::Payload::RegisterResponse(resp) => {
            assert!(!resp.accepted);
            assert!(!resp.rejection_reason.is_empty());
        }
        other => panic!("expected RegisterResponse, got {other:?}"),
    }
}
