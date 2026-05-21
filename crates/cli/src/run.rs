use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;

use open_sandbox_contracts::controller::{AgentResources, Architecture};
use open_sandbox_contracts::types::{AgentId, JoinToken};

use open_sandbox_agent::controller_client::ControllerConnection;
use open_sandbox_agent::proxy_client::ProxyConnection;
use open_sandbox_agent::sandbox::SandboxManager;
use open_sandbox_agent::tunnel::TunnelForwarder;

use open_sandbox_api::grpc_service::GrpcSandboxService;
use open_sandbox_api::router::build_router;
use open_sandbox_controller::grpc::Controller;
use open_sandbox_controller::management::management_service;
use open_sandbox_controller::pg_store::PgStore;
use open_sandbox_controller::token::TokenValidator;

use open_sandbox_proxy::grpc::tunnel_service;
use open_sandbox_proxy::http_server::HttpServer;
use open_sandbox_proxy::pg_store::PgRoutingStore;
use open_sandbox_proxy::router::Router;
use open_sandbox_proxy::routing_cache::RoutingCache;
use open_sandbox_proxy::stream_mux::StreamMux;
use open_sandbox_proxy::tunnel_pool::TunnelPool;

use crate::cli::{AgentArgs, ApiArgs, ControllerArgs, ProxyArgs};
use crate::docker_runtime::DockerRuntime;
use crate::http_client::ReqwestHttpClient;

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

    let join_token = std::env::var("OPEN_SANDBOX_JOIN_TOKEN").map_err(|_| {
        "OPEN_SANDBOX_JOIN_TOKEN must be set for the controller to validate agent registrations"
    })?;
    let validator = StaticTokenValidator {
        expected: join_token,
    };

    let controller = Arc::new(Controller::new(pg_store, validator));
    let agent_service = controller.grpc_service();
    let mgmt_service = management_service(controller.clone(), controller.exec_broker());

    let addr = format!("0.0.0.0:{}", args.grpc_port);
    let listener = TcpListener::bind(&addr).await?;

    let sweep_controller = controller.clone();
    let sweep_interval = Duration::from_secs(args.sweep_interval);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(sweep_interval).await;
            sweep_controller.sweep_dead_agents().await;
        }
    });

    tonic::transport::Server::builder()
        .add_service(agent_service)
        .add_service(mgmt_service)
        .serve_with_incoming_shutdown(TcpListenerStream::new(listener), shutdown_signal())
        .await?;

    Ok(())
}

pub async fn run_proxy(args: ProxyArgs) -> Result<(), Box<dyn std::error::Error>> {
    let pg_pool = sqlx::PgPool::connect(&args.database_url).await?;
    let routing_store = PgRoutingStore::new(pg_pool);
    let cache = Arc::new(RoutingCache::new(routing_store));

    cache.refresh().await.map_err(|e| e.to_string())?;

    let tunnel_pool = Arc::new(TunnelPool::new());
    let mux = Arc::new(StreamMux::new(tunnel_pool.clone()));
    let router = Arc::new(Router::new(cache.clone(), mux.clone()));

    let grpc_service = tunnel_service(mux, tunnel_pool);

    let grpc_addr = format!("0.0.0.0:{}", args.grpc_port);
    let grpc_listener = TcpListener::bind(&grpc_addr).await?;

    let http_addr = format!("0.0.0.0:{}", args.http_port);
    let http_listener = TcpListener::bind(&http_addr).await?;

    let http_server = HttpServer::new(router);

    let cache_refresh = cache.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            let _ = cache_refresh.refresh().await;
        }
    });

    let grpc_handle = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(grpc_service)
            .serve_with_incoming_shutdown(TcpListenerStream::new(grpc_listener), shutdown_signal())
            .await
    });

    let http_handle = tokio::spawn(async move { http_server.run(http_listener).await });

    tokio::select! {
        result = grpc_handle => { result??; }
        result = http_handle => { result?.map_err(|e| e.to_string())?; }
        () = shutdown_signal() => {}
    }

    Ok(())
}

pub async fn run_agent(args: AgentArgs) -> Result<(), Box<dyn std::error::Error>> {
    let agent_id = AgentId::new();
    let join_token = JoinToken::new(args.token);

    let runtime = Arc::new(DockerRuntime::connect()?);
    let sandbox_manager = Arc::new(SandboxManager::new(runtime));

    let resources = AgentResources {
        cpu_cores: num_cpus() as u32,
        memory_bytes: total_memory_bytes(),
        arch: host_architecture() as i32,
        os: std::env::consts::OS.to_string(),
    };

    let controller_conn = ControllerConnection::new(
        agent_id.clone(),
        join_token,
        sandbox_manager.clone(),
        resources,
    );

    let http_client = Arc::new(ReqwestHttpClient::new());
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

pub async fn run_api(args: ApiArgs) -> Result<(), Box<dyn std::error::Error>> {
    let service = GrpcSandboxService::connect(&args.controller_url)
        .await
        .map_err(|e| format!("failed to connect to controller: {e}"))?;

    let router = build_router(Arc::new(service));
    let addr = format!("0.0.0.0:{}", args.port);
    let listener = TcpListener::bind(&addr).await?;

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

fn total_memory_bytes() -> u64 {
    // Conservative default; real detection requires platform-specific APIs
    // or a crate like sysinfo. 4 GiB is a safe lower bound.
    4 * 1024 * 1024 * 1024
}

fn host_architecture() -> Architecture {
    match std::env::consts::ARCH {
        "x86_64" => Architecture::X8664,
        "aarch64" => Architecture::Aarch64,
        _ => Architecture::Unspecified,
    }
}
