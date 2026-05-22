use clap::Parser;
use open_sandbox::cli::{Cli, Command};
use open_sandbox::run;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::Controller(args) => run::run_controller(args).await,
        Command::Proxy(args) => run::run_proxy(args).await,
        Command::Agent(args) => run::run_agent(args).await,
        Command::Api(args) => run::run_api(args).await,
    }
}
