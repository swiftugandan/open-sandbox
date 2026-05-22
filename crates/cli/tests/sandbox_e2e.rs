use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use open_sandbox_agent_docker::DockerRuntime;
use open_sandbox_agent::container::{ContainerConfig, ContainerRuntime};
use open_sandbox_agent::sandbox::SandboxManager;
use open_sandbox_contracts::controller::{SandboxConfig, StartSandbox};
use open_sandbox_contracts::types::SandboxId;

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_lifecycle_create_exec_stop() {
    let runtime = Arc::new(DockerRuntime::connect().unwrap());
    let sandbox_manager = Arc::new(SandboxManager::new(runtime.clone()));

    let sandbox_id = SandboxId::new();
    let start_cmd = StartSandbox {
        sandbox_id: sandbox_id.to_string(),
        image: "nginx:alpine".into(),
        config: Some(SandboxConfig {
            cpu_limit_millicores: 1000,
            memory_limit_bytes: 512 * 1024 * 1024,
            env_vars: HashMap::new(),
            exposed_port: 80,
        }),
    };

    let state = sandbox_manager.start_sandbox(start_cmd).await.unwrap();
    assert_eq!(
        state,
        open_sandbox_contracts::controller::SandboxState::Running
    );

    let output = sandbox_manager
        .exec_sandbox(
            &sandbox_id,
            vec!["echo".into(), "hello".into()],
            vec![],
        )
        .await
        .unwrap();
    assert_eq!(output.exit_code, 0);
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "hello");

    let entry = sandbox_manager.get_sandbox(&sandbox_id).unwrap();
    let _ = runtime
        .stop_and_remove(&entry.container_id, Duration::from_secs(5))
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_list_and_reconcile() {
    let runtime = Arc::new(DockerRuntime::connect().unwrap());
    let sandbox_manager = Arc::new(SandboxManager::new(runtime.clone()));
    let sandbox_id = SandboxId::new();

    let config = ContainerConfig {
        sandbox_id: sandbox_id.clone(),
        image: "nginx:alpine".into(),
        cpu_limit_millicores: 500,
        memory_limit_bytes: 256 * 1024 * 1024,
        env_vars: HashMap::new(),
        exposed_port: 80,
    };
    let info = runtime.create_and_start(config).await.unwrap();

    // Fresh manager that doesn't know about the container
    let fresh_manager = SandboxManager::new(runtime.clone());
    assert!(fresh_manager.list_sandboxes().is_empty());

    let reconciled = fresh_manager.reconcile().await.unwrap();
    assert!(reconciled.iter().any(|e| e.sandbox_id == sandbox_id));
    assert!(!fresh_manager.list_sandboxes().is_empty());

    let _ = runtime
        .stop_and_remove(&info.id, Duration::from_secs(5))
        .await;
}
