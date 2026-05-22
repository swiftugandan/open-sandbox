use std::path::Path;

use open_sandbox_agent::container::ExecOutput;
use open_sandbox_contracts::error::AgentError;

pub fn exec_in_container(
    _container_id: &str,
    _root_dir: &Path,
    _command: Vec<String>,
    _stdin: Vec<u8>,
) -> Result<ExecOutput, AgentError> {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_captures_stdout() {
        // Requires a running container — this test validates the contract,
        // not the implementation. It will be exercised in e2e tests.
        let result = exec_in_container(
            "test-container",
            Path::new("/tmp/test"),
            vec!["echo".into(), "hello".into()],
            vec![],
        );

        // RED phase: todo!() panics
        let output = result.unwrap();
        assert_eq!(output.stdout, b"hello\n");
        assert!(output.stderr.is_empty());
        assert_eq!(output.exit_code, 0);
    }

    #[test]
    fn exec_returns_nonzero_exit_code() {
        let result = exec_in_container(
            "test-container",
            Path::new("/tmp/test"),
            vec!["false".into()],
            vec![],
        );

        let output = result.unwrap();
        assert_ne!(output.exit_code, 0);
    }

    #[test]
    fn exec_pipes_stdin() {
        let result = exec_in_container(
            "test-container",
            Path::new("/tmp/test"),
            vec!["cat".into()],
            b"input data".to_vec(),
        );

        let output = result.unwrap();
        assert_eq!(output.stdout, b"input data");
    }
}
