use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::oneshot;

use open_sandbox_contracts::constants::UPSTREAM_TIMEOUT;
use open_sandbox_contracts::error::ProxyError;
use open_sandbox_contracts::proxy::{HttpRequest, HttpResponse, TunnelRequest, tunnel_request};
use open_sandbox_contracts::types::AgentId;

use crate::tunnel_pool::TunnelPool;

pub struct PendingStream {
    pub response_tx: oneshot::Sender<Result<HttpResponse, ProxyError>>,
    pub agent_id: AgentId,
}

pub struct StreamMux {
    pool: Arc<TunnelPool>,
    pending: Mutex<HashMap<String, PendingStream>>,
    stream_counter: AtomicU64,
}

impl StreamMux {
    pub fn new(pool: Arc<TunnelPool>) -> Self {
        Self {
            pool,
            pending: Mutex::new(HashMap::new()),
            stream_counter: AtomicU64::new(0),
        }
    }

    pub fn next_stream_id(&self) -> String {
        let id = self.stream_counter.fetch_add(1, Ordering::SeqCst);
        format!("s-{id}")
    }

    pub async fn send_request(
        &self,
        agent_id: &AgentId,
        sandbox_id: &str,
        method: String,
        uri: String,
        headers: HashMap<String, String>,
        body: Vec<u8>,
    ) -> Result<HttpResponse, ProxyError> {
        let sender =
            self.pool
                .get_sender(agent_id)
                .ok_or_else(|| ProxyError::TunnelUnavailable {
                    agent_id: agent_id.to_string(),
                })?;

        let stream_id = self.next_stream_id();
        let (response_tx, response_rx) = oneshot::channel();

        self.pending.lock().unwrap().insert(
            stream_id.clone(),
            PendingStream {
                response_tx,
                agent_id: agent_id.clone(),
            },
        );

        let request = TunnelRequest {
            stream_id: stream_id.clone(),
            payload: Some(tunnel_request::Payload::HttpRequest(HttpRequest {
                method,
                uri,
                headers,
                body,
                sandbox_id: sandbox_id.to_string(),
            })),
        };

        sender.send(request).await.map_err(|_| {
            self.pending.lock().unwrap().remove(&stream_id);
            ProxyError::TunnelUnavailable {
                agent_id: agent_id.to_string(),
            }
        })?;

        match tokio::time::timeout(UPSTREAM_TIMEOUT, response_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(ProxyError::TunnelUnavailable {
                agent_id: agent_id.to_string(),
            }),
            Err(_) => {
                self.pending.lock().unwrap().remove(&stream_id);
                Err(ProxyError::UpstreamTimeout {
                    sandbox_id: sandbox_id.to_string(),
                    timeout_ms: UPSTREAM_TIMEOUT.as_millis() as u64,
                })
            }
        }
    }

    /// Deliver a response on the given stream. Comp-2 A2: requires
    /// `from_agent` to match the stream's owning agent — a malicious or
    /// confused agent cannot inject responses into another agent's session.
    /// Returns true if the response was forwarded; false if the stream is
    /// unknown OR the carrier agent doesn't own the stream.
    pub fn deliver_response(
        &self,
        stream_id: &str,
        from_agent: &AgentId,
        response: HttpResponse,
    ) -> bool {
        let pending = {
            let mut guard = self.pending.lock().unwrap();
            match guard.get(stream_id) {
                Some(p) if p.agent_id == *from_agent => guard.remove(stream_id),
                Some(_) => {
                    tracing::warn!(
                        stream_id = %stream_id,
                        carrier = %from_agent,
                        "deliver_response: carrier agent does not own stream; dropping"
                    );
                    return false;
                }
                None => return false,
            }
        };
        match pending {
            Some(p) => p.response_tx.send(Ok(response)).is_ok(),
            None => false,
        }
    }

