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
        Self { mux, pool }
    }
}

#[tonic::async_trait]
impl TunnelService for TunnelHandler {
    type OpenTunnelStream = ReceiverStream<Result<TunnelRequest, Status>>;

    async fn open_tunnel(
        &self,
        request: Request<Streaming<TunnelResponse>>,
    ) -> Result<Response<Self::OpenTunnelStream>, Status> {
        let mut inbound = request.into_inner();
        let (result_tx, outbound_rx) = mpsc::channel::<Result<TunnelRequest, Status>>(32);
        let (request_tx, mut request_rx) = mpsc::channel::<TunnelRequest>(32);
        tokio::spawn(async move {
            while let Some(req) = request_rx.recv().await {
                if result_tx.send(Ok(req)).await.is_err() {
                    break;
                }
            }
        });

        let pool = self.pool.clone();
        let mux = self.mux.clone();

        tokio::spawn(async move {
            let mut registered_agent_id: Option<AgentId> = None;

            while let Ok(Some(msg)) = inbound.message().await {
                let Some(payload) = msg.payload else {
                    continue;
                };

                match payload {
                    tunnel_response::Payload::Ready(ready) => {
                        let Ok(agent_uuid) = uuid::Uuid::parse_str(&ready.agent_id) else {
                            break;
                        };
                        let agent_id = AgentId::from(agent_uuid);
                        pool.register(agent_id.clone(), request_tx.clone());
                        registered_agent_id = Some(agent_id);
                    }
                    tunnel_response::Payload::HttpResponse(resp) => {
                        mux.deliver_response(&msg.stream_id, resp);
                    }
                    tunnel_response::Payload::Data(_) | tunnel_response::Payload::Close(_) => {}
                }
            }

            if let Some(agent_id) = registered_agent_id {
                mux.cancel_agent_streams(&agent_id);
                pool.remove(&agent_id);
            }
        });

        Ok(Response::new(ReceiverStream::new(outbound_rx)))
    }
}

pub fn tunnel_service(
    mux: Arc<StreamMux>,
    pool: Arc<TunnelPool>,
) -> TunnelServiceServer<TunnelHandler> {
    TunnelServiceServer::new(TunnelHandler::new(mux, pool))
}

#[cfg(test)]
mod tests {
    use super::*;
    use open_sandbox_contracts::proxy::{
        tunnel_service_client::TunnelServiceClient, TunnelReady,
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
        let mut _inbound = response.into_inner();

        let agent_id = AgentId::new();
        let ready = TunnelResponse {
            stream_id: String::new(),
            payload: Some(tunnel_response::Payload::Ready(TunnelReady {
                agent_id: agent_id.to_string(),
            })),
        };
        outbound_tx.send(ready).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(shared_pool.contains(&agent_id));
    }
}
