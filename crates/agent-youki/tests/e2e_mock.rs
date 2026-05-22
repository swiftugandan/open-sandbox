use std::collections::HashMap;
use std::sync::Arc;

use open_sandbox_agent::sandbox::SandboxManager;
use open_sandbox_agent_youki::{YoukiConfig, YoukiRuntime};
use open_sandbox_contracts::controller::{SandboxConfig, SandboxState, StartSandbox, StopSandbox};
use open_sandbox_contracts::types::SandboxId;
use serial_test::serial;

fn youki_config() -> YoukiConfig {
    YoukiConfig {
        root_dir: std::path::PathBuf::from("/tmp/youki-e2e"),
        cni_bin_path: std::path::PathBuf::from("/opt/cni/bin"),
    }
}

fn start_cmd(sandbox_id: &SandboxId, image: &str) -> StartSandbox {
    StartSandbox {
        sandbox_id: sandbox_id.to_string(),
        image: image.into(),
        config: Some(SandboxConfig {
            cpu_limit_millicores: 1000,
            memory_limit_bytes: 512_000_000,
            env_vars: HashMap::new(),
            exposed_port: 8080,
        }),
    }
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn sandbox_lifecycle_through_manager() {
    let runtime = Arc::new(YoukiRuntime::new(youki_config()).unwrap());
    let manager = SandboxManager::new(runtime);
    let sandbox_id = SandboxId::new();

    let state = manager
        .start_sandbox(start_cmd(&sandbox_id, "alpine:latest"))
        .await
        .unwrap();
    assert_eq!(state, SandboxState::Running);

    let entry = manager.get_sandbox(&sandbox_id).unwrap();
    assert!(entry.host_port > 0);

    let sandboxes = manager.list_sandboxes();
    assert!(sandboxes.iter().any(|s| s.sandbox_id == sandbox_id));

    let stop_state = manager
        .stop_sandbox(StopSandbox {
            sandbox_id: sandbox_id.to_string(),
            timeout_seconds: 5,
        })
        .await
        .unwrap();
    assert_eq!(stop_state, SandboxState::Stopped);

    assert!(manager.get_sandbox(&sandbox_id).is_none());
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn sandbox_exec_through_manager() {
    let runtime = Arc::new(YoukiRuntime::new(youki_config()).unwrap());
    let manager = SandboxManager::new(runtime);
    let sandbox_id = SandboxId::new();

    manager
        .start_sandbox(start_cmd(&sandbox_id, "alpine:latest"))
        .await
        .unwrap();

    let output = manager
        .exec_sandbox(&sandbox_id, vec!["echo".into(), "e2e-test".into()], vec![])
        .await
        .unwrap();

    assert_eq!(output.exit_code, 0);
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "e2e-test");

    manager
        .stop_sandbox(StopSandbox {
            sandbox_id: sandbox_id.to_string(),
            timeout_seconds: 5,
        })
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn sandbox_exec_with_stdin_through_manager() {
    let runtime = Arc::new(YoukiRuntime::new(youki_config()).unwrap());
    let manager = SandboxManager::new(runtime);
    let sandbox_id = SandboxId::new();

    manager
        .start_sandbox(start_cmd(&sandbox_id, "alpine:latest"))
        .await
        .unwrap();

    let output = manager
        .exec_sandbox(
            &sandbox_id,
            vec!["cat".into()],
            b"piped through manager".to_vec(),
        )
        .await
        .unwrap();

    assert_eq!(output.exit_code, 0);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "piped through manager"
    );

    manager
        .stop_sandbox(StopSandbox {
            sandbox_id: sandbox_id.to_string(),
            timeout_seconds: 5,
        })
        .await
        .unwrap();
}
