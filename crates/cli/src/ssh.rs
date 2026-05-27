//! `open-sandbox ssh` + `open-sandbox ssh-pipe`.
//!
//! ## Architecture
//!
//! The sandbox fleet is reached over a single primitive: the v1.0
//! streaming-exec WebSocket. SSH is just another full-duplex byte
//! protocol — we run `sshd -i` (inetd mode) inside the container and
//! its stdio is the SSH connection. No new endpoints, no new proto,
//! no in-container listening port.
//!
//! ```text
//! ssh / scp / git / code-remote
//!   └─> ProxyCommand: open-sandbox ssh-pipe <id>
//!         └─> WS /v1/sandboxes/{id}/exec   [api gateway, unchanged]
//!               └─> OpenIoStream            [proxy → agent, unchanged]
//!                     └─> docker exec / nsenter: `sshd -i -e`
//! ```
//!
//! ## Subcommands
//!
//! - `ssh-pipe <id>` is the load-bearing one — opens the WS exec,
//!   sends an `IoStart` whose command is the openssh bootstrap +
//!   `exec sshd -i -e` one-liner, and pipes local stdin↔stdout to
//!   the `Stdin`/`Stdout` frames. SSH handles signals, framing,
//!   keepalive itself; we just shovel bytes.
//!
//! - `ssh <id>` is UX sugar — `exec`s a local `ssh` client with
//!   `-o ProxyCommand="<self> ssh-pipe <id> --api-base …"` and the
//!   api key forwarded via the environment (NOT argv, to keep the
//!   secret out of `ps -ef`). User gets a real ssh client with
//!   config-file integration, keepalive, scp/rsync/code-remote
//!   compatibility for free.
//!
//! ## Auth model
//!
//! The api key gates the WS upgrade. Inside the sandbox sshd is
//! configured with `PermitEmptyPasswords yes + PermitRootLogin yes`
//! because the channel is already authenticated one layer up — same
//! trust model as `/v1/sandboxes/{id}/exec` today. A compromised api
//! key already implied RCE; SSH doesn't widen the surface.
//!
//! ## Image requirement
//!
//! `openssh-server` must be in the sandbox. The bootstrap one-liner
//! auto-installs via `apk add` (alpine) or `apt-get install` (debian)
//! on first connect. `--no-install` skips this for air-gapped /
//! pre-baked images.

use std::io::{Read, Write};
use std::process::ExitCode;

use open_sandbox_ws_client::{ExecParams, ExecSession, ServerFrame, WsClientError};
use tokio::sync::mpsc;

use crate::cli::{SshArgs, SshPipeArgs};

const EXIT_CONNECT_FAILED: u8 = 124;
const EXIT_REMOTE_ERROR: u8 = 125;
const EXIT_SESSION_BROKEN: u8 = 126;
const EXIT_IO_ERROR: u8 = 127;

/// The bootstrap-or-exec one-liner sent as the exec command. On first
/// connect inside a sandbox: detects the package manager, installs
/// openssh-server, generates host keys, clears the root password, and
/// edits sshd_config. Subsequent connects short-circuit at the
/// `command -v sshd` check and go straight to `exec sshd -i -e`, so
/// the cost is just one process spawn.
///
/// `-i` = inetd mode (use stdin/stdout as the SSH connection).
/// `-e` = log to stderr (otherwise sshd logs to syslog, which is
///        silent inside most sandbox images).
const SSHD_INETD_LAUNCHER: &str = r#"
if ! command -v sshd >/dev/null 2>&1; then
  if command -v apk >/dev/null 2>&1; then
    apk add --no-cache openssh-server >&2 || exit 127
  elif command -v apt-get >/dev/null 2>&1; then
    apt-get update >&2 && apt-get install -y openssh-server >&2 || exit 127
  else
    echo "open-sandbox ssh: no supported package manager (apk / apt-get); rerun with --no-install on an image that already bundles sshd" >&2
    exit 127
  fi
  ssh-keygen -A >&2 || exit 127
  passwd -d root >&2 || true
  if [ -f /etc/ssh/sshd_config ]; then
    sed -i 's|^#*PermitRootLogin.*|PermitRootLogin yes|; s|^#*PermitEmptyPasswords.*|PermitEmptyPasswords yes|' /etc/ssh/sshd_config 2>/dev/null || true
  fi
