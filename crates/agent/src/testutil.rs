use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use open_sandbox_contracts::controller::{SandboxConfig, StartSandbox};
use open_sandbox_contracts::error::AgentError;
use open_sandbox_contracts::types::SandboxId;

use crate::container::{ContainerConfig, ContainerId, ContainerInfo, ContainerRuntime};
use crate::tunnel::{ForwardRequest, ForwardResponse, HttpClient};

pub struct MockContainerRuntime {
    created: AtomicUsize,
    stopped: AtomicUsize,
    port_counter: AtomicUsize,
    existing: Mutex<Vec<ContainerInfo>>,
}

impl MockContainerRuntime {
    pub fn new() -> Self {
        Self {
            created: AtomicUsize::new(0),
            stopped: AtomicUsize::new(0),
            port_counter: AtomicUsize::new(9000),
            existing: Mutex::new(Vec::new()),
        }
    }

    pub fn with_existing(containers: Vec<ContainerInfo>) -> Self {
        Self {
            created: AtomicUsize::new(0),
            stopped: AtomicUsize::new(0),
            port_counter: AtomicUsize::new(9000),
            existing: Mutex::new(containers),
        }
    }

    pub fn created_count(&self) -> usize {
        self.created.load(Ordering::SeqCst)
    }

    pub fn stopped_count(&self) -> usize {
        self.stopped.load(Ordering::SeqCst)
    }
}

impl ContainerRuntime for MockContainerRuntime {
    async fn create_and_start(
        &self,
        config: ContainerConfig,
    ) -> Result<ContainerInfo, AgentError> {
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
}

pub struct FailingContainerRuntime;

impl ContainerRuntime for FailingContainerRuntime {
    async fn create_and_start(
        &self,
        _config: ContainerConfig,
    ) -> Result<ContainerInfo, AgentError> {
        Err(AgentError::Docker {
            detail: "mock docker failure".into(),
        })
    }

    async fn stop_and_remove(
        &self,
        _id: &ContainerId,
        _timeout: Duration,
    ) -> Result<(), AgentError> {
        Err(AgentError::Docker {
            detail: "mock docker failure".into(),
        })
    }

    async fn list_sandbox_containers(&self) -> Result<Vec<ContainerInfo>, AgentError> {
        Ok(Vec::new())
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
        }),
    }
}
