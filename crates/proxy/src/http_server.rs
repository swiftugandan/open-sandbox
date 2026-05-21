use std::net::SocketAddr;
use std::sync::Arc;

use open_sandbox_contracts::error::ProxyError;

use crate::router::Router;
use crate::routing_store::RoutingStore;

pub struct HttpServer<S: RoutingStore + 'static> {
    _router: Arc<Router<S>>,
}

impl<S: RoutingStore + 'static> HttpServer<S> {
    pub fn new(router: Arc<Router<S>>) -> Self {
        Self { _router: router }
    }

    pub async fn run(&self, _addr: SocketAddr) -> Result<(), ProxyError> {
        Err(ProxyError::Internal {
            detail: "HTTP server not yet implemented".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routing_cache::RoutingCache;
    use crate::stream_mux::StreamMux;
    use crate::testutil::InMemoryRoutingStore;
    use crate::tunnel_pool::TunnelPool;
    use open_sandbox_contracts::proxy::{tunnel_request, HttpResponse, TunnelRequest};
    use open_sandbox_contracts::types::{AgentId, SandboxId};
    use std::time::Duration;
    use tokio::sync::mpsc;

    fn setup_router_with_agent(
        sandbox_id: &SandboxId,
        agent_id: &AgentId,
    ) -> (
        Arc<Router<InMemoryRoutingStore>>,
        Arc<StreamMux>,
        mpsc::Receiver<TunnelRequest>,
    ) {
        let store = InMemoryRoutingStore::new();
        store.add_entry(sandbox_id.clone(), agent_id.clone());

        let cache = Arc::new(RoutingCache::new(store));
        cache.insert(sandbox_id.clone(), agent_id.clone());

        let pool = Arc::new(TunnelPool::new());
        let (tx, rx) = mpsc::channel(32);
        pool.register(agent_id.clone(), tx);

        let mux = Arc::new(StreamMux::new(pool));
        let router = Arc::new(Router::new(cache, mux.clone()));

        (router, mux, rx)
    }

    #[tokio::test]
    async fn http_server_routes_request_to_agent() {
        let sandbox_id = SandboxId::new();
        let agent_id = AgentId::new();
        let (router, mux, mut rx) = setup_router_with_agent(&sandbox_id, &agent_id);

        let server = HttpServer::new(router);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server_handle = tokio::spawn(async move {
            server.run(addr).await
        });

        let mux_clone = mux.clone();
        let agent_handle = tokio::spawn(async move {
            while let Some(req) = rx.recv().await {
                let response = HttpResponse {
                    status_code: 200,
                    headers: Default::default(),
                    body: b"hello from sandbox".to_vec(),
                };
                mux_clone.deliver_response(&req.stream_id, response);
            }
        });

        tokio::time::sleep(Duration::from_millis(100)).await;

        let host = format!("{}.sandbox.example.com", sandbox_id.subdomain());
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let resp = client
            .get(format!("http://{addr}/"))
            .header("host", &host)
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), 200);
        assert_eq!(resp.bytes().await.unwrap().as_ref(), b"hello from sandbox");

        server_handle.abort();
        agent_handle.abort();
    }

    #[tokio::test]
    async fn http_server_returns_502_for_unknown_sandbox() {
        let store = InMemoryRoutingStore::new();
        let cache = Arc::new(RoutingCache::new(store));
        let pool = Arc::new(TunnelPool::new());
        let mux = Arc::new(StreamMux::new(pool));
        let router = Arc::new(Router::new(cache, mux));

        let server = HttpServer::new(router);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server_handle = tokio::spawn(async move {
            server.run(addr).await
        });

        tokio::time::sleep(Duration::from_millis(100)).await;

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let resp = client
            .get(format!("http://{addr}/"))
            .header("host", "abc123def456.sandbox.example.com")
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), 502);

        server_handle.abort();
    }

    #[tokio::test]
    async fn http_server_forwards_method_and_headers() {
        let sandbox_id = SandboxId::new();
        let agent_id = AgentId::new();
        let (router, mux, mut rx) = setup_router_with_agent(&sandbox_id, &agent_id);

        let server = HttpServer::new(router);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server_handle = tokio::spawn(async move {
            server.run(addr).await
        });

        let mux_clone = mux.clone();
        let agent_handle = tokio::spawn(async move {
            if let Some(req) = rx.recv().await {
                if let Some(tunnel_request::Payload::HttpRequest(http_req)) = &req.payload {
                    assert_eq!(http_req.method, "POST");
                    assert_eq!(http_req.uri, "/api/data");
                    assert_eq!(
                        http_req.headers.get("x-custom").map(|s| s.as_str()),
                        Some("test-value")
                    );
                    assert_eq!(http_req.body, b"request body");
                }
                let response = HttpResponse {
                    status_code: 201,
                    headers: Default::default(),
                    body: b"created".to_vec(),
                };
                mux_clone.deliver_response(&req.stream_id, response);
            }
        });

        tokio::time::sleep(Duration::from_millis(100)).await;

        let host = format!("{}.sandbox.example.com", sandbox_id.subdomain());
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let resp = client
            .post(format!("http://{addr}/api/data"))
            .header("host", &host)
            .header("x-custom", "test-value")
            .body("request body")
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), 201);

        server_handle.abort();
        agent_handle.abort();
    }
}
