use std::sync::Arc;
use std::time::Duration;

use tokio_stream::wrappers::TcpListenerStream;

use open_sandbox_agent::controller_client::ControllerConnection;
use open_sandbox_agent::sandbox::SandboxManager;
use open_sandbox_agent_youki::{YoukiConfig, YoukiRuntime};
use open_sandbox_contracts::controller::AgentResources;
use open_sandbox_contracts::types::{AgentId, JoinToken, SandboxId};

use open_sandbox_controller::grpc::{Controller, CreateSandboxRequest};
use open_sandbox_controller::pg_store::PgStore;
use open_sandbox_controller::scheduler::SandboxRequirements;
use open_sandbox_controller::token::TokenValidator;

const TEST_TOKEN: &str = "youki-live-e2e-token";

struct StaticToken(&'static str);
impl TokenValidator for StaticToken {
    fn validate(&self, token: &str) -> bool {
        token == self.0
    }
}

fn youki_config() -> YoukiConfig {
    YoukiConfig {
        root_dir: std::path::PathBuf::from("/tmp/youki-live-e2e"),
        cni_bin_path: std::path::PathBuf::from("/opt/cni/bin"),
    }
}

fn test_resources() -> AgentResources {
    AgentResources {
        cpu_cores: 4,
        memory_bytes: 8_000_000_000,
        arch: 1,
        os: "linux".into(),
    }
}

fn database_url() -> String {
    std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:test@127.0.0.1:5432/open_sandbox_test".into())
}

async fn start_live_controller(pool: &sqlx::PgPool) -> (Controller<PgStore>, String) {
    let pg_store = PgStore::new(pool.clone());
    pg_store.migrate().await.expect("migration failed");

    let store = Arc::new(pg_store);
    let controller = Controller::new(store, StaticToken(TEST_TOKEN));
    let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
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

#[tokio::test(flavor = "multi_thread")]
async fn live_youki_agent_creates_real_container_via_controller() {
    let pool = sqlx::PgPool::connect(&database_url())
        .await
        .expect("cannot connect to Postgres — set DATABASE_URL or run via docker-compose");

    let (controller, addr) = start_live_controller(&pool).await;

    let runtime = Arc::new(YoukiRuntime::new(youki_config()).unwrap());
    let manager = Arc::new(SandboxManager::new(runtime));

    let conn = ControllerConnection::new(
        AgentId::new(),
        JoinToken::new(TEST_TOKEN.into()),
        manager,
        test_resources(),
    );

    let handle = tokio::spawn(async move { conn.run(&addr).await });

    tokio::time::sleep(Duration::from_millis(500)).await;

    let sandbox_id = SandboxId::new();
    let result = controller
        .create_sandbox(CreateSandboxRequest {
            sandbox_id: sandbox_id.clone(),
            image: "alpine:latest".into(),
            requirements: SandboxRequirements {
                cpu_millicores: 1000,
                memory_bytes: 512_000_000,
            },
            env_vars: std::collections::HashMap::new(),
            exposed_port: 8080,
        })
        .await;
    assert!(result.is_ok(), "sandbox creation should succeed");

    // Wait for agent to pull image + create container
    tokio::time::sleep(Duration::from_secs(15)).await;
    handle.abort();

    let row: Option<(uuid::Uuid,)> =
        sqlx::query_as("SELECT sandbox_id FROM routing_entries WHERE sandbox_id = $1")
            .bind(sandbox_id.0)
            .fetch_optional(&pool)
            .await
            .unwrap();

    assert!(
        row.is_some(),
        "routing entry should be persisted in Postgres after sandbox creation"
    );
}
