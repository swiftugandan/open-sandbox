use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::{Request, Response, Status, Streaming};

use open_sandbox_contracts::controller::controller_command;
use open_sandbox_contracts::controller::controller_service_server::{
    ControllerService, ControllerServiceServer,
};
use open_sandbox_contracts::controller::{
    AgentMessage, AgentResources, ControllerCommand, HeartbeatAck, RegisterResponse, SandboxConfig,
    SandboxState, StartSandbox, agent_message,
};
use open_sandbox_contracts::proxy::tunnel_request;
use open_sandbox_contracts::proxy::tunnel_response;
use open_sandbox_contracts::proxy::tunnel_service_server::{TunnelService, TunnelServiceServer};
use open_sandbox_contracts::proxy::{HttpRequest, TunnelRequest, TunnelResponse};
use open_sandbox_contracts::types::{AgentId, JoinToken, SandboxId};

use open_sandbox_agent::container::{
    ContainerConfig, ContainerId, ContainerInfo, ContainerRuntime,
};
use open_sandbox_agent::controller_client::ControllerConnection;
use open_sandbox_agent::proxy_client::ProxyConnection;
use open_sandbox_agent::sandbox::SandboxManager;
use open_sandbox_agent::tunnel::{ForwardRequest, ForwardResponse, HttpClient, TunnelForwarder};

use open_sandbox_contracts::error::AgentError;

// --- Mock Container Runtime ---

struct MockRuntime {
    created: AtomicUsize,
    stopped: AtomicUsize,
    port_counter: AtomicUsize,
}

impl MockRuntime {
    fn new() -> Self {
        Self {
            created: AtomicUsize::new(0),
            stopped: AtomicUsize::new(0),
            port_counter: AtomicUsize::new(9000),
        }
    }
}

impl ContainerRuntime for MockRuntime {
    async fn create_and_start(&self, config: ContainerConfig) -> Result<ContainerInfo, AgentError> {
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
        Ok(Vec::new())
    }
}

// --- Mock HTTP Client ---

struct MockHttp {
    response: ForwardResponse,
}

impl HttpClient for MockHttp {
    async fn send(
        &self,
        _port: u16,
        _request: ForwardRequest,
    ) -> Result<ForwardResponse, AgentError> {
        Ok(self.response.clone())
    }
}

// --- Mock Controller Server ---

struct MockController {
    accept_all: bool,
    registered_agents: Arc<Mutex<Vec<String>>>,
    heartbeat_count: Arc<AtomicUsize>,
    status_reports: Arc<Mutex<Vec<(String, i32)>>>,
    pending_commands: Arc<Mutex<Vec<ControllerCommand>>>,
}

