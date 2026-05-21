use clap::Parser;
use open_sandbox::cli::{Cli, Command};
use open_sandbox::run;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    match cli.command {
        Command::Controller(args) => run::run_controller(args).await,
        Command::Proxy(args) => run::run_proxy(args).await,
        Command::Agent(args) => run::run_agent(args).await,
    }
}
