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

use crate::cli::{AgentArgs, ApiArgs, ControllerArgs, MigrateArgs, ProxyArgs};
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
    let pool = sqlx::PgPool::connect(args.database_url.expose()).await?;
    let pg_store = Arc::new(PgStore::new(pool));
    if args.auto_migrate {
        pg_store.migrate().await?;
        info!("controller migrations applied (auto-migrate)");
    } else {
        info!(
            "skipping controller migrations (auto-migrate off); \
             run `open-sandbox migrate` separately"
        );
    }

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
    let pg_pool = sqlx::PgPool::connect(args.database_url.expose()).await?;
    if args.auto_migrate {
        // Comp-2 A3: proxy owns the routing_entries_subdomain_idx functional
        // index; the controller owns the table itself. Retry the index
        // creation since the proxy can race the controller's migrate().
        apply_proxy_migrations_with_retry(&pg_pool).await?;
    } else {
        info!(
            "skipping proxy migrations (auto-migrate off); \
             run `open-sandbox migrate` separately"
        );
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

    // PLAN_12FACTOR.md Phase 4 drain flag, shared by public + internal
    // gRPC handlers. Flipped by the supervisor task below when SIGTERM
    // arrives, causing both `OpenTunnel` and `OpenIoStream` to reject
    // new calls with `Unavailable` while in-flight sessions finish.
    let drain_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));

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
        drain_flag.clone(),
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

    // PLAN_12FACTOR.md Phase 4: drain coordinator.
    //
    // One `tokio::sync::watch` channel signals "drain is complete; tonic
    // may begin its own graceful shutdown" to every listener task. The
    // supervisor:
    //   1. Awaits SIGTERM via shutdown_signal().
    //   2. Flips the shared drain flag — new OpenTunnel / OpenIoStream
    //      calls now return Unavailable.
    //   3. Polls IoSessions::is_empty() every 100ms until drained OR
    //      `--shutdown-drain-timeout` (default 30s) elapses.
    //   4. On timeout: sends each remaining session a terminal
    //      Unavailable frame via IoSessions::fail_all_remaining so the
    //      gateway never sees a silent disconnect.
    //   5. Sends `true` on the watch channel.
    // Each listener awaits the watch channel and passes that future to
    // tonic's `serve_with_incoming_shutdown`.
    //
    // INTENTIONAL: this only drains IoSessions (gateway-facing
    // bidirectional streams). The TunnelPool (agent OpenTunnel response
    // streams) is NOT given a terminal frame on shutdown — agents
    // detect the abrupt close and enter their existing exponential-
    // backoff reconnect loop (see crates/agent/src/.../reconnect logic;
    // run.rs:434-446 wraps the OpenTunnel call). Adding tunnel-side
    // terminal frames would require a new TunnelRequest payload variant
    // (a Phase 4+ contract change) and provides no observable benefit
    // over reconnect-on-RST. See docs/design/SCALING_TIERS.md for the
    // partner ADR ("agent reconnect is the primary recovery mechanism;
    // proxy is a connection-affinity tier, not a stateful peer").
    // Code-review finding #10.
    let (drain_complete_tx, drain_complete_rx) = tokio::sync::watch::channel(false);
    let drain_timeout = Duration::from_secs(args.shutdown_drain_timeout);
    {
        let drain_flag = drain_flag.clone();
        let sessions_for_drain = sessions.clone();
        tokio::spawn(async move {
            shutdown_signal().await;
            info!("proxy: SIGTERM received, beginning drain");
            drain_flag.store(true, std::sync::atomic::Ordering::Release);
            let start = std::time::Instant::now();
            // Code-review finding #6: race the drain loop against a
            // SECOND shutdown_signal so a second SIGTERM/Ctrl-C
            // escalates to immediate-fail instead of being silently
            // coalesced for up to `drain_timeout` seconds. Operators
            // who hit Ctrl-C twice expecting fast exit now get it.
            let drain_loop = async {
                while !sessions_for_drain.is_empty() && start.elapsed() < drain_timeout {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            };
            tokio::select! {
                () = drain_loop => {
                    let remaining = sessions_for_drain.len();
                    if remaining == 0 {
                        info!(
                            elapsed_ms = start.elapsed().as_millis() as u64,
                            "proxy: drain complete"
                        );
                    } else {
                        warn!(
                            remaining,
                            timeout_secs = drain_timeout.as_secs(),
                            elapsed_ms = start.elapsed().as_millis() as u64,
                            "proxy: drain timeout; failing remaining sessions with Unavailable"
                        );
                        sessions_for_drain.fail_all_remaining(tonic::Status::unavailable(
                            "proxy shutting down (drain timeout)",
                        ));
                    }
                }
                () = shutdown_signal() => {
                    let remaining = sessions_for_drain.len();
                    warn!(
                        remaining,
                        elapsed_ms = start.elapsed().as_millis() as u64,
                        "proxy: second SIGTERM received during drain; failing remaining sessions immediately"
                    );
                    sessions_for_drain.fail_all_remaining(tonic::Status::unavailable(
                        "proxy shutting down (escalated)",
                    ));
                }
            }
            // Even if no listener has subscribed yet (the spawn race),
            // watch::Sender holds the value so later subscribers observe
            // it via `borrow_and_update()` on first poll. Unlike Notify,
            // there is no "wakeup happens before await" hazard here.
            let _ = drain_complete_tx.send(true);
        });
    }

    // Comp-2 C5 / comp-9: optional in-binary ACME for the public listener.
    // Operator opts in via TUNNEL_ACME_DOMAIN + ACME_EMAIL; otherwise the
    // listener serves plaintext h2c (development mode).
    let acme_settings = crate::tls::AcmeSettings::from_env();
    let public_drain_rx = drain_complete_rx.clone();
    let public_grpc_handle = tokio::spawn(async move {
        // Comp-2 B4: HTTP/2 keepalive pings detect a frozen-but-TCP-alive
        // agent within the documented spike-03 budget.
        let builder = tonic::transport::Server::builder()
            .http2_keepalive_interval(Some(Duration::from_secs(15)))
            .http2_keepalive_timeout(Some(Duration::from_secs(20)))
            .add_service(public_service);
        match acme_settings {
            Some(settings) => {
                info!("public listener: ACME-managed TLS");
                let incoming = crate::tls::acme_incoming(grpc_listener, settings);
                builder
                    .serve_with_incoming_shutdown(incoming, wait_for_drain(public_drain_rx))
                    .await
            }
            None => {
                warn!(
                    "public listener: PLAINTEXT (set TUNNEL_ACME_DOMAIN + ACME_EMAIL for production TLS)"
                );
                builder
                    .serve_with_incoming_shutdown(
                        TcpListenerStream::new(grpc_listener),
                        wait_for_drain(public_drain_rx),
                    )
                    .await
            }
        }
    });

    let internal_grpc_handle = if split_listeners {
        let internal_addr = format!("0.0.0.0:{}", args.internal_grpc_port);
        let internal_listener = TcpListener::bind(&internal_addr).await?;
        let internal_service = sandbox_io_service_with_role(
            mux,
            tunnel_pool,
            sessions.clone(),
            cache.clone(),
            internal_token,
            // Internal listener does NOT accept OpenTunnel; no token needed
            // there. Public listener already enforced tunnel_token above.
            None,
            internal_role,
            drain_flag.clone(),
        );
        info!(internal_grpc = %internal_addr, "proxy: internal listener ready");
        let internal_drain_rx = drain_complete_rx.clone();
        Some(tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(internal_service)
                .serve_with_incoming_shutdown(
                    TcpListenerStream::new(internal_listener),
                    wait_for_drain(internal_drain_rx),
                )
                .await
        }))
    } else {
        None
    };

    // Code-review finding #2: http listener gets the same drain signal
    // as the gRPC listeners so long-running HTTP responses (SSE, large
    // file transfers through sandbox subdomains) aren't aborted with a
    // TCP RST when the proxy shuts down. The HTTP server tracks
    // in-flight connections via a JoinSet and waits up to `drain_timeout`
    // for them to finish their current response.
    let http_drain_rx = drain_complete_rx.clone();
    let http_drain_timeout = drain_timeout;
    let http_handle = tokio::spawn(async move {
        http_server
            .run_with_shutdown(http_listener, wait_for_drain(http_drain_rx), http_drain_timeout)
            .await
    });

    if let Some(internal_handle) = internal_grpc_handle {
        tokio::select! {
            result = public_grpc_handle => { result??; }
            result = internal_handle => { result??; }
            result = http_handle => { result?.map_err(|e| e.to_string())?; }
        }
    } else {
        tokio::select! {
            result = public_grpc_handle => { result??; }
            result = http_handle => { result?.map_err(|e| e.to_string())?; }
        }
    }

    info!("proxy shut down");
    Ok(())
}