fi
exec "$(command -v sshd)" -i -e
"#;

/// Like above but without the install branch — used when --no-install
/// is passed. Just runs sshd; if it isn't there, sshd fails and the
/// client sees a closed connection.
const SSHD_INETD_LAUNCHER_NO_INSTALL: &str = r#"exec "$(command -v sshd)" -i -e"#;

pub async fn ssh_pipe(args: SshPipeArgs) -> ExitCode {
    let api_base = ws_base(&args.api_base);
    let api_key = args.api_key.into_inner();
    if api_key.is_empty() {
        eprintln!("error: OPEN_SANDBOX_API_KEY (or --api-key) must be non-empty");
        return ExitCode::from(EXIT_CONNECT_FAILED);
    }

    // Accept both bare UUIDs ("abc123…") and the `sandbox-<uuid>`
    // form so the recommended `~/.ssh/config` block (which has
    // `Host sandbox-*` and uses `%h` as the ssh-pipe argument)
    // works without a shell-substitution wrapper.
    let sandbox_id = args
        .sandbox_id
        .strip_prefix("sandbox-")
        .unwrap_or(&args.sandbox_id);

    let launcher = if args.no_install {
        SSHD_INETD_LAUNCHER_NO_INSTALL
    } else {
        SSHD_INETD_LAUNCHER
    };
    let params = ExecParams::new(vec!["sh".into(), "-c".into(), launcher.into()]);

    let mut session = match ExecSession::connect(&api_base, sandbox_id, &api_key, params).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("# open-sandbox ssh-pipe: connect failed: {e}");
            return ExitCode::from(EXIT_CONNECT_FAILED);
        }
    };

    // Stdin pump: read from local stdin in a blocking thread, hand
    // chunks to the async loop via mpsc. ProxyCommand is always a
    // pipe (never a TTY), so we don't need TTY-detection.
    let (tx, mut stdin_rx) = mpsc::channel::<StdinMsg>(8);
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut handle = stdin.lock();
        let mut buf = [0u8; 8192];
        loop {
            match handle.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx
                        .blocking_send(StdinMsg::Chunk(buf[..n].to_vec()))
                        .is_err()
                    {
                        return;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = tx.blocking_send(StdinMsg::Eof);
    });
    let mut stdin_done = false;

    loop {
        let frame_res = if stdin_done {
            session.next_frame().await
        } else {
            tokio::select! {
                msg = stdin_rx.recv() => {
                    match msg {
                        Some(StdinMsg::Chunk(c)) => {
                            if let Err(e) = session.send_stdin(c).await {
                                eprintln!("# ssh-pipe: stdin send: {e}");
                            }
                            continue;
                        }
                        Some(StdinMsg::Eof) | None => {
                            let _ = session.close_stdin().await;
                            stdin_done = true;
                            continue;
                        }
                    }
                }
                f = session.next_frame() => f,
            }
        };

        match frame_res {
            Ok(Some(ServerFrame::Started { .. })) => {}
            Ok(Some(ServerFrame::Stdout(b))) => {
                if std::io::stdout().write_all(&b).is_err() {
                    // Local ssh client closed its end of the pipe.
                    return ExitCode::SUCCESS;
                }
                let _ = std::io::stdout().flush();
            }
            Ok(Some(ServerFrame::Stderr(b))) => {
                let _ = std::io::stderr().write_all(&b);
                let _ = std::io::stderr().flush();
            }
            Ok(Some(ServerFrame::Exited { exit_code, .. })) => {
                return ExitCode::from((exit_code & 0xff) as u8);
            }
            Ok(Some(ServerFrame::Error { code, detail })) => {
                eprintln!("# ssh-pipe: remote error code={code} detail={detail}");
                return ExitCode::from(EXIT_REMOTE_ERROR);
            }
            Ok(None) => return ExitCode::from(EXIT_SESSION_BROKEN),
            Err(WsClientError::ReadTimeout { timeout }) => {
                eprintln!("# ssh-pipe: read timeout after {timeout:?}");
                return ExitCode::from(EXIT_IO_ERROR);
            }
            Err(e) => {
                eprintln!("# ssh-pipe: i/o: {e}");
                return ExitCode::from(EXIT_IO_ERROR);
            }
        }
    }
}

enum StdinMsg {
    Chunk(Vec<u8>),
    Eof,
}

