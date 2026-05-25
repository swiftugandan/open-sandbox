use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tracing::info;

use open_sandbox_contracts::constants::HEARTBEAT_INTERVAL;
use open_sandbox_contracts::controller::controller_command;
use open_sandbox_contracts::controller::controller_service_client::ControllerServiceClient;
use open_sandbox_contracts::controller::{
    AgentMessage, AgentResources, Heartbeat, RegisterRequest, SandboxStatus, agent_message,
};
use open_sandbox_contracts::error::AgentError;
use open_sandbox_contracts::types::{AgentId, JoinToken};

use crate::container::ContainerRuntime;
use crate::sandbox::SandboxManager;

pub struct ControllerConnection<R: ContainerRuntime> {
    agent_id: AgentId,
    join_token: JoinToken,
    sandbox_manager: Arc<SandboxManager<R>>,
    resources: AgentResources,
}

impl<R: ContainerRuntime + 'static> ControllerConnection<R> {
    pub fn new(
        agent_id: AgentId,
        join_token: JoinToken,
        sandbox_manager: Arc<SandboxManager<R>>,
        resources: AgentResources,
    ) -> Self {
        Self {
            agent_id,
            join_token,
            sandbox_manager,
            resources,
        }
    }

    pub async fn run(&self, addr: &str) -> Result<(), AgentError> {
        // Comp-3 B6: client-side HTTP/2 keepalive on the controller
        // channel too. The controller heartbeat reply path was the only
        // liveness signal; without keepalive, a frozen controller left
        // the agent's inbound `message().await` parked for OS-TCP
        // minutes.
        let channel = tonic::transport::Channel::from_shared(addr.to_string())
            .map_err(|e| AgentError::Internal {
                detail: e.to_string(),
            })?
            .keep_alive_while_idle(true)
            .keep_alive_timeout(std::time::Duration::from_secs(20))
            .http2_keep_alive_interval(std::time::Duration::from_secs(15))
            .connect()
            .await
            .map_err(|_| AgentError::ControllerDisconnected)?;

        let mut client = ControllerServiceClient::new(channel);

        let (outbound_tx, outbound_rx) = mpsc::channel(32);
        let outbound_stream = ReceiverStream::new(outbound_rx);

        let response = client
            .agent_stream(outbound_stream)
            .await
            .map_err(|_| AgentError::ControllerDisconnected)?;
        let mut inbound = response.into_inner();

        let register_msg = AgentMessage {
            payload: Some(agent_message::Payload::Register(RegisterRequest {
                agent_id: self.agent_id.to_string(),
                join_token: self.join_token.as_str().to_string(),
                resources: Some(self.resources.clone()),
            })),
        };
        outbound_tx
            .send(register_msg)
            .await
            .map_err(|_| AgentError::ControllerDisconnected)?;

        let msg = inbound
            .message()
            .await
            .map_err(|_| AgentError::ControllerDisconnected)?
            .ok_or(AgentError::ControllerDisconnected)?;

        let accepted = match msg.payload {
            Some(controller_command::Payload::RegisterResponse(resp)) => {
                if !resp.accepted {
                    return Err(AgentError::Internal {
                        detail: resp.rejection_reason,
                    });
                }
                true
            }
            _ => return Err(AgentError::ControllerDisconnected),
        };

        if !accepted {
            return Ok(());
        }

        info!(agent_id = %self.agent_id, "registered with controller");

        let heartbeat_tx = outbound_tx.clone();
        let agent_id_hb = self.agent_id.clone();
        let heartbeat_handle = tokio::spawn(async move {
            loop {
                tokio::time::sleep(HEARTBEAT_INTERVAL).await;
                let hb = AgentMessage {
                    payload: Some(agent_message::Payload::Heartbeat(Heartbeat {
                        agent_id: agent_id_hb.to_string(),
                        timestamp: Some(prost_types::Timestamp::from(std::time::SystemTime::now())),
                    })),
                };
                if heartbeat_tx.send(hb).await.is_err() {
                    break;
                }
            }
        });

        let sandbox_manager = self.sandbox_manager.clone();
        let status_tx = outbound_tx.clone();
        while let Ok(Some(cmd)) = inbound.message().await {
            let Some(payload) = cmd.payload else {
                continue;
            };

            match payload {
                controller_command::Payload::HeartbeatAck(_) => {}
                // Comp-3 A5: spawn start/stop dispatch into separate tasks
                // so a slow image pull or graceful shutdown can't head-of-line
                // block unrelated commands (the inbound loop must keep
                // pumping heartbeat acks and other tenants' lifecycle
                // commands without waiting for an in-flight stop).
                controller_command::Payload::StartSandbox(start) => {
                    let sandbox_id_str = start.sandbox_id.clone();
                    info!(sandbox_id = %sandbox_id_str, "received start command");
                    let sandbox_manager = sandbox_manager.clone();
                    let status_tx = status_tx.clone();
                    tokio::spawn(async move {
                        let state = sandbox_manager.start_sandbox(start).await;
                        let (state_val, error_msg) = match state {
                            Ok(s) => (s as i32, String::new()),
                            Err(e) => (
                                open_sandbox_contracts::controller::SandboxState::Failed as i32,
                                e.to_string(),
                            ),
                        };
                        let status = AgentMessage {
                            payload: Some(agent_message::Payload::SandboxStatus(SandboxStatus {
                                sandbox_id: sandbox_id_str.clone(),
                                state: state_val,
                                error_message: error_msg,
                            })),
                        };
                        let _ = status_tx.send(status).await;
                        info!(sandbox_id = %sandbox_id_str, state = state_val, "reported sandbox status");
                    });
                }
                controller_command::Payload::StopSandbox(stop) => {
                    let sandbox_id_str = stop.sandbox_id.clone();
                    info!(sandbox_id = %sandbox_id_str, "received stop command");
                    let sandbox_manager = sandbox_manager.clone();
                    let status_tx = status_tx.clone();
                    tokio::spawn(async move {
                        let state = sandbox_manager.stop_sandbox(stop).await;
                        let (state_val, error_msg) = match state {
                            Ok(s) => (s as i32, String::new()),
                            Err(e) => (
                                open_sandbox_contracts::controller::SandboxState::Failed as i32,
                                e.to_string(),
                            ),
                        };
                        let status = AgentMessage {
                            payload: Some(agent_message::Payload::SandboxStatus(SandboxStatus {
                                sandbox_id: sandbox_id_str.clone(),
                                state: state_val,
                                error_message: error_msg,
                            })),
                        };
                        let _ = status_tx.send(status).await;
                        info!(sandbox_id = %sandbox_id_str, state = state_val, "reported sandbox status");
                    });
                }
                controller_command::Payload::RegisterResponse(_) => {}
                // ExecCommand was removed from the controller stream in v1.0;
                // exec is now routed via the proxy data plane.
                controller_command::Payload::FetchLogs(_) => {}
            }
        }

        heartbeat_handle.abort();
        Err(AgentError::ControllerDisconnected)
    }
}
