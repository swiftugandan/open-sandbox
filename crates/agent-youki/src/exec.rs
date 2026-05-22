use std::path::Path;

use open_sandbox_agent::container::{ExecOutput, detect_command_not_found};
use open_sandbox_contracts::error::AgentError;

use libcontainer::container::Container;

pub fn exec_in_container(
    container_id: &str,
    state_dir: &Path,
    command: Vec<String>,
    stdin_data: Vec<u8>,
    cwd: &str,
) -> Result<ExecOutput, AgentError> {
    let container_root = state_dir.join(container_id);
    let container = Container::load(container_root).map_err(|e| AgentError::Runtime {
        detail: format!("failed to load container for exec: {e}"),
    })?;
    let pid = container.pid().ok_or_else(|| AgentError::Runtime {
        detail: "container has no PID for exec".into(),
    })?;

    let mut cmd = std::process::Command::new("nsenter");
    cmd.arg("--target")
        .arg(pid.as_raw().to_string())
        .arg("--mount")
        .arg("--uts")
        .arg("--ipc")
        .arg("--net")
        .arg("--pid");

    if !cwd.is_empty() {
        // --wd= chdirs inside the target namespace, so the working directory
        // is resolved against the container's mount namespace, not the host.
        cmd.arg(format!("--wd={cwd}"));
    }

    cmd.arg("--")
        .args(&command)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    if !stdin_data.is_empty() {
        cmd.stdin(std::process::Stdio::piped());
    } else {
        cmd.stdin(std::process::Stdio::null());
    }

    let mut child = cmd.spawn().map_err(|e| AgentError::Runtime {
        detail: format!("failed to spawn nsenter: {e}"),
    })?;

    if !stdin_data.is_empty() {
        use std::io::Write;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(&stdin_data).map_err(|e| AgentError::Runtime {
                detail: format!("failed to write stdin data: {e}"),
            })?;
            drop(stdin);
        }
    }

    let output = child.wait_with_output().map_err(|e| AgentError::Runtime {
        detail: format!("exec process failed: {e}"),
    })?;

    let exit_code = output.status.code().unwrap_or(-1);
    let mut stdout = output.stdout;
    let mut stderr = output.stderr;
    let mut cnf = false;
    if exit_code == 127 {
        if detect_command_not_found(&stderr) {
            cnf = true;
        } else if detect_command_not_found(&stdout) {
            cnf = true;
            stderr.extend_from_slice(&stdout);
            stdout.clear();
        }
    }

    Ok(ExecOutput {
        exit_code,
        stdout,
        stderr,
        command_not_found: cnf,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires a running container — exercised by lib.rs integration tests"]
    fn exec_captures_stdout() {
        let result = exec_in_container(
            "test-container",
            Path::new("/tmp/test"),
            vec!["echo".into(), "hello".into()],
            vec![],
            "",
        );

        let output = result.unwrap();
        assert_eq!(output.stdout, b"hello\n");
        assert!(output.stderr.is_empty());
        assert_eq!(output.exit_code, 0);
    }

    #[test]
    #[ignore = "requires a running container — exercised by lib.rs integration tests"]
    fn exec_returns_nonzero_exit_code() {
        let result = exec_in_container(
            "test-container",
            Path::new("/tmp/test"),
            vec!["false".into()],
            vec![],
            "",
        );

        let output = result.unwrap();
        assert_ne!(output.exit_code, 0);
    }

    #[test]
    #[ignore = "requires a running container — exercised by lib.rs integration tests"]
    fn exec_pipes_stdin() {
        let result = exec_in_container(
            "test-container",
            Path::new("/tmp/test"),
            vec!["cat".into()],
            b"input data".to_vec(),
            "",
        );

        let output = result.unwrap();
        assert_eq!(output.stdout, b"input data");
    }
}
