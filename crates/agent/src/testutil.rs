use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::{mpsc, oneshot};

use open_sandbox_contracts::controller::{SandboxConfig, StartSandbox};
use open_sandbox_contracts::error::AgentError;
use open_sandbox_contracts::types::SandboxId;

use crate::container::{
    ContainerConfig, ContainerId, ContainerInfo, ContainerRuntime, EXEC_CHANNEL_CAPACITY,
    ExecExitInfo, ExecHandle, ExecStart, detect_command_not_found,
};
use crate::tunnel::{ForwardRequest, ForwardResponse, HttpClient};

#[derive(Debug, Clone)]
pub struct SignalRecord {
    pub container_id: ContainerId,
    pub pid: i32,
    pub signum: i32,
}

#[derive(Debug, Clone)]
pub struct WriteRecord {
    pub container_id: ContainerId,
    pub resolved_path: String,
    pub bytes: Bytes,
}

/// Programmable mock for streaming runtime testing.
///
/// Default behaviour for `start_exec`:
///   - `echo ARGS...`     → stdout = ARGS joined by space + "\n", exit 0
///   - `cat`              → echo stdin to stdout, exit 0 on stdin EOF
///   - `sleep N`          → wait N seconds (or until a signal arrives), exit 0
///                          or 128+signum on signal
///   - `not_a_binary` (or anything else) → exit 127, command_not_found=true,
///                                          stderr carries OCI diagnostic
///
/// Tests can also pre-seed files via `with_file`.
pub struct MockContainerRuntime {
    created: AtomicUsize,
    stopped: AtomicUsize,
    port_counter: AtomicUsize,
    existing: Mutex<Vec<ContainerInfo>>,
    files: Mutex<HashMap<String, Bytes>>,
    signals_received: Mutex<Vec<SignalRecord>>,
    writes_received: Mutex<Vec<WriteRecord>>,
    next_pid: AtomicI32,
}

impl MockContainerRuntime {
    pub fn new() -> Self {
        Self {
            created: AtomicUsize::new(0),
            stopped: AtomicUsize::new(0),
            port_counter: AtomicUsize::new(9000),
            existing: Mutex::new(Vec::new()),
            files: Mutex::new(HashMap::new()),
            signals_received: Mutex::new(Vec::new()),
            writes_received: Mutex::new(Vec::new()),
            next_pid: AtomicI32::new(1000),
        }
    }

    pub fn with_existing(containers: Vec<ContainerInfo>) -> Self {
        let mut me = Self::new();
        me.existing = Mutex::new(containers);
        me
    }

    pub fn with_file(self, path: impl Into<String>, content: impl Into<Bytes>) -> Self {
        self.files
            .lock()
            .unwrap()
            .insert(path.into(), content.into());
        self
    }

    pub fn created_count(&self) -> usize {
        self.created.load(Ordering::SeqCst)
    }

    pub fn stopped_count(&self) -> usize {
        self.stopped.load(Ordering::SeqCst)
    }

    pub fn signals_received(&self) -> Vec<SignalRecord> {
        self.signals_received.lock().unwrap().clone()
    }

    pub fn writes_received(&self) -> Vec<WriteRecord> {
        self.writes_received.lock().unwrap().clone()
    }
}

impl Default for MockContainerRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl ContainerRuntime for MockContainerRuntime {
    async fn create_and_start(&self, config: ContainerConfig) -> Result<ContainerInfo, AgentError> {
        self.created.fetch_add(1, Ordering::SeqCst);
        let port = self.port_counter.fetch_add(1, Ordering::SeqCst) as u16;
        Ok(ContainerInfo {
            id: ContainerId(format!("mock-{}", config.sandbox_id)),
            sandbox_id: config.sandbox_id,
            host_port: port,
            running: true,
        })
    }

