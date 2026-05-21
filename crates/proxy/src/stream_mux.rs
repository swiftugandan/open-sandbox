use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::oneshot;

use open_sandbox_contracts::error::ProxyError;
use open_sandbox_contracts::proxy::HttpResponse;
use open_sandbox_contracts::types::AgentId;

use crate::tunnel_pool::TunnelPool;

pub struct PendingStream {
    pub response_tx: oneshot::Sender<HttpResponse>,
}

pub struct StreamMux {
    pool: Arc<TunnelPool>,
    pending: Mutex<HashMap<String, PendingStream>>,
    stream_counter: std::sync::atomic::AtomicU64,
}

impl StreamMux {
    pub fn new(pool: Arc<TunnelPool>) -> Self {
        todo!()
    }

    pub fn next_stream_id(&self) -> String {
        todo!()
    }

    pub async fn send_request(
        &self,
        agent_id: &AgentId,
        sandbox_id: &str,
        method: String,
        uri: String,
        headers: std::collections::HashMap<String, String>,
        body: Vec<u8>,
    ) -> Result<HttpResponse, ProxyError> {
        todo!()
    }

    pub fn deliver_response(&self, stream_id: &str, response: HttpResponse) -> bool {
        todo!()
    }

    pub fn cancel_stream(&self, stream_id: &str) {
        todo!()
    }

    pub fn pending_count(&self) -> usize {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use open_sandbox_contracts::proxy::tunnel_request;
    use open_sandbox_contracts::proxy::TunnelRequest;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn next_stream_id_is_unique() {
        let pool = Arc::new(TunnelPool::new());
        let mux = StreamMux::new(pool);

        let id1 = mux.next_stream_id();
        let id2 = mux.next_stream_id();
        assert_ne!(id1, id2);
    }

    #[tokio::test]
    async fn send_request_fails_for_unknown_agent() {
        let pool = Arc::new(TunnelPool::new());
        let mux = StreamMux::new(pool);

        let result = mux
            .send_request(
                &AgentId::new(),
                "sandbox-1",
                "GET".into(),
                "/".into(),
                Default::default(),
                vec![],
            )
            .await;

        assert!(matches!(result, Err(ProxyError::TunnelUnavailable { .. })));
    }

    #[tokio::test]
    async fn send_request_and_receive_response() {
        let pool = Arc::new(TunnelPool::new());
        let agent_id = AgentId::new();
        let (tx, mut rx) = mpsc::channel::<TunnelRequest>(32);

        pool.register(agent_id.clone(), tx);
        let mux = StreamMux::new(pool);

        let mux_handle = {
            let agent_id = agent_id.clone();
            tokio::spawn(async move {
                mux.send_request(
                    &agent_id,
                    "sandbox-1",
                    "GET".into(),
                    "/hello".into(),
                    Default::default(),
                    vec![],
                )
                .await
            })
        };

        let req = rx.recv().await.unwrap();
        assert!(matches!(
            req.payload,
            Some(tunnel_request::Payload::HttpRequest(_))
        ));

        drop(rx);
        let result = mux_handle.await.unwrap();
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn deliver_response_completes_pending_request() {
        let pool = Arc::new(TunnelPool::new());
        let agent_id = AgentId::new();
        let (tx, mut rx) = mpsc::channel::<TunnelRequest>(32);

        pool.register(agent_id.clone(), tx);
        let mux = Arc::new(StreamMux::new(pool));

        let mux_clone = mux.clone();
        let agent_id_clone = agent_id.clone();
        let handle = tokio::spawn(async move {
            mux_clone
                .send_request(
                    &agent_id_clone,
                    "sandbox-1",
                    "GET".into(),
                    "/".into(),
                    Default::default(),
                    vec![],
                )
                .await
        });

        let req = rx.recv().await.unwrap();
        let stream_id = req.stream_id.clone();

        let response = HttpResponse {
            status_code: 200,
            headers: Default::default(),
            body: b"ok".to_vec(),
        };
        assert!(mux.deliver_response(&stream_id, response));

        let result = handle.await.unwrap().unwrap();
        assert_eq!(result.status_code, 200);
        assert_eq!(result.body, b"ok");
    }

    #[tokio::test]
    async fn deliver_response_returns_false_for_unknown_stream() {
        let pool = Arc::new(TunnelPool::new());
        let mux = StreamMux::new(pool);

        let response = HttpResponse {
            status_code: 200,
            headers: Default::default(),
            body: vec![],
        };
        assert!(!mux.deliver_response("nonexistent", response));
    }

    #[tokio::test]
    async fn cancel_stream_removes_pending() {
        let pool = Arc::new(TunnelPool::new());
        let agent_id = AgentId::new();
        let (tx, mut rx) = mpsc::channel::<TunnelRequest>(32);

        pool.register(agent_id.clone(), tx);
        let mux = Arc::new(StreamMux::new(pool));

        let mux_clone = mux.clone();
        let agent_id_clone = agent_id.clone();
        let handle = tokio::spawn(async move {
            mux_clone
                .send_request(
                    &agent_id_clone,
                    "sandbox-1",
                    "GET".into(),
                    "/".into(),
                    Default::default(),
                    vec![],
                )
                .await
        });

        let req = rx.recv().await.unwrap();
        let stream_id = req.stream_id.clone();

        mux.cancel_stream(&stream_id);
        assert_eq!(mux.pending_count(), 0);

        let result = handle.await.unwrap();
        assert!(result.is_err());
    }
}
