use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};

use open_sandbox_contracts::proxy::tunnel_request;
use open_sandbox_contracts::proxy::{
    HttpResponse, StreamClose, TunnelReady, TunnelResponse, tunnel_response,
    tunnel_service_client::TunnelServiceClient,
};
use open_sandbox_contracts::types::{AgentId, SandboxId};

use open_sandbox_proxy::grpc::tunnel_service;
use open_sandbox_proxy::http_server::HttpServer;
use open_sandbox_proxy::router::Router;
use open_sandbox_proxy::routing_cache::RoutingCache;
use open_sandbox_proxy::stream_mux::StreamMux;
use open_sandbox_proxy::testutil::InMemoryRoutingStore;
use open_sandbox_proxy::tunnel_pool::TunnelPool;

async fn start_proxy(pool: Arc<TunnelPool>, mux: Arc<StreamMux>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = format!("http://{}", listener.local_addr().unwrap());

    let service = tunnel_service(mux, pool);
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(service)
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .unwrap();
    });

    addr
}

async fn connect_agent(
    addr: &str,
    agent_id: &AgentId,
) -> (
    mpsc::Sender<TunnelResponse>,
    tonic::Streaming<open_sandbox_contracts::proxy::TunnelRequest>,
) {
    let channel = tonic::transport::Channel::from_shared(addr.to_string())
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut client = TunnelServiceClient::new(channel);

    let (outbound_tx, outbound_rx) = mpsc::channel(32);
    let outbound = ReceiverStream::new(outbound_rx);
    let response = client.open_tunnel(outbound).await.unwrap();
    let inbound = response.into_inner();

    let ready = TunnelResponse {
        stream_id: String::new(),
        payload: Some(tunnel_response::Payload::Ready(TunnelReady {
            agent_id: agent_id.to_string(),
        })),
    };
    outbound_tx.send(ready).await.unwrap();

    (outbound_tx, inbound)
}

#[tokio::test(flavor = "multi_thread")]
async fn full_request_routing_through_mock_agent() {
    let store = InMemoryRoutingStore::new();
    let sandbox_id = SandboxId::new();
    let agent_id = AgentId::new();
    store.add_entry(sandbox_id.clone(), agent_id.clone());

    let cache = Arc::new(RoutingCache::new(store));
    cache.refresh().await.unwrap();

    let pool = Arc::new(TunnelPool::new());
    let mux = Arc::new(StreamMux::new(pool.clone()));

    let addr = start_proxy(pool.clone(), mux.clone()).await;

    let (agent_tx, mut agent_rx) = connect_agent(&addr, &agent_id).await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let router = Router::new(cache, mux.clone());
    let host = format!("{}.sandbox.example.com", sandbox_id.subdomain());

    let route_handle = tokio::spawn(async move {
        router
            .route_request(
                &host,
                "GET".into(),
                "/hello".into(),
                Default::default(),
                vec![],
            )
            .await
    });

    let req = agent_rx.message().await.unwrap().unwrap();
    let stream_id = req.stream_id.clone();
    if let Some(tunnel_request::Payload::HttpRequest(http_req)) = req.payload {
        assert_eq!(http_req.method, "GET");
        assert_eq!(http_req.uri, "/hello");
    } else {
        panic!("expected HttpRequest payload");
    }

    let response = TunnelResponse {
        stream_id,
        payload: Some(tunnel_response::Payload::HttpResponse(HttpResponse {
            status_code: 200,
            headers: Default::default(),
            body: b"hello world".to_vec(),
        })),
    };
    agent_tx.send(response).await.unwrap();

    let result = route_handle.await.unwrap().unwrap();
    assert_eq!(result.status_code, 200);
    assert_eq!(result.body, b"hello world");
}

