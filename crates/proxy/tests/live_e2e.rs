use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio_stream::wrappers::TcpListenerStream;

use open_sandbox_contracts::controller::AgentResources;
use open_sandbox_contracts::error::AgentError;
use open_sandbox_contracts::types::{AgentId, JoinToken, SandboxId};

use open_sandbox_agent::container::{
    ContainerConfig, ContainerId, ContainerInfo, ContainerRuntime,
};
use open_sandbox_agent::controller_client::ControllerConnection;
use open_sandbox_agent::proxy_client::ProxyConnection;
use open_sandbox_agent::sandbox::SandboxManager;
use open_sandbox_agent::tunnel::{ForwardRequest, ForwardResponse, HttpClient, TunnelForwarder};

use open_sandbox_controller::grpc::{Controller, CreateSandboxRequest};
use open_sandbox_controller::pg_store::PgStore;
use open_sandbox_controller::scheduler::SandboxRequirements;
use open_sandbox_controller::token::TokenValidator;

use open_sandbox_proxy::grpc::tunnel_service;
use open_sandbox_proxy::pg_store::PgRoutingStore;
use open_sandbox_proxy::routing_cache::RoutingCache;
use open_sandbox_proxy::router::Router;
use open_sandbox_proxy::stream_mux::StreamMux;
use open_sandbox_proxy::tunnel_pool::TunnelPool;

const TEST_TOKEN: &str = "live-proxy-test-token";

struct StaticToken(&'static str);
impl TokenValidator for StaticToken {
    fn validate(&self, token: &str) -> bool {
        token == self.0
    }
}

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

struct LiveMockHttpClient;

impl HttpClient for LiveMockHttpClient {
    async fn send(
        &self,
        _port: u16,
        _request: ForwardRequest,
    ) -> Result<ForwardResponse, AgentError> {
        Ok(ForwardResponse {
            status_code: 200,
            headers: Default::default(),
            body: b"live-response".to_vec(),
        })
    }
}

struct TestPg {
    container: String,
    pool: sqlx::PgPool,
}

impl TestPg {
    async fn start() -> Self {
        let container = format!(
            "osb-proxy-test-pg-{}",
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

async fn start_live_proxy(
    pg: &TestPg,
) -> (Arc<TunnelPool>, Arc<StreamMux>, Arc<RoutingCache<PgRoutingStore>>, String) {
    let routing_store = PgRoutingStore::new(pg.pool.clone());
    let cache = Arc::new(RoutingCache::new(routing_store));
    let pool = Arc::new(TunnelPool::new());
    let mux = Arc::new(StreamMux::new(pool.clone()));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = format!("http://{}", listener.local_addr().unwrap());

    let service = tunnel_service(mux.clone(), pool.clone());
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(service)
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .unwrap();
    });

    (pool, mux, cache, addr)
}

fn test_resources() -> AgentResources {
    AgentResources {
        cpu_cores: 4,
        memory_bytes: 8_000_000_000,
        arch: 1,
        os: "linux".into(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn live_full_request_flow_through_real_proxy() {
    let pg = TestPg::start().await;
    let (controller, controller_addr) = start_live_controller(&pg).await;
    let (pool, mux, cache, proxy_addr) = start_live_proxy(&pg).await;

    let runtime = Arc::new(LiveMockRuntime::new());
    let manager = Arc::new(SandboxManager::new(runtime.clone()));
    let agent_id = AgentId::new();

    let controller_conn = ControllerConnection::new(
        agent_id.clone(),
        JoinToken::new(TEST_TOKEN.into()),
        manager.clone(),
        test_resources(),
    );
    let ctrl_handle = tokio::spawn({
        let addr = controller_addr.clone();
        async move { controller_conn.run(&addr).await }
    });

    tokio::time::sleep(Duration::from_millis(200)).await;

    let http_client = Arc::new(LiveMockHttpClient);
    let forwarder = Arc::new(TunnelForwarder::new(manager.clone(), http_client));
    let proxy_conn = ProxyConnection::new(agent_id.clone(), forwarder);
    let tunnel_handle = tokio::spawn({
        let addr = proxy_addr.clone();
        async move { proxy_conn.run(&addr).await }
    });

    tokio::time::sleep(Duration::from_millis(200)).await;

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
        .expect("sandbox creation should succeed");

    tokio::time::sleep(Duration::from_millis(300)).await;

    cache.refresh().await.expect("cache refresh should succeed");

    assert!(
        pool.contains(&agent_id),
        "agent should be registered in tunnel pool"
    );

    let router = Router::new(cache, mux.clone());
    let host = format!("{}.sandbox.example.com", sandbox_id.subdomain());

    let result = router
        .route_request(&host, "GET".into(), "/live-test".into(), Default::default(), vec![])
        .await;

    let response = result.expect("routing should succeed");
    assert_eq!(response.status_code, 200);
    assert_eq!(response.body, b"live-response");

    ctrl_handle.abort();
    tunnel_handle.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn live_routing_cache_refreshes_from_postgres() {
    let pg = TestPg::start().await;
    let (controller, controller_addr) = start_live_controller(&pg).await;
    let (_, _, cache, _) = start_live_proxy(&pg).await;

    let runtime = Arc::new(LiveMockRuntime::new());
    let manager = Arc::new(SandboxManager::new(runtime));

    let controller_conn = ControllerConnection::new(
        AgentId::new(),
        JoinToken::new(TEST_TOKEN.into()),
        manager,
        test_resources(),
    );
    let ctrl_handle = tokio::spawn({
        let addr = controller_addr.clone();
        async move { controller_conn.run(&addr).await }
    });

    tokio::time::sleep(Duration::from_millis(200)).await;

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
        .expect("sandbox creation should succeed");

    cache.refresh().await.expect("cache refresh should work");

    let subdomain = sandbox_id.subdomain();
    assert!(
        cache.lookup(&subdomain).is_some(),
        "sandbox should appear in routing cache after refresh"
    );

    ctrl_handle.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn live_routing_miss_for_unknown_sandbox() {
    let pg = TestPg::start().await;
    let (_controller, _controller_addr) = start_live_controller(&pg).await;
    let (_, mux, cache, _) = start_live_proxy(&pg).await;

    cache.refresh().await.expect("cache refresh should work");

    let router = Router::new(cache, mux);
    let result = router
        .route_request(
            "aabbccddeeff.sandbox.example.com",
            "GET".into(),
            "/".into(),
            Default::default(),
            vec![],
        )
        .await;

    assert!(
        matches!(
            result,
            Err(open_sandbox_contracts::error::ProxyError::RoutingMiss { .. })
        ),
        "unknown sandbox should get routing miss"
    );
}
