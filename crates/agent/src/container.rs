use std::collections::HashMap;
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::{mpsc, oneshot};

use open_sandbox_contracts::error::AgentError;
use open_sandbox_contracts::types::SandboxId;

/// Capacity (in frames) for the in-process channels behind ExecHandle.
/// Frames are typically 64 KiB; capacity=4 ⇒ ~256 KiB bounded in
/// flight per direction. Backpressure surfaces here before any
/// kernel buffer fills — see spike 04 RESULT for the chain analysis.
pub const EXEC_CHANNEL_CAPACITY: usize = 4;

#[derive(Debug, Clone)]
pub struct ContainerConfig {
    pub sandbox_id: SandboxId,
    pub image: String,
    pub cpu_limit_millicores: u32,
    pub memory_limit_bytes: u64,
    pub env_vars: HashMap<String, String>,
    pub exposed_port: u32,
    /// v1.0.2: caller-supplied image-cache policy. See
    /// `open_sandbox_contracts::types::PullPolicy`.
    pub pull_policy: open_sandbox_contracts::types::PullPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ContainerId(pub String);

#[derive(Debug, Clone)]
pub struct ContainerInfo {
    pub id: ContainerId,
    pub sandbox_id: SandboxId,
    pub host_port: u16,
    pub running: bool,
}

/// Parameters for starting a streaming exec inside a container.
///
/// Replaces the v0.7 `ExecOptions { command, stdin, cwd }`. Stdin is
/// no longer carried by-value here — it streams via `ExecHandle.stdin`.
#[derive(Debug, Clone)]
pub struct ExecStart {
    pub command: Vec<String>,
    /// Working directory inside the container. Empty string = the
    /// runtime's default (the container's image cwd).
    pub cwd: String,
    /// Additional environment variables overlaid on the container's
    /// existing env. Empty by default.
    pub env: HashMap<String, String>,
}

/// Caller-facing handle to a running exec.
///
/// The runtime backend (docker or youki) spawns two pump tasks per
/// exec: one reads from `stdin_rx` and writes to the runtime's input
/// pipe, the other reads the runtime's output pipes and writes to
/// `stdout_tx` / `stderr_tx`. The caller — typically
/// `drive_io_session` — drives the handle by pushing bytes into
/// `stdin` and pulling from `stdout` / `stderr`, then awaiting
/// `exited`.
///
/// Dropping all of `stdin` / `stdout` / `stderr` triggers the
/// runtime's pump tasks to wind down, the underlying exec session
/// to close, and (via the `ExecRegistry` cleanup hook on the agent
/// side) the in-container PID to receive SIGTERM/SIGKILL if it has
/// not yet exited.
pub struct ExecHandle {
    /// Runtime-assigned identifier for this exec (UUID-ish string).
    /// Diagnostic correlation only — never use this as a lookup key.
    pub exec_id: String,
    /// In-container PID captured at spawn (post-fork, pre-exec is
    /// not possible). Used by the `ExecRegistry` cleanup hook to
    /// deliver SIGTERM/SIGKILL on stream close.
    pub in_container_pid: i32,
    /// Caller pushes stdin bytes into this sender. Dropping the
    /// sender signals stdin EOF to the in-container process.
    pub stdin: mpsc::Sender<Bytes>,
    /// Stdout bytes from the in-container process. Closes when the
    /// process exits.
    pub stdout: mpsc::Receiver<Bytes>,
    /// Stderr bytes from the in-container process. Closes when the
    /// process exits.
    pub stderr: mpsc::Receiver<Bytes>,
    /// Resolves when the exec terminates (normally or via signal).
    /// The error case (oneshot Canceled) means the runtime backend
    /// dropped its sender without sending — treat as IoError on the
    /// outgoing stream.
    pub exited: oneshot::Receiver<ExecExitInfo>,
}

#[derive(Debug, Clone)]
pub struct ExecExitInfo {
    pub exit_code: i32,
    /// True when the runtime reported the executable was missing
    /// (per `detect_command_not_found` heuristic on stderr/stdout).
    /// Distinguishes "command not found" from a process that ran
    /// and exited 127 of its own accord.
    pub command_not_found: bool,
}

pub trait ContainerRuntime: Send + Sync {
    fn create_and_start(
        &self,
        config: ContainerConfig,
    ) -> impl Future<Output = Result<ContainerInfo, AgentError>> + Send;

    fn stop_and_remove(
        &self,
        id: &ContainerId,
        timeout: Duration,
    ) -> impl Future<Output = Result<(), AgentError>> + Send;

