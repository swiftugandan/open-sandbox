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

/// Heuristic shared by all runtimes: scan stderr (or stdout, when
/// the runtime is known to pipe the diagnostic to the wrong stream)
/// for the canonical OCI "executable file not found" message
/// produced by runc/crun/youki when the requested binary cannot be
/// resolved in the container.
pub fn detect_command_not_found(text: &[u8]) -> bool {
    let s = String::from_utf8_lossy(text);
    let lower = s.to_ascii_lowercase();
    lower.contains("executable file not found")
        || lower.contains("no such file or directory")
            && (lower.contains("exec:") || lower.contains("starting container process"))
}
