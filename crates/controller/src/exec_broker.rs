use std::collections::HashMap;
use std::sync::Mutex;

use tokio::sync::oneshot;

use open_sandbox_contracts::controller::ExecResult;
use open_sandbox_contracts::error::ControllerError;

pub struct ExecBroker {
    pending: Mutex<HashMap<String, oneshot::Sender<ExecResult>>>,
}

impl Default for ExecBroker {
    fn default() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
        }
    }
}

impl ExecBroker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, exec_id: String) -> oneshot::Receiver<ExecResult> {
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(exec_id, tx);
        rx
    }

    pub fn deliver(&self, result: ExecResult) -> Result<(), ControllerError> {
        let sender = self.pending.lock().unwrap().remove(&result.exec_id);
        match sender {
            Some(tx) => {
                let _ = tx.send(result);
                Ok(())
            }
            None => Err(ControllerError::Internal {
                detail: format!("no pending exec for id {}", result.exec_id),
            }),
        }
    }

    pub fn cancel(&self, exec_id: &str) {
        self.pending.lock().unwrap().remove(exec_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn register_and_deliver_completes_receiver() {
        let broker = ExecBroker::new();
        let rx = broker.register("exec-1".into());

        broker
            .deliver(ExecResult {
                sandbox_id: "sb-1".into(),
                exec_id: "exec-1".into(),
                exit_code: 0,
                stdout: b"ok".to_vec(),
                stderr: vec![],
            error: String::new(),
            })
            .unwrap();

        let result = rx.await.unwrap();
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stdout, b"ok");
    }

    #[tokio::test]
    async fn deliver_without_register_returns_error() {
        let broker = ExecBroker::new();
        let result = broker.deliver(ExecResult {
            sandbox_id: "sb-1".into(),
            exec_id: "unknown".into(),
            exit_code: 1,
            stdout: vec![],
            stderr: vec![],
            error: String::new(),
        });
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn cancel_removes_pending() {
        let broker = ExecBroker::new();
        let _rx = broker.register("exec-2".into());
        broker.cancel("exec-2");

        let result = broker.deliver(ExecResult {
            sandbox_id: "sb-1".into(),
            exec_id: "exec-2".into(),
            exit_code: 0,
            stdout: vec![],
            stderr: vec![],
            error: String::new(),
        });
        assert!(result.is_err());
    }
}
