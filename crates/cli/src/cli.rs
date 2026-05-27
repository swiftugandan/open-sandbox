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
    /// Run a one-off command in a fresh sandbox and stream its output.
    /// Like `docker run --rm`, but the workload executes on whatever
    /// agent fleet the api gateway is in front of. The sandbox is
    /// created, the command is exec'd over the streaming WebSocket,
    /// and on exit the sandbox is destroyed.
    Run(RunArgs),
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
pub struct RunArgs {
    /// Container image to run (e.g. `ubuntu:22.04`, `python:3.12-alpine`).
    #[arg(long)]
    pub image: String,

    /// Environment variable to set inside the sandbox, in `KEY=VAL`
    /// form. Repeat the flag for multiple variables.
    #[arg(long = "env", value_parser = parse_env_kv)]
    pub env: Vec<(String, String)>,

    /// CPU allocation in millicores (1000 = 1 vCPU).
    #[arg(
        long,
        default_value_t = open_sandbox_contracts::constants::DEFAULT_SANDBOX_CPU_MILLICORES,
    )]
    pub cpu_millicores: u32,

    /// Memory allocation in bytes.
    #[arg(
        long,
        default_value_t = open_sandbox_contracts::constants::DEFAULT_SANDBOX_MEMORY_BYTES,
    )]
    pub memory_bytes: u64,

    /// Working directory inside the container.
    #[arg(long)]
    pub cwd: Option<String>,

    /// Image pull policy: `if-not-present` (default), `always`, or `never`.
    #[arg(long, value_parser = parse_pull_policy)]
    pub pull_policy: Option<open_sandbox_contracts::types::PullPolicy>,

    /// Base URL of the open-sandbox api gateway. http(s) for REST,
    /// the same host:port speaks ws(s) for the exec stream.
    #[arg(
        long,
        default_value = "http://127.0.0.1:8081",
        env = "OPEN_SANDBOX_API_BASE"
    )]
    pub api_base: String,

    /// Bearer API key for the api gateway (never logged).
    #[arg(long, env = "OPEN_SANDBOX_API_KEY")]
    pub api_key: Redacted,

    /// Seconds to wait for the sandbox to reach `running` before
    /// giving up. Image pulls of unfamiliar images can take 5–30s.
    #[arg(long, default_value_t = 120)]
    pub start_timeout_secs: u64,

    /// Command and arguments to exec inside the sandbox.
    #[arg(trailing_var_arg = true, required = true)]
    pub command: Vec<String>,
}

fn parse_env_kv(s: &str) -> Result<(String, String), String> {
    match s.split_once('=') {
        Some((k, v)) if !k.is_empty() => Ok((k.to_string(), v.to_string())),
        _ => Err(format!("expected KEY=VAL with non-empty KEY, got `{s}`")),
    }
}

fn parse_pull_policy(s: &str) -> Result<open_sandbox_contracts::types::PullPolicy, String> {
    use open_sandbox_contracts::types::PullPolicy;
    match s {
        "if-not-present" => Ok(PullPolicy::IfNotPresent),
        "always" => Ok(PullPolicy::Always),
        "never" => Ok(PullPolicy::Never),
        other => Err(format!(
            "unknown pull policy `{other}`; expected if-not-present, always, or never"
        )),
    }
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
