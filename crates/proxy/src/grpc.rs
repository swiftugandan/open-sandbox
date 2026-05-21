use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

use open_sandbox_contracts::proxy::{
    tunnel_response,
    tunnel_service_server::{TunnelService, TunnelServiceServer},
    TunnelRequest, TunnelResponse,
};
use open_sandbox_contracts::types::AgentId;

use crate::stream_mux::StreamMux;
use crate::tunnel_pool::TunnelPool;

pub struct TunnelHandler {
    mux: Arc<StreamMux>,
    pool: Arc<TunnelPool>,
}

impl TunnelHandler {
    pub fn new(mux: Arc<StreamMux>, pool: Arc<TunnelPool>) -> Self {
        todo!()
    }
}

#[tonic::async_trait]
impl TunnelService for TunnelHandler {
    type OpenTunnelStream = ReceiverStream<Result<TunnelRequest, Status>>;

    async fn open_tunnel(
        &self,
        request: Request<Streaming<TunnelResponse>>,
    ) -> Result<Response<Self::OpenTunnelStream>, Status> {
        todo!()
    }
}

pub fn tunnel_service(
    mux: Arc<StreamMux>,
    pool: Arc<TunnelPool>,
) -> TunnelServiceServer<TunnelHandler> {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use open_sandbox_contracts::proxy::{
        tunnel_service_client::TunnelServiceClient, HttpResponse, TunnelReady,
    };
    use tokio_stream::wrappers::TcpListenerStream;

    async fn start_proxy_grpc(
        mux: Arc<StreamMux>,
        pool: Arc<TunnelPool>,
    ) -> String {
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

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_connects_and_registers_tunnel() {
        let pool = Arc::new(TunnelPool::new());
        let mux = Arc::new(StreamMux::new(pool.clone()));
        let addr = start_proxy_grpc(mux, pool.clone()).await;

        let channel = tonic::transport::Channel::from_shared(addr)
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut client = TunnelServiceClient::new(channel);

        let (outbound_tx, outbound_rx) = mpsc::channel(32);
        let outbound = ReceiverStream::new(outbound_rx);
        let _response = client.open_tunnel(outbound).await.unwrap();

        let agent_id = AgentId::new();
        let ready = TunnelResponse {
            stream_id: String::new(),
            payload: Some(tunnel_response::Payload::Ready(TunnelReady {
                agent_id: agent_id.to_string(),
            })),
        };
        outbound_tx.send(ready).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(pool.contains(&agent_id));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_disconnect_removes_from_pool() {
        let pool = Arc::new(TunnelPool::new());
        let mux = Arc::new(StreamMux::new(pool.clone()));
        let addr = start_proxy_grpc(mux, pool.clone()).await;

        let channel = tonic::transport::Channel::from_shared(addr)
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut client = TunnelServiceClient::new(channel);

        let (outbound_tx, outbound_rx) = mpsc::channel(32);
        let outbound = ReceiverStream::new(outbound_rx);
        let _response = client.open_tunnel(outbound).await.unwrap();

        let agent_id = AgentId::new();
        let ready = TunnelResponse {
            stream_id: String::new(),
            payload: Some(tunnel_response::Payload::Ready(TunnelReady {
                agent_id: agent_id.to_string(),
            })),
        };
        outbound_tx.send(ready).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(pool.contains(&agent_id));

        drop(outbound_tx);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(!pool.contains(&agent_id));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn proxy_sends_request_agent_responds() {
        let shared_pool = Arc::new(TunnelPool::new());
        let shared_mux = Arc::new(StreamMux::new(shared_pool.clone()));
        let addr = start_proxy_grpc(shared_mux.clone(), shared_pool.clone()).await;

        let channel = tonic::transport::Channel::from_shared(addr)
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut client = TunnelServiceClient::new(channel);

        let (outbound_tx, outbound_rx) = mpsc::channel(32);
        let outbound = ReceiverStream::new(outbound_rx);
        let response = client.open_tunnel(outbound).await.unwrap();
        let mut inbound = response.into_inner();

        let agent_id = AgentId::new();
        let ready = TunnelResponse {
            stream_id: String::new(),
            payload: Some(tunnel_response::Payload::Ready(TunnelReady {
                agent_id: agent_id.to_string(),
            })),
        };
        outbound_tx.send(ready).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // The gRPC handler should have registered this agent's tunnel.
        // When a request comes in for this agent, the handler sends it via
        // the inbound stream. When the agent sends back a response, the
        // handler delivers it to the mux.
        // This is verified in the e2e_mock test file.
        assert!(shared_pool.contains(&agent_id));
    }
}