    fn list_sandbox_containers(
        &self,
    ) -> impl Future<Output = Result<Vec<ContainerInfo>, AgentError>> + Send;

    /// Start a streaming exec inside the container. Returns an
    /// ExecHandle the caller drives.
    fn start_exec(
        &self,
        id: &ContainerId,
        start: ExecStart,
    ) -> impl Future<Output = Result<ExecHandle, AgentError>> + Send;

    /// Deliver a POSIX signal to the in-container PID. Used by the
    /// ExecRegistry cleanup hook on stream close. Idempotent: if
    /// the PID has already exited, returns Ok(()).
    fn signal_exec(
        &self,
        id: &ContainerId,
        in_container_pid: i32,
        signum: i32,
    ) -> impl Future<Output = Result<(), AgentError>> + Send;

    /// Read a file from the container as a whole. The runtime
    /// resolves relative paths against `cwd` (or
    /// `DEFAULT_WRITE_CWD` if `cwd` is None) and emits the resolved
    /// absolute path in `AgentError::Runtime { detail }` when the
    /// file is missing — preserves the v0.7 `FileNotFound`
    /// resolved-path promise.
    fn read_file(
        &self,
        id: &ContainerId,
        path: &str,
        cwd: Option<&str>,
    ) -> impl Future<Output = Result<Bytes, AgentError>> + Send;

    /// Write a file atomically. Implementations MUST place the temp
    /// file in the target's directory (not /tmp) so the rename is
    /// within a single filesystem.
    fn write_file(
        &self,
        id: &ContainerId,
        path: &str,
        cwd: Option<&str>,
        content: Bytes,
    ) -> impl Future<Output = Result<(), AgentError>> + Send;

