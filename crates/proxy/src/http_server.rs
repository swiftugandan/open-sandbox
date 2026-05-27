use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use tokio::net::TcpListener;
use tokio::task::JoinSet;

use open_sandbox_contracts::error::ProxyError;

use crate::router::Router;
use crate::routing_store::RoutingStore;

/// Hard cap on the size of a public HTTP request body the proxy will buffer
/// before forwarding to the agent. Without this, a single 10 GiB POST OOMs
/// the proxy and takes down every tunnel. Comp-2 B6.
pub const MAX_REQUEST_BODY_BYTES: usize = 50 * 1024 * 1024;

/// Hop-by-hop headers per RFC 7230 §6.1; the proxy must strip these on each
/// hop rather than forwarding them. Comp-2 A5. The Connection header is
/// stripped separately along with any headers it names.
const HOP_BY_HOP_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

pub struct HttpServer<S: RoutingStore + 'static> {
    router: Arc<Router<S>>,
}

impl<S: RoutingStore + 'static> HttpServer<S> {
    pub fn new(router: Arc<Router<S>>) -> Self {
        Self { router }
    }

    /// Run the accept loop until cancelled externally (e.g. via task
    /// abort). Used by tests that drive their own lifetime. Production
    /// wiring uses `run_with_shutdown`, which exits cleanly on a
    /// shutdown signal and drains in-flight connections.
    pub async fn run(&self, listener: TcpListener) -> Result<(), ProxyError> {
        self.run_with_shutdown(listener, std::future::pending::<()>(), Duration::ZERO)
            .await
    }

    /// Accept loop + bounded in-flight drain.
    ///
    /// On `shutdown`, stops accepting new TCP connections and waits up
    /// to `drain_timeout` for in-flight HTTP responses to complete.
    /// Connections still in flight after the deadline are aborted with
    /// a warning (the peer sees a TCP RST). `drain_timeout == 0`
    /// disables the bounded wait and waits forever — used by tests
    /// where the future is `std::future::pending`.
    ///
    /// PLAN_12FACTOR.md Phase 4 / code-review finding #2: without this
    /// drain, a SIGTERM that completed the proxy's gRPC drain would
    /// abort `http_handle` mid-response, killing long-running HTTP
    /// requests (SSE, large file transfer) with a TCP RST.
    pub async fn run_with_shutdown(
        &self,
        listener: TcpListener,
        shutdown: impl std::future::Future<Output = ()>,
        drain_timeout: Duration,
    ) -> Result<(), ProxyError> {
        tokio::pin!(shutdown);
        let mut connections: JoinSet<()> = JoinSet::new();
        loop {
            tokio::select! {
                accept = listener.accept() => {
                    let (stream, _) = accept.map_err(|e| ProxyError::Internal {
                        detail: e.to_string(),
                    })?;
                    let router = self.router.clone();
                    connections.spawn(async move {
                        let service = service_fn(move |req: Request<Incoming>| {
                            let router = router.clone();
                            async move { handle_request(router, req).await }
                        });
                        if let Err(e) = http1::Builder::new()
                            .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                            .await
                        {
                            tracing::warn!(error = %e, "http connection error");
                        }
                    });
                }
                // Reap completed connections so the JoinSet doesn't grow
                // unboundedly for long-lived servers. join_next() returns
                // Pending (never fires) when the set is empty.
                Some(_) = connections.join_next() => {}
                () = &mut shutdown => {
                    tracing::info!(
                        in_flight = connections.len(),
                        "http server: shutdown signaled, stopping accept"
                    );
                    break;
                }
            }
        }
        let in_flight_at_drain = connections.len();
        let drain_fut = async {
            while connections.join_next().await.is_some() {}
        };
        if drain_timeout.is_zero() {
            drain_fut.await;
        } else if tokio::time::timeout(drain_timeout, drain_fut).await.is_err() {
            tracing::warn!(
                remaining = connections.len(),
                started_with = in_flight_at_drain,
                timeout_secs = drain_timeout.as_secs(),
                "http server: drain timeout; aborting remaining connections"
            );
            connections.abort_all();
        } else {
            tracing::info!(
                drained = in_flight_at_drain,
                "http server: clean drain"
            );
        }
        Ok(())
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
    let uri = req
        .uri()
        .path_and_query()
        .map_or("/".to_string(), |pq| pq.to_string());

    // Comp-2 A5: strip hop-by-hop headers per RFC 7230 §6.1 before forwarding,
    // including any header named by Connection. Forwarding Transfer-Encoding /
    // Upgrade / Proxy-Authorization enables request smuggling and leaks the
    // proxy's credential to the in-sandbox app.
    let connection_headers: Vec<String> = req
        .headers()
        .get("connection")
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(',')
                .map(|s| s.trim().to_ascii_lowercase())
                .collect()
        })
        .unwrap_or_default();

    let mut headers = HashMap::new();
    for (name, value) in req.headers() {
        let lower = name.as_str().to_ascii_lowercase();
        if HOP_BY_HOP_HEADERS.contains(&lower.as_str()) {
            continue;
        }
        if connection_headers.iter().any(|h| *h == lower) {
            continue;
        }
        if let Ok(v) = value.to_str() {
            headers.insert(name.to_string(), v.to_string());
        }
    }

    // Comp-2 B6: cap the body size so a single oversized POST cannot OOM the
    // proxy. Per RFC 9110 §15.5.14, 413 is the right response.
    let body = match Limited::new(req.into_body(), MAX_REQUEST_BODY_BYTES)
        .collect()
        .await
    {
        Ok(collected) => collected.to_bytes().to_vec(),
        Err(_) => return Ok(payload_too_large()),
    };

    match router
        .route_request(&host, method, uri, headers, body)
        .await
    {
        Ok(resp) => {
            let status = u16::try_from(resp.status_code)
                .ok()
                .and_then(|code| StatusCode::from_u16(code).ok())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            let mut builder = Response::builder().status(status);
            for (k, v) in &resp.headers {
                builder = builder.header(k.as_str(), v.as_str());
            }
            Ok(builder
                .body(Full::new(Bytes::from(resp.body)))
                .expect("fresh builder"))
        }
        // Comp-2 C4: map ProxyError variants to distinct HTTP statuses so
        // consumers can distinguish 'no such sandbox' from 'agent dead' from
        // 'upstream slow'.
        Err(ProxyError::RoutingMiss { .. }) => Ok(not_found()),
        Err(ProxyError::UpstreamTimeout { .. }) => Ok(gateway_timeout()),
        Err(ProxyError::TunnelUnavailable { .. })
        | Err(ProxyError::UpstreamRejected { .. })
        | Err(_) => Ok(bad_gateway()),
    }
}

