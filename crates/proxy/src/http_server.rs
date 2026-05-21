use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use tokio::net::TcpListener;

use open_sandbox_contracts::error::ProxyError;

use crate::router::Router;
use crate::routing_store::RoutingStore;

pub struct HttpServer<S: RoutingStore + 'static> {
    router: Arc<Router<S>>,
}

impl<S: RoutingStore + 'static> HttpServer<S> {
    pub fn new(router: Arc<Router<S>>) -> Self {
        Self { router }
    }

    pub async fn run(&self, listener: TcpListener) -> Result<(), ProxyError> {
        loop {
            let (stream, _) = listener.accept().await.map_err(|e| ProxyError::Internal {
                detail: e.to_string(),
            })?;

            let router = self.router.clone();
            tokio::spawn(async move {
                let service = service_fn(move |req: Request<Incoming>| {
                    let router = router.clone();
                    async move { handle_request(router, req).await }
                });

                if let Err(e) = http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                    .await
                {
                    eprintln!("http connection error: {e}");
                }
            });
        }
    }
}

async fn handle_request<S: RoutingStore + 'static>(
    router: Arc<Router<S>>,
    req: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    let host = req
        .headers()
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let method = req.method().to_string();
    let uri = req.uri().path_and_query().map_or("/".to_string(), |pq| pq.to_string());

    let mut headers = HashMap::new();
    for (name, value) in req.headers() {
        if let Ok(v) = value.to_str() {
            headers.insert(name.to_string(), v.to_string());
        }
    }

    let body = req
        .collect()
        .await
        .map(|collected| collected.to_bytes().to_vec())
        .unwrap_or_default();

    match router.route_request(&host, method, uri, headers, body).await {
        Ok(resp) => {
            let status = StatusCode::from_u16(resp.status_code as u16)
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            let mut builder = Response::builder().status(status);
            for (k, v) in &resp.headers {
                builder = builder.header(k.as_str(), v.as_str());
            }
            Ok(builder
                .body(Full::new(Bytes::from(resp.body)))
                .unwrap_or_else(|_| {
                    Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .body(Full::new(Bytes::new()))
                        .unwrap()
                }))
        }
        Err(_) => Ok(Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .body(Full::new(Bytes::new()))
            .unwrap()),
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
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server_handle = tokio::spawn(async move {
            server.run(listener).await
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
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server_handle = tokio::spawn(async move {
            server.run(listener).await
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
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server_handle = tokio::spawn(async move {
            server.run(listener).await
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