    /// Extract a tar.gz tarball into the container at `cwd` (or
    /// `DEFAULT_WRITE_CWD`). Creates the target directory if needed.
    fn write_files_targz(
        &self,
        id: &ContainerId,
        cwd: Option<&str>,
        tarball: Bytes,
    ) -> impl Future<Output = Result<(), AgentError>> + Send;
}

/// Wrap a user command so the in-container process emits its own
/// in-namespace `$$` on stderr as the very first line, then `exec`s
/// the real command. This is how both runtime backends capture the
/// in-container PID — the runtime-reported "pid" (bollard's
/// `inspect_exec.pid`, youki's `nsenter` child pid) is always the
/// HOST PID, which is meaningless inside the container's PID
/// namespace where signals must be delivered.
///
/// The wrapper preserves stdin, working directory, environment, and
/// exit code (because `exec` replaces the shell in place).
///
/// The agreed-upon marker line is:
///
///   `OPENSB_INPID=<pid>\n`
///
/// The runtime backend's stderr pump consumes this first line via
/// [`consume_inpid_marker`] before forwarding bytes to the caller.
pub fn wrap_command_with_inpid_marker(cmd: Vec<String>) -> Vec<String> {
    let mut wrapped = vec![
        "sh".to_string(),
        "-c".to_string(),
        "printf 'OPENSB_INPID=%s\\n' \"$$\" 1>&2; exec \"$@\"".to_string(),
        // $0 — the wrapper's argv[0]; the user's argv starts at $1.
        "opensb-wrapper".to_string(),
    ];
    wrapped.extend(cmd);
    wrapped
}

/// Maximum bytes the stderr pump will buffer while waiting for the
/// `OPENSB_INPID=...\n` marker. If the marker doesn't arrive within
/// this many bytes (e.g. the user's command writes a huge first
/// stderr burst before the shell's printf flushes), we give up
/// capturing the pid but still forward the bytes verbatim.
pub const INPID_MARKER_MAX_BUFFER: usize = 256;

/// Attempt to extract the `OPENSB_INPID=<n>\n` marker from the head
/// of a pending stderr buffer. Returns:
///
/// - `Ok(Some(pid))` — marker found and consumed; `buf` now starts
///   at the byte after the trailing newline.
/// - `Ok(None)` — no newline yet AND the buffer is still under the
///   patience window; caller should keep accumulating.
/// - `Err(())` — the head does not match `OPENSB_INPID=` OR the
///   buffer exceeded the patience window; caller should stop
///   trying to extract and just forward everything in `buf`.
///
/// Pure parser; safe to call without any I/O.
#[allow(clippy::result_unit_err)] // Err carries no info; caller branches on Err vs Ok variants.
pub fn consume_inpid_marker(buf: &mut Vec<u8>) -> Result<Option<i32>, ()> {
    const PREFIX: &[u8] = b"OPENSB_INPID=";
    // Head must match the prefix (or be a strict prefix of it while
    // we are still receiving).
    if buf.len() < PREFIX.len() {
        if !PREFIX.starts_with(buf) {
            return Err(());
        }
        if buf.len() > INPID_MARKER_MAX_BUFFER {
            return Err(());
        }
        return Ok(None);
    }
    if &buf[..PREFIX.len()] != PREFIX {
        return Err(());
    }
    // Look for the terminating newline within the patience window.
    let Some(nl) = buf.iter().position(|&b| b == b'\n') else {
        if buf.len() > INPID_MARKER_MAX_BUFFER {
            return Err(());
        }
        return Ok(None);
    };
    // Parse the digits between PREFIX.len() and nl.
    let digits = &buf[PREFIX.len()..nl];
    let s = std::str::from_utf8(digits).map_err(|_| ())?;
    let pid: i32 = s.parse().map_err(|_| ())?;
    // Consume the marker line (including the trailing newline).
    buf.drain(..=nl);
    Ok(Some(pid))
}

/// Heuristic shared by all runtimes: scan stderr (or stdout, when
/// the runtime is known to pipe the diagnostic to the wrong stream)
/// for the canonical OCI "executable file not found" message
/// produced by runc/crun/youki when the requested binary cannot be
/// resolved in the container.
pub fn detect_command_not_found(text: &[u8]) -> bool {
    let s = String::from_utf8_lossy(text);
    let lower = s.to_ascii_lowercase();
    // Pattern A: OCI runtime diagnostic (runc, crun, youki).
    if lower.contains("executable file not found") {
        return true;
    }
    // Pattern B: shell-wrapper failure. With v1.0's
    // `wrap_command_with_inpid_marker`, an unresolvable command is
    // raised by `exec` inside the shell, which prints
    //   "opensb-wrapper: exec: line N: <cmd>: not found"
    // on stderr and exits 127. Match the wrapper marker so we don't
    // false-positive on user programs that legitimately print
    // "not found".
    if lower.contains("opensb-wrapper") && lower.contains("not found") {
        return true;
    }
    // Pattern C: legacy "no such file or directory" emitted by the
    // runtime when starting the container process — preserved for
    // backends that wrap exec failures this way.
    lower.contains("no such file or directory")
        && (lower.contains("exec:") || lower.contains("starting container process"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inpid_marker_full_line_in_one_chunk() {
        let mut buf = b"OPENSB_INPID=7\nhello\n".to_vec();
        assert_eq!(consume_inpid_marker(&mut buf), Ok(Some(7)));
        assert_eq!(&buf[..], b"hello\n");
    }

    #[test]
    fn inpid_marker_split_across_chunks() {
        let mut buf = b"OPENSB_INPID=".to_vec();
        assert_eq!(consume_inpid_marker(&mut buf), Ok(None));
        buf.extend_from_slice(b"42\n");
        assert_eq!(consume_inpid_marker(&mut buf), Ok(Some(42)));
        assert!(buf.is_empty());
    }

    #[test]
    fn inpid_marker_absent_short_buffer() {
        let mut buf = b"hi".to_vec();
        assert_eq!(consume_inpid_marker(&mut buf), Err(()));
    }

    #[test]
    fn inpid_marker_absent_long_buffer() {
        let mut buf = b"OPENSB_INPID=".to_vec();
        buf.extend(std::iter::repeat_n(b'x', INPID_MARKER_MAX_BUFFER + 1));
        // exceeds patience without newline
        assert_eq!(consume_inpid_marker(&mut buf), Err(()));
    }

    #[test]
    fn inpid_marker_non_numeric_payload() {
        let mut buf = b"OPENSB_INPID=abc\n".to_vec();
        assert_eq!(consume_inpid_marker(&mut buf), Err(()));
    }

    #[test]
    fn wrap_preserves_user_command() {
        let wrapped = wrap_command_with_inpid_marker(vec!["echo".into(), "hi".into()]);
        // Wrapper must be a shell that exec's the user's argv via $@.
        assert_eq!(wrapped[0], "sh");
        assert_eq!(wrapped[1], "-c");
        assert!(wrapped[2].contains("OPENSB_INPID"));
        assert!(wrapped[2].contains("exec \"$@\""));
        // $0 is reserved for the wrapper; user args start at $1.
        assert_eq!(wrapped[3], "opensb-wrapper");
        assert_eq!(&wrapped[4..], &["echo", "hi"]);
    }
}
