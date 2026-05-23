use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

use open_sandbox_contracts::controller::{
    AgentMessage, ControllerCommand, HeartbeatAck, RegisterResponse,
    SandboxConfig as ProtoSandboxConfig, SandboxState, StartSandbox, agent_message,
    controller_command,
    controller_service_server::{ControllerService, ControllerServiceServer},
};
use open_sandbox_contracts::error::ControllerError;
use open_sandbox_contracts::types::{AgentId, JoinToken, SandboxId};

use crate::heartbeat::HeartbeatMonitor;
use crate::registry::{AgentRegistry, RegistrationResult};
use crate::scheduler::{SandboxAssignment, SandboxRequirements, Scheduler};
use crate::store::{AgentCapacity, ControllerStore};
use crate::token::TokenValidator;

use open_sandbox_contracts::types::RoutingEntry;

type CommandSender = mpsc::Sender<Result<ControllerCommand, Status>>;

pub(crate) struct AgentConnections {
    senders: Mutex<HashMap<AgentId, CommandSender>>,
}

impl AgentConnections {
    fn new() -> Self {
        Self {
            senders: Mutex::new(HashMap::new()),
        }
    }

    fn add(&self, agent_id: AgentId, sender: CommandSender) {
        self.senders.lock().unwrap().insert(agent_id, sender);
    }

    pub(crate) fn remove(&self, agent_id: &AgentId) {
        self.senders.lock().unwrap().remove(agent_id);
    }

    pub(crate) async fn send_command(
        &self,
        agent_id: &AgentId,
        command: ControllerCommand,
    ) -> Result<(), ControllerError> {
        let sender = self.senders.lock().unwrap().get(agent_id).cloned();
        match sender {
            Some(tx) => tx
                .send(Ok(command))
                .await
                .map_err(|_| ControllerError::Internal {
                    detail: format!("agent {} channel closed", agent_id),
                }),
            None => Err(ControllerError::AgentNotFound {
                agent_id: agent_id.to_string(),
            }),
        }
    }
}

pub struct GrpcHandler<S: ControllerStore> {
    registry: Arc<AgentRegistry<S>>,
    heartbeat_monitor: Arc<HeartbeatMonitor>,
    connections: Arc<AgentConnections>,
    store: Arc<S>,
}

