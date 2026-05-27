use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn binary_prints_help() {
    Command::cargo_bin("open-sandbox")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("controller"))
        .stdout(predicate::str::contains("proxy"))
        .stdout(predicate::str::contains("agent"));
}

#[test]
fn binary_prints_version() {
    Command::cargo_bin("open-sandbox")
        .unwrap()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("0.1.0"));
}

#[test]
fn controller_help_shows_flags() {
    Command::cargo_bin("open-sandbox")
        .unwrap()
        .args(["controller", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--grpc-port"))
        .stdout(predicate::str::contains("--database-url"))
        .stdout(predicate::str::contains("--sweep-interval"));
}

#[test]
fn proxy_help_shows_flags() {
    Command::cargo_bin("open-sandbox")
        .unwrap()
        .args(["proxy", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--http-port"))
        .stdout(predicate::str::contains("--grpc-port"))
        .stdout(predicate::str::contains("--database-url"));
}

#[test]
fn agent_help_shows_flags() {
    Command::cargo_bin("open-sandbox")
        .unwrap()
        .args(["agent", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--token"))
        .stdout(predicate::str::contains("--controller-url"))
        .stdout(predicate::str::contains("--proxy-url"));
}

#[test]
fn run_help_shows_flags() {
    Command::cargo_bin("open-sandbox")
        .unwrap()
        .args(["run", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--image"))
        .stdout(predicate::str::contains("--env"))
        .stdout(predicate::str::contains("--api-base"))
        .stdout(predicate::str::contains("--api-key"))
        .stdout(predicate::str::contains("COMMAND"));
}

#[test]
fn run_without_image_fails() {
    Command::cargo_bin("open-sandbox")
        .unwrap()
        .args(["run", "--api-key", "k", "--", "true"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--image"));
}

#[test]
fn ssh_help_shows_flags() {
    Command::cargo_bin("open-sandbox")
        .unwrap()
        .args(["ssh", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("SANDBOX_ID"))
        .stdout(predicate::str::contains("--no-install"))
        .stdout(predicate::str::contains("--ssh-key"))
        .stdout(predicate::str::contains("--api-base"));
}

#[test]
fn ssh_hidden_subcommand_does_not_leak_in_top_help() {
    // ssh-pipe is `hide = true` — should not appear in `--help`.
    Command::cargo_bin("open-sandbox")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("ssh-pipe").not());
}

#[test]
fn ssh_pipe_is_still_callable_when_hidden() {
    // Hidden, but invocable + has its own help.
    Command::cargo_bin("open-sandbox")
        .unwrap()
        .args(["ssh-pipe", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("SANDBOX_ID"));
}

#[test]
fn ssh_without_sandbox_id_fails() {
    Command::cargo_bin("open-sandbox")
        .unwrap()
        .args(["ssh", "--api-key", "k"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("SANDBOX_ID"));
}

#[test]
fn run_rejects_malformed_env() {
    Command::cargo_bin("open-sandbox")
        .unwrap()
        .args([
            "run",
            "--image",
            "alpine:3.21",
            "--api-key",
            "k",
            "--env",
            "no-equals-here",
            "--",
            "true",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("KEY=VAL"));
}

#[test]
fn agent_without_token_fails() {
    Command::cargo_bin("open-sandbox")
        .unwrap()
        .arg("agent")
        .assert()
        .failure()
        .stderr(predicate::str::contains("--token"));
}

#[test]
fn no_subcommand_fails() {
    Command::cargo_bin("open-sandbox")
        .unwrap()
        .assert()
        .failure();
}
