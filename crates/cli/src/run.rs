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
use open_sandbox_controller::auth::AdminAuthInterceptor;
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

    // F1: management gRPC requires CONTROLLER_ADMIN_TOKEN. Bind refuses to
    // start if the token is unset — fail closed rather than silently exposing
    // a no-auth management surface.
    let admin_auth = AdminAuthInterceptor::from_env().map_err(|e| {
        format!("CONTROLLER_ADMIN_TOKEN required for management gRPC: {e}")
    })?;

    let controller = Arc::new(Controller::new(pg_store, validator));
    let agent_service = controller.grpc_service();
    let mgmt_service = management_service(controller.clone(), admin_auth);

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
    // Comp-2 A3: proxy owns the routing_entries_subdomain_idx functional
    // index; the controller owns the table itself. Retry the index
    // creation since the proxy can race the controller's migrate().
    let migration_store = PgRoutingStore::new(pg_pool.clone());
    for attempt in 1..=PROXY_STARTUP_RETRY_ATTEMPTS {
        match migration_store.migrate().await {
            Ok(()) => {
                info!("proxy migrations applied");
                break;
            }
            Err(e) if attempt < PROXY_STARTUP_RETRY_ATTEMPTS => {
                warn!(
                    attempt,
                    max = PROXY_STARTUP_RETRY_ATTEMPTS,
                    error = %e,
                    "proxy migration not ready (controller may not have created routing_entries yet), retrying"
                );
                tokio::time::sleep(PROXY_STARTUP_RETRY_INTERVAL).await;
            }
            Err(e) => {
                return Err(format!(
                    "proxy migration failed after {PROXY_STARTUP_RETRY_ATTEMPTS} attempts: {e}"
                )
                .into());
            }
        }
    }
    // Hold a second store handle for the LISTEN subscriber below. The
    // PgListener needs its own dedicated connection (LISTEN occupies the
    // session) but borrows from the same pool.
    let listener_store = PgRoutingStore::new(pg_pool.clone());
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
    // Comp-2 C3: in Combined mode the network-isolation defense is gone, so
    // OpenIoStream auth depends entirely on the bearer token. Refuse to bind
    // if the operator hasn't set INTERNAL_TOKEN_ENV.
    if !split_listeners && internal_token.is_none() {
        return Err(format!(
            "{} must be set when running the proxy in Combined mode \
             (grpc_port == internal_grpc_port); without it OpenIoStream has \
             zero authentication and any caller on the listener can execute \
             commands in any sandbox",
            open_sandbox_contracts::constants::INTERNAL_TOKEN_ENV
        )
        .into());
    }
    // Comp-2 A1: shared-secret token agents present on OpenTunnel. Required on
    // the public listener so a network-reachable attacker cannot register
    // themselves as an arbitrary agent_id and hijack routing. Refuse to bind
    // when missing.
    let tunnel_token = std::env::var("TUNNEL_JOIN_TOKEN").ok();
    if tunnel_token.is_none() {
        return Err("TUNNEL_JOIN_TOKEN must be set so agents calling OpenTunnel \
                    are authenticated; without it any network-reachable caller \
                    can register as any agent_id and hijack routing for that \
                    agent's sandboxes"
            .into());
    }
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
        tunnel_token.clone(),
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

    // Comp-2 A6/B3/C1: LISTEN on routing_changed and invalidate cache
    // entries in real time. The 30s periodic refresh stays as a fallback
    // for when the LISTEN connection drops or notifications are lost.
    let cache_for_listener = cache.clone();
    tokio::spawn(async move {
        use open_sandbox_proxy::pg_store::RoutingChange;
        loop {
            let mut listener = match listener_store.routing_changed_listener().await {
                Ok(l) => l,
                Err(e) => {
                    warn!(error = %e, "routing_changed listener connect failed; will retry");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
            };
            info!("subscribed to routing_changed notifications");
            loop {
                match listener.recv().await {
                    Ok(notification) => {
                        let payload = notification.payload();
                        match RoutingChange::parse(payload) {
                            Some(RoutingChange::Insert {
                                sandbox_id,
                                agent_id,
                            }) => {
                                // Refresh the entry with the new (sandbox_id, agent_id)
                                // pairing — handles both freshly-inserted routes and
                                // agent reassignment.
                                cache_for_listener.insert(sandbox_id, agent_id);
                            }
                            Some(RoutingChange::Remove { sandbox_id }) => {
                                cache_for_listener.remove_by_sandbox_id(&sandbox_id);
                            }
                            None => {
                                warn!(payload = %payload, "unrecognized routing_changed payload");
                            }
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "routing_changed recv failed; reconnecting");
                        break;
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
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
            // Internal listener does NOT accept OpenTunnel; no token needed
            // there. Public listener already enforced tunnel_token above.
            None,
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
    // Comp-2 A1: present the bearer token on OpenTunnel. Optional so dev
    // setups (single-process tests) can omit; production proxies refuse
    // anonymous tunnels at the binary boundary.
    let agent_tunnel_token = std::env::var("TUNNEL_JOIN_TOKEN").ok();
    let proxy_conn = ProxyConnection::with_token(agent_id, forwarder, agent_tunnel_token);

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