#[tonic::async_trait]
impl<S: ControllerStore + 'static> ControllerService for GrpcHandler<S> {
    type AgentStreamStream = ReceiverStream<Result<ControllerCommand, Status>>;

    async fn agent_stream(
        &self,
        request: Request<Streaming<AgentMessage>>,
    ) -> Result<Response<Self::AgentStreamStream>, Status> {
        let mut inbound = request.into_inner();
        let (tx, rx) = mpsc::channel(32);

        let registry = self.registry.clone();
        let heartbeat_monitor = self.heartbeat_monitor.clone();
        let connections = self.connections.clone();
        let store = self.store.clone();

        tokio::spawn(async move {
            let mut registered_agent_id: Option<AgentId> = None;

            while let Ok(Some(msg)) = inbound.message().await {
                let Some(payload) = msg.payload else {
                    continue;
                };

                match payload {
                    agent_message::Payload::Register(req) => {
                        let Ok(agent_uuid) = uuid::Uuid::parse_str(&req.agent_id) else {
                            let _ = tx
                                .send(Err(Status::invalid_argument("invalid agent_id UUID")))
                                .await;
                            break;
                        };
                        let agent_id = AgentId::from(agent_uuid);
                        let token = JoinToken::new(req.join_token);
                        let capacity = AgentCapacity {
                            cpu_cores: req.resources.as_ref().map_or(0, |r| r.cpu_cores),
                            memory_bytes: req.resources.as_ref().map_or(0, |r| r.memory_bytes),
                        };

                        let result = registry.register(agent_id.clone(), &token, capacity).await;

                        let response = match result {
                            Ok(RegistrationResult::Accepted) => {
                                connections.add(agent_id.clone(), tx.clone());
                                heartbeat_monitor.record_heartbeat(agent_id.clone());
                                registered_agent_id = Some(agent_id);
                                RegisterResponse {
                                    accepted: true,
                                    rejection_reason: String::new(),
                                    agent_certificate: String::new(),
                                }
                            }
                            Ok(RegistrationResult::Rejected { reason }) => RegisterResponse {
                                accepted: false,
                                rejection_reason: reason,
                                agent_certificate: String::new(),
                            },
                            Err(e) => RegisterResponse {
                                accepted: false,
                                rejection_reason: e.to_string(),
                                agent_certificate: String::new(),
                            },
                        };

                        let command = ControllerCommand {
                            payload: Some(controller_command::Payload::RegisterResponse(response)),
                        };
                        if tx.send(Ok(command)).await.is_err() {
                            break;
                        }
                    }

                    agent_message::Payload::Heartbeat(hb) => {
                        let Ok(agent_uuid) = uuid::Uuid::parse_str(&hb.agent_id) else {
                            continue;
                        };
                        let agent_id = AgentId::from(agent_uuid);

                        if let Err(e) = registry.heartbeat(&agent_id).await {
                            let _ = tx.send(Err(Status::not_found(e.to_string()))).await;
                            break;
                        }

                        heartbeat_monitor.record_heartbeat(agent_id);

                        let ack = HeartbeatAck {
                            timestamp: Some(prost_types::Timestamp::from(
                                std::time::SystemTime::now(),
                            )),
                        };
                        let command = ControllerCommand {
                            payload: Some(controller_command::Payload::HeartbeatAck(ack)),
                        };
                        if tx.send(Ok(command)).await.is_err() {
                            break;
                        }
                    }

                    agent_message::Payload::SandboxStatus(status) => {
                        if let Some(ref agent_id) = registered_agent_id {
                            let sandbox_id =
                                uuid::Uuid::parse_str(&status.sandbox_id).map(SandboxId::from);
                            if let Ok(sandbox_id) = sandbox_id {
                                let state_str = sandbox_state_to_str(status.state());
                                let error = if status.error_message.is_empty() {
                                    None
                                } else {
                                    Some(status.error_message.as_str())
                                };
                                let _ = store
                                    .save_sandbox_state(&sandbox_id, agent_id, state_str, error)
                                    .await;
                            }
                        }
                    }

                    agent_message::Payload::ResourceReport(_) => {}
                }
            }

            if let Some(agent_id) = registered_agent_id {
                connections.remove(&agent_id);
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

pub struct CreateSandboxRequest {
    pub sandbox_id: SandboxId,
    pub image: String,
    pub requirements: SandboxRequirements,
    pub env_vars: std::collections::HashMap<String, String>,
    pub exposed_port: u32,
}

pub struct Controller<S: ControllerStore> {
    pub(crate) registry: Arc<AgentRegistry<S>>,
    pub(crate) heartbeat_monitor: Arc<HeartbeatMonitor>,
    pub(crate) scheduler: Arc<Scheduler<S>>,
    pub(crate) connections: Arc<AgentConnections>,
}

impl<S: ControllerStore + 'static> Controller<S> {
    pub fn new(store: Arc<S>, validator: impl TokenValidator + 'static) -> Self {
        let registry = Arc::new(AgentRegistry::new(store.clone(), validator));
        let heartbeat_monitor = Arc::new(HeartbeatMonitor::new());
        let scheduler = Arc::new(Scheduler::new(store));
        let connections = Arc::new(AgentConnections::new());
        Self {
            registry,
            heartbeat_monitor,
            scheduler,
            connections,
        }
    }

    pub fn grpc_service(&self) -> ControllerServiceServer<GrpcHandler<S>> {
        ControllerServiceServer::new(GrpcHandler {
            registry: self.registry.clone(),
            heartbeat_monitor: self.heartbeat_monitor.clone(),
            connections: self.connections.clone(),
            store: self.scheduler.store_arc(),
        })
    }

    pub async fn create_sandbox(
        &self,
        request: CreateSandboxRequest,
    ) -> Result<SandboxAssignment, ControllerError> {
        let assignment = self
            .scheduler
            .assign_sandbox(request.sandbox_id.clone(), &request.requirements)
            .await?;

        let command = ControllerCommand {
            payload: Some(controller_command::Payload::StartSandbox(StartSandbox {
                sandbox_id: request.sandbox_id.to_string(),
                image: request.image,
                config: Some(ProtoSandboxConfig {
                    cpu_limit_millicores: request.requirements.cpu_millicores,
                    memory_limit_bytes: request.requirements.memory_bytes,
                    env_vars: request.env_vars,
                    exposed_port: request.exposed_port,
                }),
            })),
        };
        self.connections
            .send_command(&assignment.agent_id, command)
            .await?;

        Ok(assignment)
    }

    pub async fn find_routing_entry(
        &self,
        sandbox_id: &SandboxId,
    ) -> Result<Option<RoutingEntry>, ControllerError> {
        self.scheduler.store().find_routing_entry(sandbox_id).await
    }

    pub async fn list_routing_entries(&self) -> Result<Vec<RoutingEntry>, ControllerError> {
        self.scheduler.store().list_routing_entries().await
    }

    pub async fn remove_routing_entry(
        &self,
        sandbox_id: &SandboxId,
    ) -> Result<(), ControllerError> {
        self.scheduler
            .store()
            .remove_routing_entry(sandbox_id)
            .await
    }

    pub async fn save_sandbox_state(
        &self,
        sandbox_id: &SandboxId,
        agent_id: &AgentId,
        state: &str,
        error: Option<&str>,
    ) -> Result<(), ControllerError> {
        self.scheduler
            .store()
            .save_sandbox_state(sandbox_id, agent_id, state, error)
            .await
    }

    pub async fn get_sandbox_state(
        &self,
        sandbox_id: &SandboxId,
    ) -> Result<Option<crate::store::SandboxStateRow>, ControllerError> {
        self.scheduler.store().get_sandbox_state(sandbox_id).await
    }

    pub async fn sweep_dead_agents(&self) -> Vec<AgentId> {
        let dead = self.heartbeat_monitor.dead_agents();
        for agent_id in &dead {
            let _ = self.registry.mark_agent_dead(agent_id).await;
            self.connections.remove(agent_id);
            self.heartbeat_monitor.remove(agent_id);
        }
        dead
    }
}

fn sandbox_state_to_str(state: SandboxState) -> &'static str {
    match state {
        SandboxState::Creating => "creating",
        SandboxState::Running => "running",
        SandboxState::Stopping => "stopping",
        SandboxState::Stopped => "stopped",
        SandboxState::Failed => "failed",
        SandboxState::Unspecified => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{AgentCapacity, AgentState};
    use crate::testutil::*;
    use open_sandbox_contracts::constants::DEAD_AGENT_TIMEOUT;
    use open_sandbox_contracts::controller::{
        AgentResources, Heartbeat, RegisterRequest,
        controller_service_client::ControllerServiceClient,
    };
    use open_sandbox_contracts::types::RoutingEntry;
    use std::time::Duration;
    use tokio_stream::wrappers::TcpListenerStream;

    async fn start_controller(
        validator: impl TokenValidator + 'static,
    ) -> (Controller<InMemoryStore>, String) {
        let store = Arc::new(InMemoryStore::new());
        let controller = Controller::new(store, validator);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = format!("http://{}", listener.local_addr().unwrap());

        let service = controller.grpc_service();
        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(service)
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
                .unwrap();
        });

        (controller, addr)
    }

    async fn connect_agent(
        addr: &str,
    ) -> (
        mpsc::Sender<AgentMessage>,
        tonic::Streaming<ControllerCommand>,
    ) {
        let channel = tonic::transport::Channel::from_shared(addr.to_string())
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut client = ControllerServiceClient::new(channel);

        let (tx, rx) = mpsc::channel(32);
        let outbound = ReceiverStream::new(rx);
        let response = client.agent_stream(outbound).await.unwrap();
        (tx, response.into_inner())
    }

    fn register_message(agent_id: &AgentId, token: &str) -> AgentMessage {
        AgentMessage {
            payload: Some(agent_message::Payload::Register(RegisterRequest {
                agent_id: agent_id.to_string(),
                join_token: token.into(),
                resources: Some(AgentResources {
                    cpu_cores: 4,
                    memory_bytes: 8_000_000_000,
                    arch: 1,
                    os: "linux".into(),
                }),
            })),
        }
    }

    fn heartbeat_message(agent_id: &AgentId) -> AgentMessage {
        AgentMessage {
            payload: Some(agent_message::Payload::Heartbeat(Heartbeat {
                agent_id: agent_id.to_string(),
                timestamp: prost_types::Timestamp::try_from(std::time::SystemTime::now()).ok(),
            })),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn registration_accepted_with_valid_token() {
        let (_controller, addr) = start_controller(AcceptAllTokens).await;
        let (tx, mut inbound) = connect_agent(&addr).await;

        tx.send(register_message(&AgentId::new(), "token"))
            .await
            .unwrap();

        let msg = inbound.message().await.unwrap().unwrap();
        match msg.payload.unwrap() {
            controller_command::Payload::RegisterResponse(resp) => assert!(resp.accepted),
            other => panic!("expected RegisterResponse, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn registration_rejected_with_invalid_token() {
        let (_controller, addr) = start_controller(RejectAllTokens).await;
        let (tx, mut inbound) = connect_agent(&addr).await;

        tx.send(register_message(&AgentId::new(), "bad"))
            .await
            .unwrap();

        let msg = inbound.message().await.unwrap().unwrap();
        match msg.payload.unwrap() {
            controller_command::Payload::RegisterResponse(resp) => {
                assert!(!resp.accepted);
                assert!(!resp.rejection_reason.is_empty());
            }
            other => panic!("expected RegisterResponse, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn heartbeat_returns_ack() {
        let (_controller, addr) = start_controller(AcceptAllTokens).await;
        let (tx, mut inbound) = connect_agent(&addr).await;
        let agent_id = AgentId::new();

        tx.send(register_message(&agent_id, "token")).await.unwrap();
        let _ = inbound.message().await.unwrap().unwrap();

        tx.send(heartbeat_message(&agent_id)).await.unwrap();

        let msg = inbound.message().await.unwrap().unwrap();
        match msg.payload.unwrap() {
            controller_command::Payload::HeartbeatAck(ack) => {
                assert!(ack.timestamp.is_some());
            }
            other => panic!("expected HeartbeatAck, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn create_sandbox_sends_start_command_to_agent() {
        let (controller, addr) = start_controller(AcceptAllTokens).await;
        let (tx, mut inbound) = connect_agent(&addr).await;
        let agent_id = AgentId::new();

        tx.send(register_message(&agent_id, "token")).await.unwrap();
        let _ = inbound.message().await.unwrap().unwrap();

        let sandbox_id = SandboxId::new();
        controller
            .create_sandbox(CreateSandboxRequest {
                sandbox_id: sandbox_id.clone(),
                image: "nginx:latest".into(),
                requirements: SandboxRequirements {
                    cpu_millicores: 1000,
                    memory_bytes: 512_000_000,
                },
                env_vars: std::collections::HashMap::new(),
                exposed_port: 8080,
            })
            .await
            .unwrap();

        let msg = inbound.message().await.unwrap().unwrap();
        match msg.payload.unwrap() {
            controller_command::Payload::StartSandbox(cmd) => {
                assert_eq!(cmd.sandbox_id, sandbox_id.to_string());
                assert_eq!(cmd.image, "nginx:latest");
            }
            other => panic!("expected StartSandbox, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn sweep_removes_dead_agents_and_routing() {
        let store = Arc::new(InMemoryStore::new());
        let controller = Controller::new(store.clone(), AcceptAllTokens);
        let agent_id = AgentId::new();

        controller
            .registry
            .register(
                agent_id.clone(),
                &JoinToken::new("token".into()),
                AgentCapacity {
                    cpu_cores: 4,
                    memory_bytes: 8_000_000_000,
                },
            )
            .await
            .unwrap();
        controller
            .heartbeat_monitor
            .record_heartbeat(agent_id.clone());

        store
            .insert_routing_entry(RoutingEntry {
                sandbox_id: SandboxId::new(),
                agent_id: agent_id.clone(),
            })
            .await
            .unwrap();

        tokio::time::advance(DEAD_AGENT_TIMEOUT + Duration::from_secs(1)).await;

        let dead = controller.sweep_dead_agents().await;
        assert_eq!(dead, vec![agent_id.clone()]);

        let agent = store.get_agent(&agent_id).await.unwrap().unwrap();
        assert_eq!(agent.state, AgentState::Dead);
        assert!(store.routing_entries_for_agent(&agent_id).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn sweep_preserves_heartbeat_when_mark_dead_fails_transiently() {
        let store = Arc::new(FailNextStore::new());
        let controller = Controller::new(store.clone(), AcceptAllTokens);
        let agent_id = AgentId::new();

        controller
            .registry
            .register(
                agent_id.clone(),
                &JoinToken::new("token".into()),
                AgentCapacity {
                    cpu_cores: 4,
                    memory_bytes: 8_000_000_000,
                },
            )
            .await
            .unwrap();
        controller
            .heartbeat_monitor
            .record_heartbeat(agent_id.clone());

        tokio::time::advance(DEAD_AGENT_TIMEOUT + Duration::from_secs(1)).await;

        // Inject one failure into the next update_agent_state call.
        store.arm_update_agent_state_failure();

        let dead = controller.sweep_dead_agents().await;
        assert_eq!(dead, vec![agent_id.clone()]);

        // The Database error means mark_agent_dead failed. The next sweep
        // must be able to retry, so the heartbeat entry MUST still be present.
        let dead_again = controller.heartbeat_monitor.dead_agents();
        assert_eq!(
            dead_again,
            vec![agent_id.clone()],
            "heartbeat entry must survive a transient mark_agent_dead failure"
        );

        // Second sweep, without injected failure, completes the cleanup.
        let dead = controller.sweep_dead_agents().await;
        assert_eq!(dead, vec![agent_id.clone()]);
        assert!(
            controller.heartbeat_monitor.dead_agents().is_empty(),
            "successful sweep clears the heartbeat entry"
        );
    }
}