    pub fn fail_stream(&self, stream_id: &str, from_agent: &AgentId, reason: String) {
        let pending = {
            let mut guard = self.pending.lock().unwrap();
            match guard.get(stream_id) {
                Some(p) if p.agent_id == *from_agent => guard.remove(stream_id),
                Some(_) => {
                    tracing::warn!(
                        stream_id = %stream_id,
                        carrier = %from_agent,
                        "fail_stream: carrier agent does not own stream; dropping"
                    );
                    return;
                }
                None => return,
            }
        };
        if let Some(pending) = pending {
            let _ = pending.response_tx.send(Err(ProxyError::UpstreamRejected {
                stream_id: stream_id.to_string(),
                reason,
            }));
        }
    }

    pub fn cancel_stream(&self, stream_id: &str) {
        self.pending.lock().unwrap().remove(stream_id);
    }

    pub fn cancel_agent_streams(&self, agent_id: &AgentId) {
        self.pending
            .lock()
            .unwrap()
            .retain(|_, p| p.agent_id != *agent_id);
    }

    pub fn pending_count(&self) -> usize {
        self.pending.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    async fn send_request_emits_tunnel_request() {
        let pool = Arc::new(TunnelPool::new());
        let agent_id = AgentId::new();
        let (tx, mut rx) = mpsc::channel::<TunnelRequest>(32);

        pool.register(agent_id.clone(), tx);
        let mux = Arc::new(StreamMux::new(pool));

        let mux_clone = mux.clone();
        let agent_id_clone = agent_id.clone();
        let _handle = tokio::spawn(async move {
            mux_clone
                .send_request(
                    &agent_id_clone,
                    "sandbox-1",
                    "GET".into(),
                    "/hello".into(),
                    Default::default(),
                    vec![],
                )
                .await
        });

        let req = rx.recv().await.unwrap();
        assert!(matches!(
            req.payload,
            Some(tunnel_request::Payload::HttpRequest(_))
        ));
        if let Some(tunnel_request::Payload::HttpRequest(http)) = req.payload {
            assert_eq!(http.method, "GET");
            assert_eq!(http.uri, "/hello");
            assert_eq!(http.sandbox_id, "sandbox-1");
        }

        let response = HttpResponse {
            status_code: 200,
            headers: Default::default(),
            body: b"ok".to_vec(),
        };
        mux.deliver_response(&req.stream_id, &agent_id, response);
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
        assert!(mux.deliver_response(&stream_id, &agent_id, response));

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
        assert!(!mux.deliver_response("nonexistent", &AgentId::new(), response));
    }

    #[tokio::test]
    async fn deliver_response_from_wrong_agent_is_dropped() {
        // Comp-2 A2: a response delivered by a different agent than the
        // pending stream's owner must NOT complete the request.
        let pool = Arc::new(TunnelPool::new());
        let owner = AgentId::new();
        let attacker = AgentId::new();
        let (tx, mut rx) = mpsc::channel::<TunnelRequest>(32);
        pool.register(owner.clone(), tx);
        let mux = Arc::new(StreamMux::new(pool));

        let mux_clone = mux.clone();
        let owner_clone = owner.clone();
        let handle = tokio::spawn(async move {
            mux_clone
                .send_request(
                    &owner_clone,
                    "sandbox-1",
                    "GET".into(),
                    "/".into(),
                    Default::default(),
                    vec![],
                )
                .await
        });

        let req = rx.recv().await.unwrap();
        let response = HttpResponse {
            status_code: 200,
            headers: Default::default(),
            body: vec![],
        };
        assert!(!mux.deliver_response(&req.stream_id, &attacker, response));
        // Cleanup so the test doesn't hang on UPSTREAM_TIMEOUT.
        mux.cancel_stream(&req.stream_id);
        let _ = handle.await;
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

    #[tokio::test]
    async fn fail_stream_returns_upstream_rejected_error() {
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
        mux.fail_stream(&req.stream_id, &agent_id, "container not ready".into());

        let result = handle.await.unwrap();
        assert!(matches!(result, Err(ProxyError::UpstreamRejected { .. })));
    }
}
