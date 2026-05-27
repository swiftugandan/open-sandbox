use clap::{Parser, Subcommand};

use crate::secret::Redacted;

#[derive(Parser, Debug)]
#[command(
    name = "open-sandbox",
    about = "Open Sandbox — isolated container environments with public HTTPS access",
    version = env!("CARGO_PKG_VERSION"),
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Start the controller server
    Controller(ControllerArgs),
    /// Start the proxy server
    Proxy(ProxyArgs),
    /// Start the agent and connect to a controller
    Agent(AgentArgs),
    /// Start the REST API gateway
    Api(ApiArgs),
    /// Run database schema migrations (controller + proxy) and exit.
    /// Idempotent — safe to run multiple times. Production deploys should
    /// run this once before starting the long-running services; dev
    /// deploys can rely on --auto-migrate on the controller/proxy
    /// subcommands instead.
    Migrate(MigrateArgs),
}

#[derive(Parser, Debug, Clone)]
pub struct ControllerArgs {
    /// gRPC listen port for agent connections
    #[arg(
        long,
        default_value_t = 50051,
        env = "OPEN_SANDBOX_CONTROLLER_GRPC_PORT"
    )]
    pub grpc_port: u16,

    /// PostgreSQL connection URL (contains the password — never logged)
    #[arg(long, env = "OPEN_SANDBOX_DATABASE_URL")]
    pub database_url: Redacted,

    /// Dead agent sweep interval in seconds
    #[arg(long, default_value_t = 15, env = "OPEN_SANDBOX_SWEEP_INTERVAL")]
    pub sweep_interval: u64,

    /// Run schema migrations on startup. Off by default in production —
    /// run `open-sandbox migrate` once before starting services so a
    /// migration failure doesn't cascade into a service-startup failure.
    /// Dev environments (docker-compose, `open-sandbox dev`) pass this
    /// flag to preserve a frictionless first-run.
    #[arg(long, default_value_t = false, env = "OPEN_SANDBOX_AUTO_MIGRATE")]
    pub auto_migrate: bool,
}

#[derive(Parser, Debug, Clone)]
pub struct ProxyArgs {
    /// HTTP listen port for incoming sandbox requests
    #[arg(long, default_value_t = 8080, env = "OPEN_SANDBOX_PROXY_HTTP_PORT")]
    pub http_port: u16,

    /// gRPC listen port for agent tunnel connections (Public role:
    /// hosts only the OpenTunnel RPC; OpenIoStream calls are
    /// rejected with Unimplemented at the role gate).
    #[arg(long, default_value_t = 50052, env = "OPEN_SANDBOX_PROXY_GRPC_PORT")]
    pub grpc_port: u16,

    /// gRPC listen port for gateway → proxy OpenIoStream calls
    /// (Internal role; hosts only the OpenIoStream RPC). In
    /// production this listener should be reachable ONLY from the
    /// api gateway's network segment. The bearer-token check
    /// (INTERNAL_TOKEN env) layers on top of the network
    /// isolation. Set to the same value as `--grpc-port` to fall
    /// back to a single combined listener (development only).
    #[arg(
        long,
        default_value_t = 50053,
        env = "OPEN_SANDBOX_PROXY_INTERNAL_GRPC_PORT"
    )]
    pub internal_grpc_port: u16,

    /// PostgreSQL connection URL (contains the password — never logged)
    #[arg(long, env = "OPEN_SANDBOX_DATABASE_URL")]
    pub database_url: Redacted,

    /// Run schema migrations on startup. Off by default in production;
    /// see ControllerArgs.auto_migrate for the rationale.
    #[arg(long, default_value_t = false, env = "OPEN_SANDBOX_AUTO_MIGRATE")]
    pub auto_migrate: bool,

    /// Maximum seconds to wait for in-flight IoSessions to complete
    /// after SIGTERM before sending each remaining gateway stream a
    /// terminal `Unavailable` frame and exiting. Bounds the blast
    /// radius of a planned proxy restart. Default: 30s.
    #[arg(
        long,
        default_value_t = 30,
        env = "OPEN_SANDBOX_SHUTDOWN_DRAIN_TIMEOUT"
    )]
    pub shutdown_drain_timeout: u64,
}

#[derive(Parser, Debug, Clone)]
pub struct MigrateArgs {
    /// PostgreSQL connection URL (contains the password — never logged).
    /// Same value as the controller's and proxy's --database-url.
    #[arg(long, env = "OPEN_SANDBOX_DATABASE_URL")]
    pub database_url: Redacted,
}

#[derive(Parser, Debug, Clone)]
pub struct AgentArgs {
    /// Join token for authenticating with the controller (never logged)
    #[arg(long, env = "OPEN_SANDBOX_JOIN_TOKEN")]
    pub token: Redacted,

    /// Controller gRPC address
    #[arg(
        long,
        default_value = "http://127.0.0.1:50051",
        env = "OPEN_SANDBOX_CONTROLLER_URL"
    )]
    pub controller_url: String,

    /// Proxy gRPC address for tunnel connections
    #[arg(
        long,
        default_value = "http://127.0.0.1:50052",
        env = "OPEN_SANDBOX_PROXY_URL"
    )]
    pub proxy_url: String,
}

#[derive(Parser, Debug, Clone)]
pub struct ApiArgs {
    /// HTTP listen port for the REST + WebSocket API
    #[arg(long, default_value_t = open_sandbox_contracts::constants::API_DEFAULT_PORT, env = "OPEN_SANDBOX_API_PORT")]
    pub port: u16,

    /// Controller gRPC address (lifecycle RPCs)
    #[arg(
        long,
        default_value = "http://127.0.0.1:50051",
        env = "OPEN_SANDBOX_CONTROLLER_URL"
    )]
    pub controller_url: String,

    /// Proxy gRPC address for OpenIoStream — the proxy's
    /// INTERNAL listener (50053 by default; see ProxyArgs.
    /// internal_grpc_port). Dialing the public port 50052
    /// will be rejected with Unimplemented at the role gate.
    #[arg(
        long,
        default_value = "http://127.0.0.1:50053",
        env = "OPEN_SANDBOX_PROXY_URL"
    )]
    pub proxy_url: String,

    /// Bearer API key callers must present to the gateway (never logged).
    /// In production, set via env. Test default included for dev only.
    #[arg(long, env = "OPEN_SANDBOX_API_KEY")]
    pub api_key: Redacted,
}
