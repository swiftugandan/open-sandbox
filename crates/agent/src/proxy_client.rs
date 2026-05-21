use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use open_sandbox_contracts::error::AgentError;
use open_sandbox_contracts::proxy::tunnel_request;
use open_sandbox_contracts::proxy::tunnel_response;
use open_sandbox_contracts::proxy::tunnel_service_client::TunnelServiceClient;
use open_sandbox_contracts::proxy::{HttpResponse, TunnelReady, TunnelResponse};
use open_sandbox_contracts::types::AgentId;

use crate::container::ContainerRuntime;
use crate::tunnel::{ForwardRequest, HttpClient, TunnelForwarder};

pub struct ProxyConnection<R: ContainerRuntime, H: HttpClient> {
    agent_id: AgentId,
    forwarder: Arc<TunnelForwarder<R, H>>,
}

impl<R: ContainerRuntime + 'static, H: HttpClient + 'static> ProxyConnection<R, H> {
    pub fn new(agent_id: AgentId, forwarder: Arc<TunnelForwarder<R, H>>) -> Self {
        Self {
            agent_id,
            forwarder,
        }
    }

    pub async fn run(&self, addr: &str) -> Result<(), AgentError> {
        let channel = tonic::transport::Channel::from_shared(addr.to_string())
            .map_err(|e| AgentError::Internal {
                detail: e.to_string(),
            })?
            .connect()
            .await
            .map_err(|_| AgentError::TunnelDisconnected)?;

        let mut client = TunnelServiceClient::new(channel);

        let (outbound_tx, outbound_rx) = mpsc::channel(32);
        let outbound_stream = ReceiverStream::new(outbound_rx);

        let response = client
            .open_tunnel(outbound_stream)
            .await
            .map_err(|_| AgentError::TunnelDisconnected)?;
        let mut inbound = response.into_inner();

        let ready = TunnelResponse {
            stream_id: String::new(),
            payload: Some(tunnel_response::Payload::Ready(TunnelReady {
                agent_id: self.agent_id.to_string(),
            })),
        };
        outbound_tx
            .send(ready)
            .await
            .map_err(|_| AgentError::TunnelDisconnected)?;

        let forwarder = self.forwarder.clone();
        while let Ok(Some(req)) = inbound.message().await {
            let stream_id = req.stream_id.clone();
            let Some(payload) = req.payload else {
                continue;
            };

            match payload {
                tunnel_request::Payload::HttpRequest(http_req) => {
                    let sandbox_id = open_sandbox_contracts::types::SandboxId::from(
                        uuid::Uuid::parse_str(&http_req.sandbox_id).map_err(|e| {
                            AgentError::Internal {
                                detail: e.to_string(),
                            }
                        })?,
                    );

                    let forward_req = ForwardRequest {
                        method: http_req.method,
                        uri: http_req.uri,
                        headers: http_req.headers,
                        body: http_req.body,
                    };

                    let resp = match forwarder.forward(&sandbox_id, forward_req).await {
                        Ok(r) => TunnelResponse {
                            stream_id,
                            payload: Some(tunnel_response::Payload::HttpResponse(HttpResponse {
                                status_code: r.status_code,
                                headers: r.headers,
                                body: r.body,
                            })),
                        },
                        Err(e) => TunnelResponse {
                            stream_id,
                            payload: Some(tunnel_response::Payload::Close(
                                open_sandbox_contracts::proxy::StreamClose {
                                    reason: e.to_string(),
                                },
                            )),
                        },
                    };

                    let _ = outbound_tx.send(resp).await;
                }
                tunnel_request::Payload::Data(_) | tunnel_request::Payload::Close(_) => {}
            }
        }

        Err(AgentError::TunnelDisconnected)
    }
}