/// Resolves when the proxy's drain coordinator signals "tonic may
/// shut down now." Each listener subscribes to the same watch channel
/// via a clone of the receiver; `borrow_and_update()` on first poll
/// observes the value even if the send happened before subscription.
async fn wait_for_drain(mut rx: tokio::sync::watch::Receiver<bool>) {
    if *rx.borrow_and_update() {
        return;
    }
    while rx.changed().await.is_ok() {
        if *rx.borrow_and_update() {
            return;
        }
    }
}

pub async fn run_agent(args: AgentArgs) -> Result<(), Box<dyn std::error::Error>> {
    let agent_id = AgentId::new();
    info!(agent_id = %agent_id, "agent starting");

    let join_token = JoinToken::new(args.token.into_inner());

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

    // Comp-3 A1/B2/C1: each connection runs in its own reconnect loop.
    // The agent process stays alive across controller / proxy blips, and
    // sandboxes are only torn down on a real SIGTERM/SIGINT shutdown
    // (handled below). The two loops are independent so a controller
    // outage doesn't restart the proxy session, and vice versa.
    let ctrl_handle = tokio::spawn(async move {
        let mut backoff = open_sandbox_agent::reconnect::ExponentialBackoff::new();
        loop {
            match controller_conn.run(&controller_url).await {
                Ok(()) => {
                    warn!("controller stream closed; reconnecting");
                }
                Err(e) => {
                    warn!(error = %e, "controller connection lost; reconnecting");
                }
            }
            let delay = backoff.next_delay();
            info!(reconnect_delay_ms = delay.as_millis() as u64, "controller reconnect backoff");
            tokio::time::sleep(delay).await;
        }
    });
    let proxy_handle = tokio::spawn(async move {
        let mut backoff = open_sandbox_agent::reconnect::ExponentialBackoff::new();
        loop {
            match proxy_conn.run(&proxy_url).await {
                Ok(()) => {
                    warn!("proxy stream closed; reconnecting");
                }
                Err(e) => {
                    warn!(error = %e, "proxy connection lost; reconnecting");
                }
            }
            let delay = backoff.next_delay();
            info!(reconnect_delay_ms = delay.as_millis() as u64, "proxy reconnect backoff");
            tokio::time::sleep(delay).await;
        }
    });

    // Block until a real shutdown signal — neither reconnect loop ever
    // returns of its own accord.
    shutdown_signal().await;
    ctrl_handle.abort();
    proxy_handle.abort();

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

    let api_key = args.api_key.into_inner();
    if api_key.is_empty() {
        // Fail closed: an empty key would let any caller authenticate
        // (constant_time_eq("","") = true). The api requires a key —
        // OPEN_SANDBOX_API_KEY env var or --api-key flag.
        return Err(
            "OPEN_SANDBOX_API_KEY must be set to a non-empty value; refusing to start with empty key"
                .into(),
        );
    }
    let state = Arc::new(ApiState {
        lifecycle,
        proxy,
        api_key,
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

/// Run both controller and proxy schema migrations against a single
/// Postgres URL. Idempotent: every statement is `CREATE TABLE/INDEX
/// IF NOT EXISTS`, so re-running is a no-op. Production deploys
/// invoke this via `open-sandbox migrate` once before starting the
/// long-running services; dev environments set `--auto-migrate`
/// instead. The proxy index depends on the controller's
/// `routing_entries` table, so the controller migration is applied
/// first and the proxy migration uses the same retry loop the
/// long-running proxy uses (the index creation can race a concurrent
/// `CREATE TABLE` on a cold DB).
pub async fn run_migrate(args: MigrateArgs) -> Result<(), Box<dyn std::error::Error>> {
    info!("running schema migrations");
    let pool = sqlx::PgPool::connect(args.database_url.expose()).await?;
    let pg_store = PgStore::new(pool.clone());
    pg_store.migrate().await?;
    info!("controller migrations applied");
    apply_proxy_migrations_with_retry(&pool).await?;
    info!("migrations complete");
    Ok(())
}

/// Apply the proxy's index creation with bounded retries. Shared
/// between `run_proxy` (when `--auto-migrate` is set) and
/// `run_migrate`. The retry exists because the index is created
/// against the controller's `routing_entries` table; on a fresh DB,
/// concurrent invocations can race the table creation.
async fn apply_proxy_migrations_with_retry(
    pool: &sqlx::PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    let migration_store = PgRoutingStore::new(pool.clone());
    for attempt in 1..=PROXY_STARTUP_RETRY_ATTEMPTS {
        match migration_store.migrate().await {
            Ok(()) => {
                info!("proxy migrations applied");
                return Ok(());
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
    unreachable!("loop above either returns Ok or Err")
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        // Comp-8: fall back to Ctrl-C only if SIGTERM registration fails
        // (e.g. restricted seccomp profile). Previously expect()-panicked
        // during startup before any resources could be cleaned up.
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sigterm) => {
                tokio::select! {
                    _ = ctrl_c => {}
                    _ = sigterm.recv() => {}
                }
            }
            Err(e) => {
                warn!(error = %e, "could not register SIGTERM handler; falling back to Ctrl-C only");
                ctrl_c.await.ok();
            }
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
    // Comp-8: read MemTotal from /proc/meminfo on Linux (where the agent
    // production deployment lives — agent-youki is Linux-only). On
    // non-Linux dev hosts, fall back to a conservative 4 GiB. Without
    // this, the controller's scheduler treated every agent as 4 GiB and
    // refused to place sandboxes >4 GiB onto otherwise-idle 64 GiB boxes.
    #[cfg(target_os = "linux")]
    {
        if let Ok(contents) = std::fs::read_to_string("/proc/meminfo") {
            for line in contents.lines() {
                if let Some(rest) = line.strip_prefix("MemTotal:") {
                    let trimmed = rest.trim().trim_end_matches(" kB").trim();
                    if let Ok(kb) = trimmed.parse::<u64>() {
                        return kb.saturating_mul(1024);
                    }
                }
            }
        }
    }
    4 * 1024 * 1024 * 1024
}

fn host_architecture() -> Architecture {
    match std::env::consts::ARCH {
        "x86_64" => Architecture::X8664,
        "aarch64" => Architecture::Aarch64,
        _ => Architecture::Unspecified,
    }
}
