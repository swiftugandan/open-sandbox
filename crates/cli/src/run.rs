use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tracing::{info, warn};

use open_sandbox_contracts::constants::{
    PROXY_STARTUP_RETRY_ATTEMPTS, PROXY_STARTUP_RETRY_INTERVAL,
};
use open_sandbox_contracts::controller::{AgentResources, Architecture};
use open_sandbox_contracts::types::{AgentId, JoinToken};

use open_sandbox_agent::controller_client::ControllerConnection;
use open_sandbox_agent::proxy_client::ProxyConnection;
use open_sandbox_agent::sandbox::SandboxManager;
use open_sandbox_agent::tunnel::TunnelForwarder;

use open_sandbox_api::grpc_service::GrpcSandboxService;
use open_sandbox_api::proxy_client::{DEFAULT_POOL_SIZE, ProxyClientPool};
use open_sandbox_api::router::build_router;
use open_sandbox_api::state::ApiState;
use open_sandbox_controller::grpc::Controller;
use open_sandbox_controller::management::management_service;
use open_sandbox_controller::pg_store::PgStore;
use open_sandbox_controller::token::TokenValidator;

use open_sandbox_proxy::grpc::{ProxyRole, sandbox_io_service_with_role};
use open_sandbox_proxy::http_server::HttpServer;
use open_sandbox_proxy::io_sessions::IoSessions;
use open_sandbox_proxy::pg_store::PgRoutingStore;
use open_sandbox_proxy::router::Router;
use open_sandbox_proxy::routing_cache::RoutingCache;
use open_sandbox_proxy::stream_mux::StreamMux;
use open_sandbox_proxy::tunnel_pool::TunnelPool;

use crate::cli::{AgentArgs, ApiArgs, ControllerArgs, ProxyArgs};
use crate::http_client::ReqwestHttpClient;

#[cfg(feature = "docker")]
use open_sandbox_agent_docker::DockerRuntime;

struct StaticTokenValidator {
    expected: String,
}

impl TokenValidator for StaticTokenValidator {
    fn validate(&self, token: &str) -> bool {
        token == self.expected
    }
}

pub async fn run_controller(args: ControllerArgs) -> Result<(), Box<dyn std::error::Error>> {
    info!("controller starting");
    let pool = sqlx::PgPool::connect(&args.database_url).await?;
    let pg_store = Arc::new(PgStore::new(pool));
    pg_store.migrate().await?;
    info!("database migrations applied");

    let join_token = std::env::var("OPEN_SANDBOX_JOIN_TOKEN").map_err(|_| {
        "OPEN_SANDBOX_JOIN_TOKEN must be set for the controller to validate agent registrations"
    })?;
    let validator = StaticTokenValidator {
        expected: join_token,
    };

    let controller = Arc::new(Controller::new(pg_store, validator));
    let agent_service = controller.grpc_service();
    let mgmt_service = management_service(controller.clone());

    let addr = format!("0.0.0.0:{}", args.grpc_port);
    let listener = TcpListener::bind(&addr).await?;
    info!(addr = %addr, "controller ready");

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

    info!("controller shut down");
    Ok(())
}

pub async fn run_proxy(args: ProxyArgs) -> Result<(), Box<dyn std::error::Error>> {
    info!("proxy starting");
    let pg_pool = sqlx::PgPool::connect(&args.database_url).await?;
    let routing_store = PgRoutingStore::new(pg_pool);
    let cache = Arc::new(RoutingCache::new(routing_store));

    for attempt in 1..=PROXY_STARTUP_RETRY_ATTEMPTS {
        match cache.refresh().await {
            Ok(()) => {
                info!(routes = cache.len(), "routing cache loaded");
                break;
            }
            Err(e) if attempt < PROXY_STARTUP_RETRY_ATTEMPTS => {
                warn!(
                    attempt,
                    max = PROXY_STARTUP_RETRY_ATTEMPTS,
                    error = %e,
                    "routing cache not ready, retrying"
                );
                tokio::time::sleep(PROXY_STARTUP_RETRY_INTERVAL).await;
            }
            Err(e) => {
                return Err(format!(
                    "routing cache failed after {PROXY_STARTUP_RETRY_ATTEMPTS} attempts: {e}"
                )
                .into());
            }
        }
    }

    let tunnel_pool = Arc::new(TunnelPool::new());
    let mux = Arc::new(StreamMux::new(tunnel_pool.clone()));
    let sessions = Arc::new(IoSessions::new());
    let router = Arc::new(Router::new(cache.clone(), mux.clone()));

    // Optional internal authn token for OpenIoStream (gateway-side).
    // Layered on top of the network-isolation defense provided by
    // the separate internal listener.
    let internal_token = std::env::var(open_sandbox_contracts::constants::INTERNAL_TOKEN_ENV).ok();

    // Two-listener split: agents reach the public port for
    // OpenTunnel only; the api gateway reaches the internal port
    // for OpenIoStream only. Setting both to the same value
    // collapses to a single combined listener (development).
    let split_listeners = args.grpc_port != args.internal_grpc_port;
    let (public_role, internal_role) = if split_listeners {
        (ProxyRole::Public, ProxyRole::Internal)
    } else {
        info!(
            grpc_port = args.grpc_port,
            "proxy: combined listener (development mode); production should set \
             --internal-grpc-port to a separate, network-isolated port"
        );
        (ProxyRole::Combined, ProxyRole::Combined)
    };

    let grpc_addr = format!("0.0.0.0:{}", args.grpc_port);
    let grpc_listener = TcpListener::bind(&grpc_addr).await?;
    let public_service = sandbox_io_service_with_role(
        mux.clone(),
        tunnel_pool.clone(),
        sessions.clone(),
        cache.clone(),
        internal_token.clone(),
        public_role,
    );

    let http_addr = format!("0.0.0.0:{}", args.http_port);
    let http_listener = TcpListener::bind(&http_addr).await?;
    info!(grpc = %grpc_addr, http = %http_addr, "proxy ready");

    let http_server = HttpServer::new(router);

    let cache_refresh = cache.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            if let Err(e) = cache_refresh.refresh().await {
                warn!(error = %e, "routing cache refresh failed");
            }
        }
    });

    let public_grpc_handle = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(public_service)
            .serve_with_incoming_shutdown(TcpListenerStream::new(grpc_listener), shutdown_signal())
            .await
    });

    let internal_grpc_handle = if split_listeners {
        let internal_addr = format!("0.0.0.0:{}", args.internal_grpc_port);
        let internal_listener = TcpListener::bind(&internal_addr).await?;
        let internal_service = sandbox_io_service_with_role(
            mux,
            tunnel_pool,
            sessions,
            cache.clone(),
            internal_token,
            internal_role,
        );
        info!(internal_grpc = %internal_addr, "proxy: internal listener ready");
        Some(tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(internal_service)
                .serve_with_incoming_shutdown(
                    TcpListenerStream::new(internal_listener),
                    shutdown_signal(),
                )
                .await
        }))
    } else {
        None
    };

    let http_handle = tokio::spawn(async move { http_server.run(http_listener).await });

    if let Some(internal_handle) = internal_grpc_handle {
        tokio::select! {
            result = public_grpc_handle => { result??; }
            result = internal_handle => { result??; }
            result = http_handle => { result?.map_err(|e| e.to_string())?; }
            () = shutdown_signal() => {}
        }
    } else {
        tokio::select! {
            result = public_grpc_handle => { result??; }
            result = http_handle => { result?.map_err(|e| e.to_string())?; }
            () = shutdown_signal() => {}
        }
    }

    info!("proxy shut down");
    Ok(())
}

