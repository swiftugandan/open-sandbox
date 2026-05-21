use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;

use open_sandbox_contracts::controller::AgentResources;
use open_sandbox_contracts::types::{AgentId, JoinToken};

use open_sandbox_agent::container::{ContainerConfig, ContainerId, ContainerInfo, ContainerRuntime};
use open_sandbox_agent::controller_client::ControllerConnection;
use open_sandbox_agent::proxy_client::ProxyConnection;
use open_sandbox_agent::sandbox::SandboxManager;
use open_sandbox_agent::tunnel::{ForwardRequest, ForwardResponse, HttpClient, TunnelForwarder};

use open_sandbox_controller::grpc::Controller;
use open_sandbox_controller::pg_store::PgStore;
use open_sandbox_controller::token::TokenValidator;

use open_sandbox_proxy::grpc::tunnel_service;
use open_sandbox_proxy::pg_store::PgRoutingStore;
use open_sandbox_proxy::routing_cache::RoutingCache;
use open_sandbox_proxy::stream_mux::StreamMux;
use open_sandbox_proxy::tunnel_pool::TunnelPool;

use crate::cli::{AgentArgs, ControllerArgs, ProxyArgs};

struct StaticTokenValidator {
    expected: String,
}

impl TokenValidator for StaticTokenValidator {
    fn validate(&self, token: &str) -> bool {
        token == self.expected
    }
}

pub async fn run_controller(args: ControllerArgs) -> Result<(), Box<dyn std::error::Error>> {
    let pool = sqlx::PgPool::connect(&args.database_url).await?;
    let pg_store = Arc::new(PgStore::new(pool));
    pg_store.migrate().await?;

    let join_token =
        std::env::var("OPEN_SANDBOX_JOIN_TOKEN").unwrap_or_else(|_| "changeme".to_string());
    let validator = StaticTokenValidator {
        expected: join_token,
    };

    let controller = Controller::new(pg_store, validator);
    let service = controller.grpc_service();

    let addr = format!("0.0.0.0:{}", args.grpc_port);
    let listener = TcpListener::bind(&addr).await?;

    let sweep_interval = Duration::from_secs(args.sweep_interval);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(sweep_interval).await;
            controller.sweep_dead_agents().await;
        }
    });

    tonic::transport::Server::builder()
        .add_service(service)
        .serve_with_incoming_shutdown(TcpListenerStream::new(listener), shutdown_signal())
        .await?;

    Ok(())
}

pub async fn run_proxy(args: ProxyArgs) -> Result<(), Box<dyn std::error::Error>> {
    let pool = sqlx::PgPool::connect(&args.database_url).await?;
    let pg_routing = PgRoutingStore::new(pool);

    let tunnel_pool = Arc::new(TunnelPool::new());
    let cache = Arc::new(RoutingCache::new(pg_routing));
    cache.refresh().await?;

    let mux = Arc::new(StreamMux::new(tunnel_pool.clone()));
    let grpc_service = tunnel_service(mux, tunnel_pool);

    let grpc_addr = format!("0.0.0.0:{}", args.grpc_port);
    let grpc_listener = TcpListener::bind(&grpc_addr).await?;

    tonic::transport::Server::builder()
        .add_service(grpc_service)
        .serve_with_incoming_shutdown(
            TcpListenerStream::new(grpc_listener),
            shutdown_signal(),
        )
        .await?;

    Ok(())
}

struct DockerRuntime;

impl ContainerRuntime for DockerRuntime {
    async fn create_and_start(
        &self,
        _config: ContainerConfig,
    ) -> Result<ContainerInfo, open_sandbox_contracts::error::AgentError> {
        Err(open_sandbox_contracts::error::AgentError::Docker {
            detail: "Docker runtime not yet implemented in CLI".to_string(),
        })
    }

    async fn stop_and_remove(
        &self,
        _id: &ContainerId,
        _timeout: Duration,
    ) -> Result<(), open_sandbox_contracts::error::AgentError> {
        Err(open_sandbox_contracts::error::AgentError::Docker {
            detail: "Docker runtime not yet implemented in CLI".to_string(),
        })
    }

    async fn list_sandbox_containers(
        &self,
    ) -> Result<Vec<ContainerInfo>, open_sandbox_contracts::error::AgentError> {
        Ok(vec![])
    }
}

struct LocalHttpClient;

impl HttpClient for LocalHttpClient {
    async fn send(
        &self,
        _port: u16,
        _request: ForwardRequest,
    ) -> Result<ForwardResponse, open_sandbox_contracts::error::AgentError> {
        Err(open_sandbox_contracts::error::AgentError::Internal {
            detail: "HTTP client not yet implemented in CLI".to_string(),
        })
    }
}

pub async fn run_agent(args: AgentArgs) -> Result<(), Box<dyn std::error::Error>> {
    let agent_id = AgentId::new();
    let join_token = JoinToken::new(args.token);

    let runtime = Arc::new(DockerRuntime);
    let sandbox_manager = Arc::new(SandboxManager::new(runtime.clone()));

    let resources = AgentResources {
        cpu_cores: 4,
        memory_bytes: 8_000_000_000,
        arch: 1,
        os: std::env::consts::OS.to_string(),
    };

    let controller_conn =
        ControllerConnection::new(agent_id.clone(), join_token, sandbox_manager.clone(), resources);

    let http_client = Arc::new(LocalHttpClient);
    let forwarder = Arc::new(TunnelForwarder::new(sandbox_manager, http_client));
    let proxy_conn = ProxyConnection::new(agent_id, forwarder);

    let controller_url = args.controller_url.clone();
    let proxy_url = args.proxy_url.clone();

    let ctrl_handle = tokio::spawn(async move { controller_conn.run(&controller_url).await });
    let proxy_handle = tokio::spawn(async move { proxy_conn.run(&proxy_url).await });

    tokio::select! {
        result = ctrl_handle => {
            result??;
        }
        result = proxy_handle => {
            result??;
        }
        () = shutdown_signal() => {}
    }

    Ok(())
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("failed to install CTRL+C signal handler");
}