fn bad_gateway() -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::BAD_GATEWAY)
        .body(Full::new(Bytes::new()))
        .expect("fresh builder")
}

fn not_found() -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Full::new(Bytes::new()))
        .expect("fresh builder")
}

fn gateway_timeout() -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::GATEWAY_TIMEOUT)
        .body(Full::new(Bytes::new()))
        .expect("fresh builder")
}

fn payload_too_large() -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::PAYLOAD_TOO_LARGE)
        .body(Full::new(Bytes::new()))
        .expect("fresh builder")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routing_cache::RoutingCache;
    use crate::stream_mux::StreamMux;
    use crate::testutil::InMemoryRoutingStore;
    use crate::tunnel_pool::TunnelPool;
    use open_sandbox_contracts::proxy::{HttpResponse, TunnelRequest, tunnel_request};
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

        let server_handle = tokio::spawn(async move { server.run(listener).await });

        let mux_clone = mux.clone();
        let agent_id_clone = agent_id.clone();
        let agent_handle = tokio::spawn(async move {
            while let Some(req) = rx.recv().await {
                let response = HttpResponse {
                    status_code: 200,
                    headers: Default::default(),
                    body: b"hello from sandbox".to_vec(),
                };
                mux_clone.deliver_response(&req.stream_id, &agent_id_clone, response);
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
    async fn http_server_strips_hop_by_hop_headers() {
        // Comp-2 A5: hop-by-hop headers MUST NOT be forwarded to the agent.
        let sandbox_id = SandboxId::new();
        let agent_id = AgentId::new();
        let (router, mux, mut rx) = setup_router_with_agent(&sandbox_id, &agent_id);

        let server = HttpServer::new(router);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_handle = tokio::spawn(async move { server.run(listener).await });

        let captured = std::sync::Arc::new(std::sync::Mutex::new(None));
        let captured_clone = captured.clone();
        let mux_clone = mux.clone();
        let agent_id_clone = agent_id.clone();
        let agent_handle = tokio::spawn(async move {
            if let Some(req) = rx.recv().await {
                if let Some(tunnel_request::Payload::HttpRequest(http_req)) = &req.payload {
                    *captured_clone.lock().unwrap() = Some(http_req.headers.clone());
                }
                let response = HttpResponse {
                    status_code: 200,
                    headers: Default::default(),
                    body: vec![],
                };
                mux_clone.deliver_response(&req.stream_id, &agent_id_clone, response);
            }
        });

        tokio::time::sleep(Duration::from_millis(100)).await;

        let host = format!("{}.sandbox.example.com", sandbox_id.subdomain());
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let _ = client
            .post(format!("http://{addr}/"))
            .header("host", &host)
            .header("connection", "keep-alive, x-private")
            .header("proxy-authorization", "Bearer s3cret")
            .header("transfer-encoding", "chunked")
            .header("upgrade", "websocket")
            .header("x-private", "should-also-be-stripped")
            .header("x-safe", "passthrough")
            .body("hi")
            .send()
            .await
            .unwrap();

        agent_handle.await.unwrap();
        let captured = captured.lock().unwrap().clone().unwrap();
        for header in [
            "connection",
            "proxy-authorization",
            "transfer-encoding",
            "upgrade",
            "x-private", // named via Connection
        ] {
            assert!(
                !captured.keys().any(|k| k.eq_ignore_ascii_case(header)),
                "hop-by-hop header {header} must not be forwarded: {captured:?}"
            );
        }
        assert!(captured.keys().any(|k| k.eq_ignore_ascii_case("x-safe")));

        server_handle.abort();
    }

    #[tokio::test]
    async fn http_server_rejects_oversized_body_with_413() {
        // Comp-2 B6: bodies above MAX_REQUEST_BODY_BYTES get 413, not OOM.
        let sandbox_id = SandboxId::new();
        let agent_id = AgentId::new();
        let (router, _mux, _rx) = setup_router_with_agent(&sandbox_id, &agent_id);

        let server = HttpServer::new(router);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_handle = tokio::spawn(async move { server.run(listener).await });

        tokio::time::sleep(Duration::from_millis(100)).await;

        let oversize = vec![b'x'; MAX_REQUEST_BODY_BYTES + 1];
        let host = format!("{}.sandbox.example.com", sandbox_id.subdomain());
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let resp = client
            .post(format!("http://{addr}/"))
            .header("host", &host)
            .body(oversize)
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), 413);

        server_handle.abort();
    }

    #[tokio::test]
    async fn http_server_stops_accepting_on_shutdown_and_drains_in_flight() {
        // Code-review finding #2: shutdown future must stop the accept
        // loop AND wait for in-flight HTTP responses to finish their
        // current body before returning, so peers don't see a TCP RST.
        let sandbox_id = SandboxId::new();
        let agent_id = AgentId::new();
        let (router, mux, mut rx) = setup_router_with_agent(&sandbox_id, &agent_id);
        let server = HttpServer::new(router);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let server_handle = tokio::spawn(async move {
            server
                .run_with_shutdown(
                    listener,
                    async {
                        let _ = shutdown_rx.await;
                    },
                    Duration::from_secs(5),
                )
                .await
        });

        // Agent task that holds the request body open for 300ms before
        // responding — simulates a long-running response that must finish
        // even after shutdown signal arrives.
        let mux_clone = mux.clone();
        let agent_id_clone = agent_id.clone();
        let agent_handle = tokio::spawn(async move {
            if let Some(req) = rx.recv().await {
                tokio::time::sleep(Duration::from_millis(300)).await;
                let response = HttpResponse {
                    status_code: 200,
                    headers: Default::default(),
                    body: b"slow body finished".to_vec(),
                };
                mux_clone.deliver_response(&req.stream_id, &agent_id_clone, response);
            }
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        // Issue the request, then immediately signal shutdown. The
        // shutdown should NOT cancel the in-flight response.
        let host = format!("{}.sandbox.example.com", sandbox_id.subdomain());
        let req_handle = tokio::spawn({
            let host = host.clone();
            async move {
                let client = reqwest::Client::builder()
                    .timeout(Duration::from_secs(3))
                    .build()
                    .unwrap();
                client
                    .get(format!("http://{addr}/"))
                    .header("host", &host)
                    .send()
                    .await
                    .unwrap()
                    .bytes()
                    .await
                    .unwrap()
            }
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = shutdown_tx.send(());

        let body = req_handle.await.unwrap();
        assert_eq!(body.as_ref(), b"slow body finished");

        let server_result = tokio::time::timeout(Duration::from_secs(2), server_handle)
            .await
            .expect("server should exit cleanly after drain");
        server_result.unwrap().unwrap();
        agent_handle.abort();
    }

    #[tokio::test]
    async fn http_server_returns_404_for_unknown_sandbox() {
        // Comp-2 C4: RoutingMiss → 404 (was 502). Distinct from agent-dead
        // (502) so CDNs / health checks can stop retrying when the sandbox
        // genuinely doesn't exist.
        let store = InMemoryRoutingStore::new();
        let cache = Arc::new(RoutingCache::new(store));
        let pool = Arc::new(TunnelPool::new());
        let mux = Arc::new(StreamMux::new(pool));
        let router = Arc::new(Router::new(cache, mux));

        let server = HttpServer::new(router);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server_handle = tokio::spawn(async move { server.run(listener).await });

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

        assert_eq!(resp.status(), 404);

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

        let server_handle = tokio::spawn(async move { server.run(listener).await });

        let mux_clone = mux.clone();
        let agent_id_clone = agent_id.clone();
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
                mux_clone.deliver_response(&req.stream_id, &agent_id_clone, response);
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
