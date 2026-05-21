use clap::Parser;
use open_sandbox::cli::{Cli, Command};

#[test]
fn controller_subcommand_parses_with_required_args() {
    let cli = Cli::parse_from(["open-sandbox", "controller", "--database-url", "postgres://localhost/test"]);
    match cli.command {
        Command::Controller(args) => {
            assert_eq!(args.database_url, "postgres://localhost/test");
            assert_eq!(args.grpc_port, 50051);
            assert_eq!(args.sweep_interval, 15);
        }
        _ => panic!("expected Controller subcommand"),
    }
}

#[test]
fn controller_subcommand_overrides_defaults() {
    let cli = Cli::parse_from([
        "open-sandbox", "controller",
        "--database-url", "postgres://db/prod",
        "--grpc-port", "9000",
        "--sweep-interval", "30",
    ]);
    match cli.command {
        Command::Controller(args) => {
            assert_eq!(args.grpc_port, 9000);
            assert_eq!(args.sweep_interval, 30);
        }
        _ => panic!("expected Controller subcommand"),
    }
}

#[test]
fn proxy_subcommand_parses_with_required_args() {
    let cli = Cli::parse_from(["open-sandbox", "proxy", "--database-url", "postgres://localhost/test"]);
    match cli.command {
        Command::Proxy(args) => {
            assert_eq!(args.database_url, "postgres://localhost/test");
            assert_eq!(args.http_port, 8080);
            assert_eq!(args.grpc_port, 50052);
        }
        _ => panic!("expected Proxy subcommand"),
    }
}

#[test]
fn proxy_subcommand_overrides_defaults() {
    let cli = Cli::parse_from([
        "open-sandbox", "proxy",
        "--database-url", "postgres://db/prod",
        "--http-port", "443",
        "--grpc-port", "9001",
    ]);
    match cli.command {
        Command::Proxy(args) => {
            assert_eq!(args.http_port, 443);
            assert_eq!(args.grpc_port, 9001);
        }
        _ => panic!("expected Proxy subcommand"),
    }
}

#[test]
fn agent_subcommand_parses_with_required_token() {
    let cli = Cli::parse_from(["open-sandbox", "agent", "--token", "secret-join-token"]);
    match cli.command {
        Command::Agent(args) => {
            assert_eq!(args.token, "secret-join-token");
            assert_eq!(args.controller_url, "http://127.0.0.1:50051");
            assert_eq!(args.proxy_url, "http://127.0.0.1:50052");
        }
        _ => panic!("expected Agent subcommand"),
    }
}

#[test]
fn agent_subcommand_overrides_defaults() {
    let cli = Cli::parse_from([
        "open-sandbox", "agent",
        "--token", "my-token",
        "--controller-url", "http://ctrl:50051",
        "--proxy-url", "http://prx:50052",
    ]);
    match cli.command {
        Command::Agent(args) => {
            assert_eq!(args.controller_url, "http://ctrl:50051");
            assert_eq!(args.proxy_url, "http://prx:50052");
        }
        _ => panic!("expected Agent subcommand"),
    }
}

#[test]
fn agent_subcommand_fails_without_token() {
    let result = Cli::try_parse_from(["open-sandbox", "agent"]);
    assert!(result.is_err(), "agent subcommand should require --token");
}

#[test]
fn controller_subcommand_fails_without_database_url() {
    let result = Cli::try_parse_from(["open-sandbox", "controller"]);
    assert!(result.is_err(), "controller subcommand should require --database-url");
}

#[test]
fn proxy_subcommand_fails_without_database_url() {
    let result = Cli::try_parse_from(["open-sandbox", "proxy"]);
    assert!(result.is_err(), "proxy subcommand should require --database-url");
}

#[test]
fn no_subcommand_fails() {
    let result = Cli::try_parse_from(["open-sandbox"]);
    assert!(result.is_err(), "should require a subcommand");
}