/// User-facing `open-sandbox ssh <id>`. Builds and execs a local
/// `ssh` client whose ProxyCommand recursively invokes this binary's
/// `ssh-pipe` subcommand.
pub fn ssh(args: SshArgs) -> ExitCode {
    let api_key = args.api_key.clone().into_inner();
    if api_key.is_empty() {
        eprintln!("error: OPEN_SANDBOX_API_KEY (or --api-key) must be non-empty");
        return ExitCode::from(EXIT_CONNECT_FAILED);
    }

    let self_path = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: cannot resolve self path for ProxyCommand: {e}");
            return ExitCode::from(EXIT_CONNECT_FAILED);
        }
    };

    // Build the ProxyCommand. CRITICAL: the api key must NOT appear
    // in argv — anything passed via `-o ProxyCommand=...` becomes
    // argv of the local `ssh` AND of the spawned `ssh-pipe`, both
    // visible via `ps -ef` to every user on the box. We pass the
    // key via the environment instead. ssh inherits our env and
    // forwards it to the ProxyCommand subprocess.
    let mut proxy_cmd = format!(
        "{} ssh-pipe {} --api-base {}",
        shell_quote(&self_path.display().to_string()),
        shell_quote(&args.sandbox_id),
        shell_quote(&args.api_base),
    );
    if args.no_install {
        proxy_cmd.push_str(" --no-install");
    }

    let mut cmd = std::process::Command::new("ssh");
    // Set the api key explicitly even if the parent shell already
    // exported it — covers callers who pass --api-key on the
    // command line without setting the env var.
    cmd.env("OPEN_SANDBOX_API_KEY", &api_key);
    cmd.args([
        "-o",
        &format!("ProxyCommand={proxy_cmd}"),
        // Disable host-key checking: the sandbox's host key is
        // ephemeral (generated by ssh-keygen -A on first connect)
        // and we authenticate the *channel* via the api key, so
        // pinning the SSH host key would be theatre.
        "-o",
        "StrictHostKeyChecking=no",
        "-o",
        "UserKnownHostsFile=/dev/null",
        // The default openssh bootstrap sets root with an empty
        // password; ask the ssh client to try that first.
        "-o",
        "PreferredAuthentications=publickey,password",
        // Quiet the "Warning: Permanently added …" line. Real
        // errors still surface.
        "-o",
        "LogLevel=ERROR",
    ]);
    if let Some(key) = args.ssh_key.as_ref() {
        cmd.args(["-i", key]);
        cmd.args(["-o", "PreferredAuthentications=publickey"]);
    }
    cmd.arg(format!("root@sandbox-{}", args.sandbox_id));
    if !args.command.is_empty() {
        cmd.arg("--");
        cmd.args(&args.command);
    }

    // Exec into ssh; the ssh client's exit code becomes ours.
    let status = match cmd.status() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: failed to spawn local `ssh` client: {e}");
            return ExitCode::from(EXIT_CONNECT_FAILED);
        }
    };
    match status.code() {
        Some(c) => ExitCode::from((c & 0xff) as u8),
        None => ExitCode::FAILURE,
    }
}

fn ws_base(base: &str) -> String {
    if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        base.to_string()
    }
}

/// Single-quoted shell escaping. Wraps the input in `'...'` and
/// replaces each embedded single-quote with `'\''` — the canonical
/// POSIX-shell idiom. The result is safe to drop into any `sh -c`
/// or `ProxyCommand=` context regardless of metacharacters.
fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str(r"'\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_base_swaps_scheme() {
        assert_eq!(ws_base("http://localhost:8081"), "ws://localhost:8081");
        assert_eq!(ws_base("https://api.example.com"), "wss://api.example.com");
        assert_eq!(ws_base("ws://x"), "ws://x");
    }

    #[test]
    fn shell_quote_wraps_plain() {
        assert_eq!(shell_quote("abc"), "'abc'");
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("a'b"), r"'a'\''b'");
        assert_eq!(shell_quote("'"), r"''\'''");
    }

    #[test]
    fn shell_quote_handles_metacharacters() {
        // $, `, |, ;, &, *, ?, (, ), <, >, \, !, # — all neutralized
        // by single quotes.
        assert_eq!(shell_quote("$(ls);rm -rf /"), "'$(ls);rm -rf /'");
        assert_eq!(shell_quote("`evil`"), "'`evil`'");
    }
}