    async fn stop_and_remove(
        &self,
        _id: &ContainerId,
        _timeout: Duration,
    ) -> Result<(), AgentError> {
        self.stopped.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn list_sandbox_containers(&self) -> Result<Vec<ContainerInfo>, AgentError> {
        Ok(self.existing.lock().unwrap().clone())
    }

    async fn start_exec(
        &self,
        _id: &ContainerId,
        start: ExecStart,
    ) -> Result<ExecHandle, AgentError> {
        let pid = self.next_pid.fetch_add(1, Ordering::SeqCst);
        let exec_id = format!("mock-exec-{pid}");

        let (stdin_tx, stdin_rx) = mpsc::channel::<Bytes>(EXEC_CHANNEL_CAPACITY);
        let (stdout_tx, stdout_rx) = mpsc::channel::<Bytes>(EXEC_CHANNEL_CAPACITY);
        let (stderr_tx, stderr_rx) = mpsc::channel::<Bytes>(EXEC_CHANNEL_CAPACITY);
        let (exited_tx, exited_rx) = oneshot::channel::<ExecExitInfo>();

        let cmd = start.command.clone();
        tokio::spawn(simulate_exec(
            cmd, stdin_rx, stdout_tx, stderr_tx, exited_tx,
        ));

        Ok(ExecHandle {
            exec_id,
            in_container_pid: pid,
            stdin: stdin_tx,
            stdout: stdout_rx,
            stderr: stderr_rx,
            exited: exited_rx,
        })
    }

    async fn signal_exec(
        &self,
        id: &ContainerId,
        in_container_pid: i32,
        signum: i32,
    ) -> Result<(), AgentError> {
        self.signals_received.lock().unwrap().push(SignalRecord {
            container_id: id.clone(),
            pid: in_container_pid,
            signum,
        });
        Ok(())
    }

    async fn read_file(
        &self,
        _id: &ContainerId,
        path: &str,
        cwd: Option<&str>,
    ) -> Result<Bytes, AgentError> {
        let resolved = resolve_path(path, cwd);
        self.files
            .lock()
            .unwrap()
            .get(&resolved)
            .cloned()
            .ok_or_else(|| AgentError::Runtime {
                detail: format!("No such file: {resolved}"),
            })
    }

    async fn write_file(
        &self,
        id: &ContainerId,
        path: &str,
        cwd: Option<&str>,
        content: Bytes,
    ) -> Result<(), AgentError> {
        let resolved = resolve_path(path, cwd);
        self.files
            .lock()
            .unwrap()
            .insert(resolved.clone(), content.clone());
        self.writes_received.lock().unwrap().push(WriteRecord {
            container_id: id.clone(),
            resolved_path: resolved,
            bytes: content,
        });
        Ok(())
    }

    async fn write_files_targz(
        &self,
        id: &ContainerId,
        cwd: Option<&str>,
        tarball: Bytes,
    ) -> Result<(), AgentError> {
        self.writes_received.lock().unwrap().push(WriteRecord {
            container_id: id.clone(),
            resolved_path: cwd.unwrap_or("/home").to_string(),
            bytes: tarball,
        });
        Ok(())
    }
}

fn resolve_path(path: &str, cwd: Option<&str>) -> String {
    if path.starts_with('/') {
        return path.to_string();
    }
    let base = cwd.unwrap_or("/home");
    format!("{}/{}", base.trim_end_matches('/'), path)
}

async fn simulate_exec(
    cmd: Vec<String>,
    mut stdin_rx: mpsc::Receiver<Bytes>,
    stdout_tx: mpsc::Sender<Bytes>,
    stderr_tx: mpsc::Sender<Bytes>,
    exited_tx: oneshot::Sender<ExecExitInfo>,
) {
    let exit = match cmd.first().map(String::as_str) {
        Some("echo") => {
            let out = cmd[1..].join(" ") + "\n";
            let _ = stdout_tx.send(Bytes::from(out)).await;
            ExecExitInfo {
                exit_code: 0,
                command_not_found: false,
            }
        }
        Some("cat") => {
            while let Some(bytes) = stdin_rx.recv().await {
                if stdout_tx.send(bytes).await.is_err() {
                    break;
                }
            }
            ExecExitInfo {
                exit_code: 0,
                command_not_found: false,
            }
        }
        Some("sleep") => {
            // Use sleep as a "process that doesn't exit until told."
            // It exits 0 when the requested duration elapses, OR
            // earlier if its stdin is closed (which our io_session
            // mechanism causes when on_stream_closed fires via
            // SIGTERM — well, the mock doesn't actually receive
            // signals through stdin, so we treat stdin-closed +
            // outer drop as the signal arrival).
            let secs: u64 = cmd.get(1).and_then(|s| s.parse().ok()).unwrap_or(30);
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(secs)) => ExecExitInfo {
                    exit_code: 0,
                    command_not_found: false,
                },
                _ = stdin_rx.recv() => {
                    // stdin EOF arrived early — treat as signaled.
                    ExecExitInfo { exit_code: 143, command_not_found: false }
                }
            }
        }
        _ => {
            let msg = format!(
                "OCI runtime exec failed: exec failed: unable to start container process: exec: \"{}\": executable file not found in $PATH\r\n",
                cmd.first().map(String::as_str).unwrap_or("")
            );
            let _ = stderr_tx.send(Bytes::from(msg.clone())).await;
            ExecExitInfo {
                exit_code: 127,
                command_not_found: detect_command_not_found(msg.as_bytes()),
            }
        }
    };

    let _ = exited_tx.send(exit);
}

