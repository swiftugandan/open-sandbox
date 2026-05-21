use std::sync::Arc;

use open_sandbox_contracts::error::AgentError;
use open_sandbox_contracts::types::SandboxId;

use crate::sandbox::SandboxManager;
use crate::container::ContainerRuntime;

pub trait HttpClient: Send + Sync {
    fn send(
        &self,
        port: u16,
        request: ForwardRequest,
    ) -> impl Future<Output = Result<ForwardResponse, AgentError>> + Send;
}

#[derive(Debug, Clone)]
pub struct ForwardRequest {
    pub method: String,
    pub uri: String,
    pub headers: std::collections::HashMap<String, String>,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct ForwardResponse {
    pub status_code: u32,
    pub headers: std::collections::HashMap<String, String>,
    pub body: Vec<u8>,
}

pub struct TunnelForwarder<R: ContainerRuntime, H: HttpClient> {
    sandbox_manager: Arc<SandboxManager<R>>,
    http_client: Arc<H>,
}

impl<R: ContainerRuntime, H: HttpClient> TunnelForwarder<R, H> {
    pub fn new(sandbox_manager: Arc<SandboxManager<R>>, http_client: Arc<H>) -> Self {
        Self {
            sandbox_manager,
            http_client,
        }
    }

    pub async fn forward(
        &self,
        sandbox_id: &SandboxId,
        request: ForwardRequest,
    ) -> Result<ForwardResponse, AgentError> {
        let port = self.sandbox_manager.host_port_for(sandbox_id)?;
        self.http_client.send(port, request).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container::ContainerRuntime;
    use crate::testutil::*;

    #[tokio::test]
    async fn forwards_http_request_to_local_port() {
        let runtime = Arc::new(MockContainerRuntime::new());
        let manager = Arc::new(SandboxManager::new(runtime));
        let sandbox_id = SandboxId::new();

        manager
            .start_sandbox(start_cmd(&sandbox_id, "nginx:latest"))
            .await
            .unwrap();

        let http_client = Arc::new(MockHttpClient::new(ForwardResponse {
            status_code: 200,
            headers: Default::default(),
            body: b"hello".to_vec(),
        }));
        let forwarder = TunnelForwarder::new(manager, http_client.clone());

        let response = forwarder
            .forward(
                &sandbox_id,
                ForwardRequest {
                    method: "GET".into(),
                    uri: "/".into(),
                    headers: Default::default(),
                    body: vec![],
                },
            )
            .await
            .unwrap();

        assert_eq!(response.status_code, 200);
        assert_eq!(response.body, b"hello");
        assert!(http_client.was_called());
    }

    #[tokio::test]
    async fn unknown_sandbox_returns_error() {
        let runtime = Arc::new(MockContainerRuntime::new());
        let manager = Arc::new(SandboxManager::new(runtime));
        let http_client = Arc::new(MockHttpClient::new(ForwardResponse {
            status_code: 200,
            headers: Default::default(),
            body: vec![],
        }));
        let forwarder = TunnelForwarder::new(manager, http_client);

        let result = forwarder
            .forward(
                &SandboxId::new(),
                ForwardRequest {
                    method: "GET".into(),
                    uri: "/".into(),
                    headers: Default::default(),
                    body: vec![],
                },
            )
            .await;

        assert!(matches!(result, Err(AgentError::SandboxNotFound { .. })));
    }
}