pub async fn run_agent(args: AgentArgs) -> Result<(), Box<dyn std::error::Error>> {
    let agent_id = AgentId::new();
    info!(agent_id = %agent_id, "agent starting");

    let join_token = JoinToken::new(args.token);

    #[cfg(feature = "youki")]
    let runtime_name = "youki";
    #[cfg(not(feature = "youki"))]
    let runtime_name = "docker";

    #[cfg(feature = "youki")]
    let runtime = Arc::new(open_sandbox_agent_youki::YoukiRuntime::new(
        open_sandbox_agent_youki::YoukiConfig::default(),
    )?);
    #[cfg(not(feature = "youki"))]
    let runtime = Arc::new(DockerRuntime::connect()?);
    let sandbox_manager = Arc::new(SandboxManager::new(runtime));

    let resources = AgentResources {
        cpu_cores: num_cpus() as u32,
        memory_bytes: total_memory_bytes(),
        arch: host_architecture() as i32,
        os: std::env::consts::OS.to_string(),
    };
    info!(
        runtime = runtime_name,
        cpu_cores = resources.cpu_cores,
        memory_mb = resources.memory_bytes / (1024 * 1024),
        "system resources collected"
    );

    let controller_conn = ControllerConnection::new(
        agent_id.clone(),
        join_token,
        sandbox_manager.clone(),
        resources,
    );

    let http_client = Arc::new(ReqwestHttpClient::new());
    let forwarder = Arc::new(TunnelForwarder::new(sandbox_manager.clone(), http_client));
    let proxy_conn = ProxyConnection::new(agent_id, forwarder);

    let controller_url = args.controller_url.clone();
    let proxy_url = args.proxy_url.clone();
    info!(
        controller = %controller_url,
        proxy = %proxy_url,
        "connecting to platform"
    );

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

    let sandboxes = sandbox_manager.list_sandboxes();
    if !sandboxes.is_empty() {
        warn!(count = sandboxes.len(), "stopping managed sandboxes");
        for entry in &sandboxes {
            let stop_cmd = open_sandbox_contracts::controller::StopSandbox {
                sandbox_id: entry.sandbox_id.to_string(),
                timeout_seconds: open_sandbox_contracts::constants::SANDBOX_STOP_TIMEOUT.as_secs()
                    as u32,
            };
            match sandbox_manager.stop_sandbox(stop_cmd).await {
                Ok(_) => info!(sandbox_id = %entry.sandbox_id, "stopped sandbox"),
                Err(e) => {
                    warn!(sandbox_id = %entry.sandbox_id, error = %e, "failed to stop sandbox")
                }
            }
        }
    }

    info!("agent shut down");
    Ok(())
}

pub async fn run_api(args: ApiArgs) -> Result<(), Box<dyn std::error::Error>> {
    info!(
        controller = %args.controller_url,
        proxy = %args.proxy_url,
        "api gateway starting"
    );

    let lifecycle = Arc::new(
        GrpcSandboxService::connect(&args.controller_url)
            .await
            .map_err(|e| format!("failed to connect to controller: {e}"))?,
    );

    let internal_token = std::env::var(open_sandbox_contracts::constants::INTERNAL_TOKEN_ENV).ok();

    let proxy = Arc::new(
        ProxyClientPool::connect(&args.proxy_url, DEFAULT_POOL_SIZE, internal_token)
            .await
            .map_err(|e| format!("failed to connect to proxy: {e}"))?,
    );

    let state = Arc::new(ApiState {
        lifecycle,
        proxy,
        api_key: args.api_key,
    });

    let router = build_router(state);
    let addr = format!("0.0.0.0:{}", args.port);
    let listener = TcpListener::bind(&addr).await?;
    info!(addr = %addr, "api gateway ready");

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    info!("api gateway shut down");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to register SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {}
            _ = sigterm.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        ctrl_c.await.ok();
    }
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
