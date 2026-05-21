use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use open_sandbox_contracts::constants::HEARTBEAT_INTERVAL;
use open_sandbox_contracts::controller::controller_command;
use open_sandbox_contracts::controller::controller_service_client::ControllerServiceClient;
use open_sandbox_contracts::controller::{
    AgentMessage, AgentResources, ExecResult, Heartbeat, RegisterRequest, SandboxStatus,
    agent_message,
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
        let channel = tonic::transport::Channel::from_shared(addr.to_string())
            .map_err(|e| AgentError::Internal {
                detail: e.to_string(),
            })?
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
                controller_command::Payload::StartSandbox(start) => {
                    let sandbox_id_str = start.sandbox_id.clone();
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
                            sandbox_id: sandbox_id_str,
                            state: state_val,
                            error_message: error_msg,
                        })),
                    };
                    let _ = status_tx.send(status).await;
                }
                controller_command::Payload::StopSandbox(stop) => {
                    let sandbox_id_str = stop.sandbox_id.clone();
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
                            sandbox_id: sandbox_id_str,
                            state: state_val,
                            error_message: error_msg,
                        })),
                    };
                    let _ = status_tx.send(status).await;
                }
                controller_command::Payload::RegisterResponse(_) => {}
                controller_command::Payload::Exec(exec) => {
                    let mgr = sandbox_manager.clone();
                    let tx = status_tx.clone();
                    tokio::spawn(async move {
                        let sandbox_id = uuid::Uuid::parse_str(&exec.sandbox_id)
                            .map(open_sandbox_contracts::types::SandboxId::from);
                        let result = match sandbox_id {
                            Ok(sid) => mgr
                                .exec_sandbox(&sid, exec.command, exec.stdin)
                                .await,
                            Err(_) => Err(open_sandbox_contracts::error::AgentError::SandboxNotFound {
                                sandbox_id: exec.sandbox_id.clone(),
                            }),
                        };
                        let exec_result = match result {
                            Ok(output) => ExecResult {
                                sandbox_id: exec.sandbox_id,
                                exec_id: exec.exec_id,
                                exit_code: output.exit_code,
                                stdout: output.stdout,
                                stderr: output.stderr,
                            },
                            Err(e) => ExecResult {
                                sandbox_id: exec.sandbox_id,
                                exec_id: exec.exec_id,
                                exit_code: -1,
                                stdout: vec![],
                                stderr: e.to_string().into_bytes(),
                            },
                        };
                        let msg = AgentMessage {
                            payload: Some(agent_message::Payload::ExecResult(exec_result)),
                        };
                        let _ = tx.send(msg).await;
                    });
                }
                controller_command::Payload::FetchLogs(_) => {}
            }
        }

        heartbeat_handle.abort();
        Err(AgentError::ControllerDisconnected)
    }
}
