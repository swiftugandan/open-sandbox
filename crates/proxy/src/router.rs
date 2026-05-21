use std::sync::Arc;

use open_sandbox_contracts::error::ProxyError;
use open_sandbox_contracts::proxy::HttpResponse;

use crate::routing_cache::RoutingCache;
use crate::routing_store::RoutingStore;
use crate::stream_mux::StreamMux;

pub struct Router<S: RoutingStore> {
    cache: Arc<RoutingCache<S>>,
    mux: Arc<StreamMux>,
}

impl<S: RoutingStore> Router<S> {
    pub fn new(cache: Arc<RoutingCache<S>>, mux: Arc<StreamMux>) -> Self {
        Self { cache, mux }
    }

    pub fn extract_sandbox_id(host: &str) -> Option<String> {
        let host = host.split(':').next().unwrap_or(host);
        let first_dot = host.find('.')?;
        let subdomain = &host[..first_dot];
        if subdomain.len() != 12 || !subdomain.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        Some(subdomain.to_string())
    }

    pub async fn route_request(
        &self,
        host: &str,
        method: String,
        uri: String,
        headers: std::collections::HashMap<String, String>,
        body: Vec<u8>,
    ) -> Result<HttpResponse, ProxyError> {
        let subdomain = Self::extract_sandbox_id(host).ok_or_else(|| ProxyError::RoutingMiss {
            sandbox_id: host.to_string(),
        })?;

        let route = self
            .cache
            .lookup(&subdomain)
            .ok_or_else(|| ProxyError::RoutingMiss {
                sandbox_id: subdomain.clone(),
            })?;

        self.mux
            .send_request(
                &route.agent_id,
                &route.sandbox_id.to_string(),
                method,
                uri,
                headers,
                body,
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;
    use crate::tunnel_pool::TunnelPool;
    use open_sandbox_contracts::types::{AgentId, SandboxId};
    use tokio::sync::mpsc;

    #[test]
    fn extract_sandbox_id_from_valid_host() {
        let id =
            Router::<InMemoryRoutingStore>::extract_sandbox_id("abc123def456.sandbox.example.com");
        assert_eq!(id, Some("abc123def456".to_string()));
    }

    #[test]
    fn extract_sandbox_id_returns_none_for_bare_domain() {
        let id = Router::<InMemoryRoutingStore>::extract_sandbox_id("sandbox.example.com");
        assert!(id.is_none());
    }

    #[test]
    fn extract_sandbox_id_returns_none_for_empty() {
        let id = Router::<InMemoryRoutingStore>::extract_sandbox_id("");
        assert!(id.is_none());
    }

    #[test]
    fn extract_sandbox_id_handles_port() {
        let id = Router::<InMemoryRoutingStore>::extract_sandbox_id(
            "abc123def456.sandbox.example.com:443",
        );
        assert_eq!(id, Some("abc123def456".to_string()));
    }

    #[tokio::test]
    async fn route_request_returns_routing_miss_for_unknown_sandbox() {
        let store = InMemoryRoutingStore::new();
        let cache = Arc::new(RoutingCache::new(store));
        let pool = Arc::new(TunnelPool::new());
        let mux = Arc::new(StreamMux::new(pool));
        let router = Router::new(cache, mux);

        let result = router
            .route_request(
                "unknown123456.sandbox.example.com",
                "GET".into(),
                "/".into(),
                Default::default(),
                vec![],
            )
            .await;

        assert!(matches!(result, Err(ProxyError::RoutingMiss { .. })));
    }

    #[tokio::test]
    async fn route_request_returns_tunnel_unavailable_when_agent_not_connected() {
        let store = InMemoryRoutingStore::new();
        let sandbox_id = SandboxId::new();
        let agent_id = AgentId::new();
        store.add_entry(sandbox_id.clone(), agent_id.clone());

        let cache = Arc::new(RoutingCache::new(store));
        cache.refresh().await.unwrap();

        let pool = Arc::new(TunnelPool::new());
        let mux = Arc::new(StreamMux::new(pool));
        let router = Router::new(cache, mux);

        let host = format!("{}.sandbox.example.com", sandbox_id.subdomain());
        let result = router
            .route_request(&host, "GET".into(), "/".into(), Default::default(), vec![])
            .await;

        assert!(matches!(result, Err(ProxyError::TunnelUnavailable { .. })));
    }

    #[tokio::test]
    async fn route_request_forwards_to_agent_and_returns_response() {
        let store = InMemoryRoutingStore::new();
        let sandbox_id = SandboxId::new();
        let agent_id = AgentId::new();
        store.add_entry(sandbox_id.clone(), agent_id.clone());

        let cache = Arc::new(RoutingCache::new(store));
        cache.refresh().await.unwrap();

        let pool = Arc::new(TunnelPool::new());
        let (tx, mut rx) = mpsc::channel(32);
        pool.register(agent_id.clone(), tx);

        let mux = Arc::new(StreamMux::new(pool));
        let router = Router::new(cache, mux.clone());

        let host = format!("{}.sandbox.example.com", sandbox_id.subdomain());
        let handle = {
            let host = host.clone();
            tokio::spawn(async move {
                router
                    .route_request(
                        &host,
                        "GET".into(),
                        "/index.html".into(),
                        Default::default(),
                        vec![],
                    )
                    .await
            })
        };

        let req = rx.recv().await.unwrap();
        let response = HttpResponse {
            status_code: 200,
            headers: Default::default(),
            body: b"<html>hello</html>".to_vec(),
        };
        mux.deliver_response(&req.stream_id, response);

        let result = handle.await.unwrap().unwrap();
        assert_eq!(result.status_code, 200);
        assert_eq!(result.body, b"<html>hello</html>");
    }
}
