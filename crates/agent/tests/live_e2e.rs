use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
use std::time::Duration;

use tokio_stream::wrappers::TcpListenerStream;

use open_sandbox_contracts::controller::AgentResources;
use open_sandbox_contracts::error::AgentError;
use open_sandbox_contracts::types::{AgentId, JoinToken, SandboxId};

use open_sandbox_agent::container::{
    ContainerConfig, ContainerId, ContainerInfo, ContainerRuntime,
};
use open_sandbox_agent::controller_client::ControllerConnection;
use open_sandbox_agent::sandbox::SandboxManager;

use open_sandbox_controller::grpc::{Controller, CreateSandboxRequest};
use open_sandbox_controller::pg_store::PgStore;
use open_sandbox_controller::scheduler::SandboxRequirements;
use open_sandbox_controller::token::TokenValidator;

const TEST_TOKEN: &str = "live-agent-test-token";

struct StaticToken(&'static str);
impl TokenValidator for StaticToken {
    fn validate(&self, token: &str) -> bool {
        token == self.0
    }
}

// --- Mock Container Runtime ---

struct LiveMockRuntime {
    created: AtomicUsize,
}

impl LiveMockRuntime {
    fn new() -> Self {
        Self {
            created: AtomicUsize::new(0),
        }
    }
}

impl ContainerRuntime for LiveMockRuntime {
    async fn create_and_start(&self, config: ContainerConfig) -> Result<ContainerInfo, AgentError> {
        self.created.fetch_add(1, Ordering::SeqCst);
        Ok(ContainerInfo {
            id: ContainerId(format!("live-mock-{}", config.sandbox_id)),
            sandbox_id: config.sandbox_id,
            host_port: 9000,
            running: true,
        })
    }

    async fn stop_and_remove(
        &self,
        _id: &ContainerId,
        _timeout: Duration,
    ) -> Result<(), AgentError> {
        Ok(())
    }

    async fn list_sandbox_containers(&self) -> Result<Vec<ContainerInfo>, AgentError> {
        Ok(Vec::new())
    }
}

// --- Test Postgres ---

struct TestPg {
    container: String,
    pool: sqlx::PgPool,
}

impl TestPg {
    async fn start() -> Self {
        let container = format!(
            "osb-agent-test-pg-{}",
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
            if let Ok(pool) = sqlx::PgPool::connect(url).await
                && sqlx::query("SELECT 1").execute(&pool).await.is_ok()
            {
                return pool;
            }
            if attempt == 29 {
                panic!("postgres not ready after 15s");
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
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

// --- Helpers ---

async fn start_live_controller(pg: &TestPg) -> (Controller<PgStore>, String) {
    let pg_store = PgStore::new(pg.pool.clone());
    pg_store.migrate().await.expect("migration failed");

    let store = Arc::new(pg_store);
    let controller = Controller::new(store, StaticToken(TEST_TOKEN));
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

fn test_resources() -> AgentResources {
    AgentResources {
        cpu_cores: 4,
        memory_bytes: 8_000_000_000,
        arch: 1,
        os: "linux".into(),
    }
}

// --- Tests ---

#[tokio::test(flavor = "multi_thread")]
async fn live_agent_registers_with_real_controller() {
    let pg = TestPg::start().await;
    let (_controller, addr) = start_live_controller(&pg).await;

    let runtime = Arc::new(LiveMockRuntime::new());
    let manager = Arc::new(SandboxManager::new(runtime));
    let agent_id = AgentId::new();

    let conn = ControllerConnection::new(
        agent_id.clone(),
        JoinToken::new(TEST_TOKEN.into()),
        manager,
        test_resources(),
    );

    let handle = tokio::spawn(async move { conn.run(&addr).await });

    tokio::time::sleep(Duration::from_millis(200)).await;
    handle.abort();

    // Verify agent is registered by querying the PgStore directly
    let row: Option<(String,)> =
        sqlx::query_as("SELECT agent_id::text FROM agents WHERE agent_id = $1")
            .bind(agent_id.0)
            .fetch_optional(&pg.pool)
            .await
            .unwrap();

    assert!(row.is_some(), "agent should be persisted in Postgres");
}

#[tokio::test(flavor = "multi_thread")]
async fn live_agent_processes_start_sandbox_from_real_controller() {
    let pg = TestPg::start().await;
    let (controller, addr) = start_live_controller(&pg).await;

    let runtime = Arc::new(LiveMockRuntime::new());
    let manager = Arc::new(SandboxManager::new(runtime.clone()));

    let conn = ControllerConnection::new(
        AgentId::new(),
        JoinToken::new(TEST_TOKEN.into()),
        manager,
        test_resources(),
    );

    let handle = tokio::spawn(async move { conn.run(&addr).await });

    // Wait for registration
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Controller creates a sandbox — this sends StartSandbox to the agent
    let sandbox_id = SandboxId::new();
    let result = controller
        .create_sandbox(CreateSandboxRequest {
            sandbox_id: sandbox_id.clone(),
            image: "nginx:latest".into(),
            requirements: SandboxRequirements {
                cpu_millicores: 1000,
                memory_bytes: 512_000_000,
            },
        })
        .await;
    assert!(result.is_ok(), "sandbox creation should succeed");

    // Wait for agent to process the command
    tokio::time::sleep(Duration::from_millis(300)).await;
    handle.abort();

    assert_eq!(
        runtime.created.load(Ordering::SeqCst),
        1,
        "agent should have created one container"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn live_agent_rejected_with_bad_token() {
    let pg = TestPg::start().await;
    let (_controller, addr) = start_live_controller(&pg).await;

    let runtime = Arc::new(LiveMockRuntime::new());
    let manager = Arc::new(SandboxManager::new(runtime));

    let conn = ControllerConnection::new(
        AgentId::new(),
        JoinToken::new("wrong-token".into()),
        manager,
        test_resources(),
    );

    let result = conn.run(&addr).await;
    assert!(
        matches!(result, Err(AgentError::Internal { .. })),
        "should be rejected"
    );
}