pub struct FailingContainerRuntime;

impl ContainerRuntime for FailingContainerRuntime {
    async fn create_and_start(
        &self,
        _config: ContainerConfig,
    ) -> Result<ContainerInfo, AgentError> {
        Err(AgentError::Runtime {
            detail: "mock runtime failure".into(),
        })
    }

    async fn stop_and_remove(
        &self,
        _id: &ContainerId,
        _timeout: Duration,
    ) -> Result<(), AgentError> {
        Err(AgentError::Runtime {
            detail: "mock runtime failure".into(),
        })
    }

    async fn list_sandbox_containers(&self) -> Result<Vec<ContainerInfo>, AgentError> {
        Ok(Vec::new())
    }

    async fn start_exec(
        &self,
        _id: &ContainerId,
        _start: ExecStart,
    ) -> Result<ExecHandle, AgentError> {
        Err(AgentError::Runtime {
            detail: "mock runtime failure".into(),
        })
    }

    async fn signal_exec(
        &self,
        _id: &ContainerId,
        _in_container_pid: i32,
        _signum: i32,
    ) -> Result<(), AgentError> {
        Err(AgentError::Runtime {
            detail: "mock runtime failure".into(),
        })
    }

    async fn read_file(
        &self,
        _id: &ContainerId,
        _path: &str,
        _cwd: Option<&str>,
    ) -> Result<Bytes, AgentError> {
        Err(AgentError::Runtime {
            detail: "mock runtime failure".into(),
        })
    }

    async fn write_file(
        &self,
        _id: &ContainerId,
        _path: &str,
        _cwd: Option<&str>,
        _content: Bytes,
    ) -> Result<(), AgentError> {
        Err(AgentError::Runtime {
            detail: "mock runtime failure".into(),
        })
    }

    async fn write_files_targz(
        &self,
        _id: &ContainerId,
        _cwd: Option<&str>,
        _tarball: Bytes,
    ) -> Result<(), AgentError> {
        Err(AgentError::Runtime {
            detail: "mock runtime failure".into(),
        })
    }
}

pub struct MockHttpClient {
    response: ForwardResponse,
    called: AtomicUsize,
}

impl MockHttpClient {
    pub fn new(response: ForwardResponse) -> Self {
        Self {
            response,
            called: AtomicUsize::new(0),
        }
    }

    pub fn was_called(&self) -> bool {
        self.called.load(Ordering::SeqCst) > 0
    }
}

impl HttpClient for MockHttpClient {
    async fn send(
        &self,
        _port: u16,
        _request: ForwardRequest,
    ) -> Result<ForwardResponse, AgentError> {
        self.called.fetch_add(1, Ordering::SeqCst);
        Ok(self.response.clone())
    }
}

pub fn mock_container_info(sandbox_id: SandboxId, port: u16) -> ContainerInfo {
    ContainerInfo {
        id: ContainerId(format!("existing-{}", sandbox_id)),
        sandbox_id,
        host_port: port,
        running: true,
    }
}

pub fn start_cmd(sandbox_id: &SandboxId, image: &str) -> StartSandbox {
    StartSandbox {
        sandbox_id: sandbox_id.to_string(),
        image: image.into(),
        config: Some(SandboxConfig {
            cpu_limit_millicores: 1000,
            memory_limit_bytes: 512_000_000,
            env_vars: HashMap::new(),
            exposed_port: 8080,
            // v1.0.2: proto3 default 0 == UNSPECIFIED, which the
            // agent collapses to IfNotPresent — same wire shape an
            // older client (that doesn't know the field) sends.
            pull_policy: 0,
        }),
    }
}
