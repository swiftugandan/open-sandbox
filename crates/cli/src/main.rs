use std::process::ExitCode;

use clap::Parser;
use open_sandbox::cli::{Cli, Command};
use open_sandbox::{run, run_subcommand};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    // Comp-8: report a malformed RUST_LOG via stderr before falling back
    // to "info", so operator typos are visible during incident triage.
    let env_filter = match EnvFilter::try_from_default_env() {
        Ok(f) => f,
        Err(e) => {
            if std::env::var_os("RUST_LOG").is_some() {
                eprintln!("warning: failed to parse RUST_LOG ({e}); falling back to 'info'");
            }
            EnvFilter::new("info")
        }
    };
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(env_filter)
        .init();

    // Comp-8: abort the process on any spawned-task panic instead of
    // letting tokio swallow it and continue running with broken state.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        prev_hook(info);
        eprintln!("panic in spawned task; aborting process to avoid silent broken state");
        std::process::abort();
    }));

    let cli = Cli::parse();

    match cli.command {
        Command::Controller(args) => report(run::run_controller(args).await),
        Command::Proxy(args) => report(run::run_proxy(args).await),
        Command::Agent(args) => report(run::run_agent(args).await),
        Command::Api(args) => report(run::run_api(args).await),
        Command::Migrate(args) => report(run::run_migrate(args).await),
        Command::Run(args) => run_subcommand::run(args).await,
    }
}

fn report(r: Result<(), Box<dyn std::error::Error>>) -> ExitCode {
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {e:?}");
            ExitCode::FAILURE
        }
    }
}