#[tokio::test(flavor = "multi_thread")]
async fn multiple_concurrent_requests_through_same_tunnel() {
    let store = InMemoryRoutingStore::new();
    let sandbox_id = SandboxId::new();
    let agent_id = AgentId::new();
    store.add_entry(sandbox_id.clone(), agent_id.clone());

    let cache = Arc::new(RoutingCache::new(store));
    cache.refresh().await.unwrap();

    let pool = Arc::new(TunnelPool::new());
    let mux = Arc::new(StreamMux::new(pool.clone()));

    let addr = start_proxy(pool.clone(), mux.clone()).await;

    let (agent_tx, mut agent_rx) = connect_agent(&addr, &agent_id).await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let router = Arc::new(Router::new(cache, mux.clone()));

    let host = format!("{}.sandbox.example.com", sandbox_id.subdomain());

    let r1 = {
        let router = router.clone();
        let host = host.clone();
        tokio::spawn(async move {
            router
                .route_request(&host, "GET".into(), "/a".into(), Default::default(), vec![])
                .await
        })
    };

    let r2 = {
        let router = router.clone();
        let host = host.clone();
        tokio::spawn(async move {
            router
                .route_request(&host, "GET".into(), "/b".into(), Default::default(), vec![])
                .await
        })
    };

    let mut received = Vec::new();
    for _ in 0..2 {
        let req = agent_rx.message().await.unwrap().unwrap();
        received.push(req);
    }

    for req in &received {
        let response = TunnelResponse {
            stream_id: req.stream_id.clone(),
            payload: Some(tunnel_response::Payload::HttpResponse(HttpResponse {
                status_code: 200,
                headers: Default::default(),
                body: format!("resp-{}", req.stream_id).into_bytes(),
            })),
        };
        agent_tx.send(response).await.unwrap();
    }

    let res1 = r1.await.unwrap().unwrap();
    let res2 = r2.await.unwrap().unwrap();
    assert_eq!(res1.status_code, 200);
    assert_eq!(res2.status_code, 200);
    assert_ne!(res1.body, res2.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn routing_miss_returns_error() {
    let store = InMemoryRoutingStore::new();
    let cache = Arc::new(RoutingCache::new(store));
    let pool = Arc::new(TunnelPool::new());
    let mux = Arc::new(StreamMux::new(pool));
    let router = Router::new(cache, mux);

    let result = router
        .route_request(
            "nonexistent12.sandbox.example.com",
            "GET".into(),
            "/".into(),
            Default::default(),
            vec![],
        )
        .await;

    assert!(matches!(
        result,
        Err(open_sandbox_contracts::error::ProxyError::RoutingMiss { .. })
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_disconnect_mid_request_returns_error() {
    let store = InMemoryRoutingStore::new();
    let sandbox_id = SandboxId::new();
    let agent_id = AgentId::new();
    store.add_entry(sandbox_id.clone(), agent_id.clone());

    let cache = Arc::new(RoutingCache::new(store));
    cache.refresh().await.unwrap();

    let pool = Arc::new(TunnelPool::new());
    let mux = Arc::new(StreamMux::new(pool.clone()));

    let addr = start_proxy(pool.clone(), mux.clone()).await;

    let (agent_tx, mut agent_rx) = connect_agent(&addr, &agent_id).await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let router = Router::new(cache, mux.clone());
    let host = format!("{}.sandbox.example.com", sandbox_id.subdomain());

    let route_handle = tokio::spawn(async move {
        router
            .route_request(&host, "GET".into(), "/".into(), Default::default(), vec![])
            .await
    });

    let _req = agent_rx.message().await.unwrap().unwrap();
    drop(agent_tx);
    drop(agent_rx);

    let result = route_handle.await.unwrap();
    assert!(result.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn two_agents_serve_different_sandboxes() {
    let store = InMemoryRoutingStore::new();
    let sandbox_a = SandboxId::new();
    let sandbox_b = SandboxId::new();
    let agent_a = AgentId::new();
    let agent_b = AgentId::new();
    store.add_entry(sandbox_a.clone(), agent_a.clone());
    store.add_entry(sandbox_b.clone(), agent_b.clone());

    let cache = Arc::new(RoutingCache::new(store));
    cache.refresh().await.unwrap();

    let pool = Arc::new(TunnelPool::new());
    let mux = Arc::new(StreamMux::new(pool.clone()));

    let addr = start_proxy(pool.clone(), mux.clone()).await;

    let (tx_a, mut rx_a) = connect_agent(&addr, &agent_a).await;
    let (tx_b, mut rx_b) = connect_agent(&addr, &agent_b).await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let router = Arc::new(Router::new(cache, mux.clone()));

    let host_a = format!("{}.sandbox.example.com", sandbox_a.subdomain());
    let host_b = format!("{}.sandbox.example.com", sandbox_b.subdomain());

    let h_a = {
        let router = router.clone();
        tokio::spawn(async move {
            router
                .route_request(
                    &host_a,
                    "GET".into(),
                    "/a".into(),
                    Default::default(),
                    vec![],
                )
                .await
        })
    };

    let h_b = {
        let router = router.clone();
        tokio::spawn(async move {
            router
                .route_request(
                    &host_b,
                    "GET".into(),
                    "/b".into(),
                    Default::default(),
                    vec![],
                )
                .await
        })
    };

    let req_a = rx_a.message().await.unwrap().unwrap();
    let resp_a = TunnelResponse {
        stream_id: req_a.stream_id,
        payload: Some(tunnel_response::Payload::HttpResponse(HttpResponse {
            status_code: 200,
            headers: Default::default(),
            body: b"from-a".to_vec(),
        })),
    };
    tx_a.send(resp_a).await.unwrap();

    let req_b = rx_b.message().await.unwrap().unwrap();
    let resp_b = TunnelResponse {
        stream_id: req_b.stream_id,
        payload: Some(tunnel_response::Payload::HttpResponse(HttpResponse {
            status_code: 200,
            headers: Default::default(),
            body: b"from-b".to_vec(),
        })),
    };
    tx_b.send(resp_b).await.unwrap();

    let res_a = h_a.await.unwrap().unwrap();
    let res_b = h_b.await.unwrap().unwrap();
    assert_eq!(res_a.body, b"from-a");
    assert_eq!(res_b.body, b"from-b");
}

#[tokio::test(flavor = "multi_thread")]
async fn http_ingress_routes_through_grpc_tunnel_to_mock_agent() {
    let store = InMemoryRoutingStore::new();
    let sandbox_id = SandboxId::new();
    let agent_id = AgentId::new();
    store.add_entry(sandbox_id.clone(), agent_id.clone());

    let cache = Arc::new(RoutingCache::new(store));
    cache.refresh().await.unwrap();

    let pool = Arc::new(TunnelPool::new());
    let mux = Arc::new(StreamMux::new(pool.clone()));

    let grpc_addr = start_proxy(pool.clone(), mux.clone()).await;

    let (agent_tx, mut agent_rx) = connect_agent(&grpc_addr, &agent_id).await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let router = Arc::new(Router::new(cache, mux));
    let http_server = HttpServer::new(router);
    let http_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http_addr = http_listener.local_addr().unwrap();

    let server_handle = tokio::spawn(async move { http_server.run(http_listener).await });

    let agent_handle = tokio::spawn(async move {
        while let Ok(Some(req)) = agent_rx.message().await {
            if let Some(tunnel_request::Payload::HttpRequest(http_req)) = &req.payload {
                let body = format!("echoed: {} {}", http_req.method, http_req.uri);
                let response = TunnelResponse {
                    stream_id: req.stream_id.clone(),
                    payload: Some(tunnel_response::Payload::HttpResponse(HttpResponse {
                        status_code: 200,
                        headers: Default::default(),
                        body: body.into_bytes(),
                    })),
                };
                let _ = agent_tx.send(response).await;
            }
        }
    });

    let host = format!("{}.sandbox.example.com", sandbox_id.subdomain());
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap();

    let resp = client
        .get(format!("http://{http_addr}/test-path"))
        .header("host", &host)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "echoed: GET /test-path");

    server_handle.abort();
    agent_handle.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn stream_close_returns_502_immediately() {
    let store = InMemoryRoutingStore::new();
    let sandbox_id = SandboxId::new();
    let agent_id = AgentId::new();
    store.add_entry(sandbox_id.clone(), agent_id.clone());

    let cache = Arc::new(RoutingCache::new(store));
    cache.refresh().await.unwrap();

    let pool = Arc::new(TunnelPool::new());
    let mux = Arc::new(StreamMux::new(pool.clone()));

    let grpc_addr = start_proxy(pool.clone(), mux.clone()).await;

    let (agent_tx, mut agent_rx) = connect_agent(&grpc_addr, &agent_id).await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let router = Arc::new(Router::new(cache, mux));
    let http_server = HttpServer::new(router);
    let http_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http_addr = http_listener.local_addr().unwrap();

    let server_handle = tokio::spawn(async move { http_server.run(http_listener).await });

    let agent_handle = tokio::spawn(async move {
        if let Ok(Some(req)) = agent_rx.message().await {
            let close = TunnelResponse {
                stream_id: req.stream_id.clone(),
                payload: Some(tunnel_response::Payload::Close(StreamClose {
                    reason: "container not ready".into(),
                })),
            };
            let _ = agent_tx.send(close).await;
        }
    });

    let host = format!("{}.sandbox.example.com", sandbox_id.subdomain());
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap();

    let t = std::time::Instant::now();
    let resp = client
        .get(format!("http://{http_addr}/"))
        .header("host", &host)
        .send()
        .await
        .unwrap();
    let elapsed = t.elapsed();

    assert_eq!(resp.status(), 502);
    assert!(
        elapsed.as_millis() < 1000,
        "should return 502 fast, not wait for timeout; took {}ms",
        elapsed.as_millis()
    );

    server_handle.abort();
    agent_handle.abort();
}
