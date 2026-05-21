use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "open-sandbox",
    about = "Open Sandbox — isolated Docker environments with public HTTPS access",
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
}

#[derive(Parser, Debug, Clone)]
pub struct ControllerArgs {
    /// gRPC listen port for agent connections
    #[arg(long, default_value_t = 50051, env = "OPEN_SANDBOX_CONTROLLER_GRPC_PORT")]
    pub grpc_port: u16,

    /// PostgreSQL connection URL
    #[arg(long, env = "OPEN_SANDBOX_DATABASE_URL")]
    pub database_url: String,

    /// Dead agent sweep interval in seconds
    #[arg(long, default_value_t = 15, env = "OPEN_SANDBOX_SWEEP_INTERVAL")]
    pub sweep_interval: u64,
}

#[derive(Parser, Debug, Clone)]
pub struct ProxyArgs {
    /// HTTP listen port for incoming sandbox requests
    #[arg(long, default_value_t = 8080, env = "OPEN_SANDBOX_PROXY_HTTP_PORT")]
    pub http_port: u16,

    /// gRPC listen port for agent tunnel connections
    #[arg(long, default_value_t = 50052, env = "OPEN_SANDBOX_PROXY_GRPC_PORT")]
    pub grpc_port: u16,

    /// PostgreSQL connection URL
    #[arg(long, env = "OPEN_SANDBOX_DATABASE_URL")]
    pub database_url: String,
}

#[derive(Parser, Debug, Clone)]
pub struct AgentArgs {
    /// Join token for authenticating with the controller
    #[arg(long, env = "OPEN_SANDBOX_JOIN_TOKEN")]
    pub token: String,

    /// Controller gRPC address
    #[arg(long, default_value = "http://127.0.0.1:50051", env = "OPEN_SANDBOX_CONTROLLER_URL")]
    pub controller_url: String,

    /// Proxy gRPC address for tunnel connections
    #[arg(long, default_value = "http://127.0.0.1:50052", env = "OPEN_SANDBOX_PROXY_URL")]
    pub proxy_url: String,
}
