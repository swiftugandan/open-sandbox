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
