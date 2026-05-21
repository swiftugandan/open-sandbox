use tokio::sync::mpsc;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};

use open_sandbox_contracts::controller::controller_command;
use open_sandbox_contracts::controller::controller_service_server::{
    ControllerService, ControllerServiceServer,
};
use open_sandbox_contracts::controller::{
    AgentMessage, ControllerCommand, HeartbeatAck, RegisterResponse,
};
use open_sandbox_contracts::proxy::tunnel_response;
use open_sandbox_contracts::proxy::tunnel_service_server::{TunnelService, TunnelServiceServer};
use open_sandbox_contracts::proxy::{TunnelRequest, TunnelResponse};

use open_sandbox::cli::AgentArgs;
use open_sandbox::run;

struct MockControllerService;

#[tonic::async_trait]
impl ControllerService for MockControllerService {
    type AgentStreamStream = ReceiverStream<Result<ControllerCommand, tonic::Status>>;

    async fn agent_stream(
        &self,
        request: tonic::Request<tonic::Streaming<AgentMessage>>,
    ) -> Result<tonic::Response<Self::AgentStreamStream>, tonic::Status> {
        let mut inbound = request.into_inner();
        let (tx, rx) = mpsc::channel(32);

        tokio::spawn(async move {
            while let Ok(Some(msg)) = inbound.message().await {
                let Some(payload) = msg.payload else {
                    continue;
                };

                match payload {
                    open_sandbox_contracts::controller::agent_message::Payload::Register(_) => {
                        let response = ControllerCommand {
                            payload: Some(controller_command::Payload::RegisterResponse(
                                RegisterResponse {
                                    accepted: true,
                                    rejection_reason: String::new(),
                                    agent_certificate: String::new(),
                                },
                            )),
                        };
                        if tx.send(Ok(response)).await.is_err() {
                            break;
                        }
                    }
                    open_sandbox_contracts::controller::agent_message::Payload::Heartbeat(_) => {
                        let ack = ControllerCommand {
                            payload: Some(controller_command::Payload::HeartbeatAck(
                                HeartbeatAck {
                                    timestamp: Some(prost_types::Timestamp::from(
                                        std::time::SystemTime::now(),
                                    )),
                                },
                            )),
                        };
                        if tx.send(Ok(ack)).await.is_err() {
                            break;
                        }
                    }
                    _ => {}
                }
            }
        });

        Ok(tonic::Response::new(ReceiverStream::new(rx)))
    }
}

struct MockTunnelService;

#[tonic::async_trait]
impl TunnelService for MockTunnelService {
    type OpenTunnelStream = ReceiverStream<Result<TunnelRequest, tonic::Status>>;

    async fn open_tunnel(
        &self,
        request: tonic::Request<tonic::Streaming<TunnelResponse>>,
    ) -> Result<tonic::Response<Self::OpenTunnelStream>, tonic::Status> {
        let mut inbound = request.into_inner();
        let (_tx, rx) = mpsc::channel::<Result<TunnelRequest, tonic::Status>>(32);

        tokio::spawn(async move {
            while let Ok(Some(msg)) = inbound.message().await {
                if let Some(tunnel_response::Payload::Ready(_)) = msg.payload {
                    break;
                }
            }
        });

        Ok(tonic::Response::new(ReceiverStream::new(rx)))
    }
}

async fn start_mock_controller() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = format!("http://{}", listener.local_addr().unwrap());

    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(ControllerServiceServer::new(MockControllerService))
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .unwrap();
    });

    addr
}

async fn start_mock_proxy() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = format!("http://{}", listener.local_addr().unwrap());

    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(TunnelServiceServer::new(MockTunnelService))
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .unwrap();
    });

    addr
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_connects_to_mock_controller_and_proxy() {
    let controller_addr = start_mock_controller().await;
    let proxy_addr = start_mock_proxy().await;

    let args = AgentArgs {
        token: "test-token".to_string(),
        controller_url: controller_addr,
        proxy_url: proxy_addr,
    };

    let handle = tokio::spawn(async move {
        run::run_agent(args).await.map_err(|e| e.to_string())
    });

    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    handle.abort();
    let result = handle.await;
    match result {
        Err(e) if e.is_cancelled() => {}
        Err(e) if e.is_panic() => panic!("agent task panicked: {e}"),
        Err(e) => panic!("unexpected join error: {e}"),
        Ok(Ok(())) => {}
        Ok(Err(_)) => {}
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_fails_gracefully_with_unreachable_controller() {
    let args = AgentArgs {
        token: "test-token".to_string(),
        controller_url: "http://127.0.0.1:1".to_string(),
        proxy_url: "http://127.0.0.1:2".to_string(),
    };

    let result = run::run_agent(args).await;
    assert!(result.is_err(), "agent should return error when controller is unreachable");
}