impl MockController {
    fn new(accept_all: bool) -> Self {
        Self {
            accept_all,
            registered_agents: Arc::new(Mutex::new(Vec::new())),
            heartbeat_count: Arc::new(AtomicUsize::new(0)),
            status_reports: Arc::new(Mutex::new(Vec::new())),
            pending_commands: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[tonic::async_trait]
impl ControllerService for MockController {
    type AgentStreamStream = ReceiverStream<Result<ControllerCommand, Status>>;

    async fn agent_stream(
        &self,
        request: Request<Streaming<AgentMessage>>,
    ) -> Result<Response<Self::AgentStreamStream>, Status> {
        let mut inbound = request.into_inner();
        let (tx, rx) = mpsc::channel(32);

        let accept_all = self.accept_all;
        let registered_agents = self.registered_agents.clone();
        let heartbeat_count = self.heartbeat_count.clone();
        let status_reports = self.status_reports.clone();
        let pending_commands = self.pending_commands.clone();

        tokio::spawn(async move {
            while let Ok(Some(msg)) = inbound.message().await {
                let Some(payload) = msg.payload else {
                    continue;
                };

                match payload {
                    agent_message::Payload::Register(req) => {
                        registered_agents.lock().unwrap().push(req.agent_id.clone());
                        let resp = RegisterResponse {
                            accepted: accept_all,
                            rejection_reason: if accept_all {
                                String::new()
                            } else {
                                "rejected".into()
                            },
                            agent_certificate: String::new(),
                        };
                        let cmd = ControllerCommand {
                            payload: Some(controller_command::Payload::RegisterResponse(resp)),
                        };
                        if tx.send(Ok(cmd)).await.is_err() {
                            break;
                        }

                        if accept_all {
                            let commands: Vec<_> =
                                pending_commands.lock().unwrap().drain(..).collect();
                            for cmd in commands {
                                if tx.send(Ok(cmd)).await.is_err() {
                                    return;
                                }
                            }
                        }
                    }
                    agent_message::Payload::Heartbeat(_) => {
                        heartbeat_count.fetch_add(1, Ordering::SeqCst);
                        let ack = ControllerCommand {
                            payload: Some(controller_command::Payload::HeartbeatAck(
                                HeartbeatAck {
                                    timestamp: Some(prost_types::Timestamp::from(
                                        std::time::SystemTime::now(),
                                    )),
                                },
                            )),
                        };
                        let _ = tx.send(Ok(ack)).await;
                    }
                    agent_message::Payload::SandboxStatus(status) => {
                        status_reports
                            .lock()
                            .unwrap()
                            .push((status.sandbox_id, status.state));
                    }
                    agent_message::Payload::ResourceReport(_) => {}
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

// --- Mock Proxy Server ---

struct MockProxy {
    pending_requests: Arc<Mutex<Vec<TunnelRequest>>>,
    received_responses: Arc<Mutex<Vec<TunnelResponse>>>,
}

#[tonic::async_trait]
impl TunnelService for MockProxy {
    type OpenTunnelStream = ReceiverStream<Result<TunnelRequest, Status>>;

    async fn open_tunnel(
        &self,
        request: Request<Streaming<TunnelResponse>>,
    ) -> Result<Response<Self::OpenTunnelStream>, Status> {
        let mut inbound = request.into_inner();
        let (tx, rx) = mpsc::channel(32);

        let pending = self.pending_requests.clone();
        let received = self.received_responses.clone();

        tokio::spawn(async move {
            // Wait for TunnelReady
            if let Ok(Some(ready_msg)) = inbound.message().await {
                received.lock().unwrap().push(ready_msg);
            }

            // Send pending requests
            let requests: Vec<_> = pending.lock().unwrap().drain(..).collect();
            for req in requests {
                if tx.send(Ok(req)).await.is_err() {
                    return;
                }
            }

            // Collect responses
            while let Ok(Some(resp)) = inbound.message().await {
                received.lock().unwrap().push(resp);
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

// --- Helpers ---

async fn start_mock_controller(mock: MockController) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = format!("http://{}", listener.local_addr().unwrap());

    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(ControllerServiceServer::new(mock))
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .unwrap();
    });

    addr
}

async fn start_mock_proxy(mock: MockProxy) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = format!("http://{}", listener.local_addr().unwrap());

    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(TunnelServiceServer::new(mock))
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .unwrap();
    });

    addr
}

fn test_resources() -> AgentResources {
    AgentResources {
        cpu_cores: 4,
        memory_bytes: 8_000_000_000,
        arch: 1,
        os: "linux".into(),
    }
}

// --- Tests ---

#[tokio::test(flavor = "multi_thread")]
async fn agent_registers_with_mock_controller() {
    let mock = MockController::new(true);
    let registered = mock.registered_agents.clone();
    let addr = start_mock_controller(mock).await;

    let runtime = Arc::new(MockRuntime::new());
    let manager = Arc::new(SandboxManager::new(runtime));
    let agent_id = AgentId::new();

    let conn = ControllerConnection::new(
        agent_id.clone(),
        JoinToken::new("test-token".into()),
        manager,
        test_resources(),
    );

    let handle = tokio::spawn(async move { conn.run(&addr).await });

    tokio::time::sleep(Duration::from_millis(100)).await;
    handle.abort();

    let agents = registered.lock().unwrap();
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0], agent_id.to_string());
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_rejected_by_controller() {
    let mock = MockController::new(false);
    let addr = start_mock_controller(mock).await;

    let runtime = Arc::new(MockRuntime::new());
    let manager = Arc::new(SandboxManager::new(runtime));

    let conn = ControllerConnection::new(
        AgentId::new(),
        JoinToken::new("bad-token".into()),
        manager,
        test_resources(),
    );

    let result = conn.run(&addr).await;
    assert!(matches!(result, Err(AgentError::Internal { .. })));
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_handles_start_sandbox_command() {
    let sandbox_id = SandboxId::new();
    let start_cmd = ControllerCommand {
        payload: Some(controller_command::Payload::StartSandbox(StartSandbox {
            sandbox_id: sandbox_id.to_string(),
            image: "nginx:latest".into(),
            config: Some(SandboxConfig {
                cpu_limit_millicores: 1000,
                memory_limit_bytes: 512_000_000,
                env_vars: HashMap::new(),
                exposed_port: 8080,
            }),
        })),
    };

    let mock = MockController::new(true);
    mock.pending_commands.lock().unwrap().push(start_cmd);
    let status_reports = mock.status_reports.clone();
    let addr = start_mock_controller(mock).await;

    let runtime = Arc::new(MockRuntime::new());
    let manager = Arc::new(SandboxManager::new(runtime.clone()));

    let conn = ControllerConnection::new(
        AgentId::new(),
        JoinToken::new("token".into()),
        manager,
        test_resources(),
    );

    let handle = tokio::spawn(async move { conn.run(&addr).await });

    tokio::time::sleep(Duration::from_millis(200)).await;
    handle.abort();

    assert_eq!(runtime.created.load(Ordering::SeqCst), 1);

    let reports = status_reports.lock().unwrap();
    assert!(!reports.is_empty());
    assert_eq!(reports[0].0, sandbox_id.to_string());
    assert_eq!(reports[0].1, SandboxState::Running as i32);
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_sends_heartbeats() {
    let mock = MockController::new(true);
    let heartbeat_count = mock.heartbeat_count.clone();
    let addr = start_mock_controller(mock).await;

    let runtime = Arc::new(MockRuntime::new());
    let manager = Arc::new(SandboxManager::new(runtime));

    let conn = ControllerConnection::new(
        AgentId::new(),
        JoinToken::new("token".into()),
        manager,
        test_resources(),
    );

    let handle = tokio::spawn(async move { conn.run(&addr).await });

    tokio::time::sleep(Duration::from_millis(5500)).await;
    handle.abort();

    assert!(heartbeat_count.load(Ordering::SeqCst) >= 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_forwards_tunnel_request() {
    let sandbox_id = SandboxId::new();

    let runtime = Arc::new(MockRuntime::new());
    let manager = Arc::new(SandboxManager::new(runtime));

    // Pre-start a sandbox so it has a port mapping
    use open_sandbox_contracts::controller::SandboxConfig;
    let start = StartSandbox {
        sandbox_id: sandbox_id.to_string(),
        image: "nginx:latest".into(),
        config: Some(SandboxConfig {
            cpu_limit_millicores: 1000,
            memory_limit_bytes: 512_000_000,
            env_vars: HashMap::new(),
            exposed_port: 8080,
        }),
    };
    manager.start_sandbox(start).await.unwrap();

    let http_client = Arc::new(MockHttp {
        response: ForwardResponse {
            status_code: 200,
            headers: Default::default(),
            body: b"proxied-response".to_vec(),
        },
    });
    let forwarder = Arc::new(TunnelForwarder::new(manager, http_client));

    let tunnel_req = TunnelRequest {
        stream_id: "stream-1".into(),
        payload: Some(tunnel_request::Payload::HttpRequest(HttpRequest {
            method: "GET".into(),
            uri: "/index.html".into(),
            headers: HashMap::new(),
            body: vec![],
            sandbox_id: sandbox_id.to_string(),
        })),
    };

    let mock_proxy = MockProxy {
        pending_requests: Arc::new(Mutex::new(vec![tunnel_req])),
        received_responses: Arc::new(Mutex::new(Vec::new())),
    };
    let received = mock_proxy.received_responses.clone();
    let addr = start_mock_proxy(mock_proxy).await;

    let proxy_conn = ProxyConnection::new(AgentId::new(), forwarder);

    let handle = tokio::spawn(async move { proxy_conn.run(&addr).await });

    tokio::time::sleep(Duration::from_millis(300)).await;
    handle.abort();

    let responses = received.lock().unwrap();
    // First response is TunnelReady, second is the HTTP response
    assert!(responses.len() >= 2);

    let http_resp = &responses[1];
    match &http_resp.payload {
        Some(tunnel_response::Payload::HttpResponse(r)) => {
            assert_eq!(r.status_code, 200);
            assert_eq!(r.body, b"proxied-response");
        }
        other => panic!("expected HttpResponse, got {other:?}"),
    }
}
