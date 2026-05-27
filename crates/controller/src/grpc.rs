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
    heartbeat_monitor: Arc<HeartbeatMonitor<S>>,
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
                                if let Err(e) =
                                    heartbeat_monitor.record_heartbeat(&agent_id).await
                                {
                                    tracing::warn!(error = %e, "record_heartbeat at register failed");
                                }
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

                        // F3: the heartbeat MUST refer to the stream's registered agent.
                        // A mismatch is treated as a benign protocol drift (could be a
                        // racy reconnect) and dropped — not stream-fatal.
                        match registered_agent_id.as_ref() {
                            Some(reg) if reg == &agent_id => {}
                            Some(reg) => {
                                tracing::warn!(
                                    stream_agent = %reg,
                                    msg_agent = %agent_id,
                                    "heartbeat agent_id mismatch; dropping"
                                );
                                continue;
                            }
                            None => {
                                tracing::warn!(
                                    msg_agent = %agent_id,
                                    "heartbeat before Register; dropping"
                                );
                                continue;
                            }
                        }

                        if let Err(e) = registry.heartbeat(&agent_id).await {
                            let _ = tx.send(Err(Status::not_found(e.to_string()))).await;
                            break;
                        }

                        if let Err(e) = heartbeat_monitor.record_heartbeat(&agent_id).await {
                            tracing::warn!(error = %e, "record_heartbeat failed");
                        }

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
                        let Some(ref agent_id) = registered_agent_id else {
                            tracing::warn!("SandboxStatus before Register; dropping");
                            continue;
                        };
                        let Ok(sandbox_id) =
                            uuid::Uuid::parse_str(&status.sandbox_id).map(SandboxId::from)
                        else {
                            continue;
                        };

                        // F2: only the agent that owns the routing entry may
                        // update the sandbox's state. A mismatch (or a routing
                        // entry that no longer exists) is dropped with a warn,
                        // not stream-fatal — a benign race during failover
                        // shouldn't tear down a healthy agent stream.
                        //
                        // Comp-3 C2 (v1.0.2 cascade bonus #11): terminal-state
                        // exception. management.delete_sandbox release_sandbox
                        // removes the routing row immediately; the agent's
                        // terminal SandboxStatus(Stopped) arrives moments
                        // later. Without this exception the no-routing-entry
                        // arm would drop it and sandbox_states would never
                        // advance past 'running' for clean deletions. We
                        // accept the late terminal state because it can only
                        // ever transition forward (Stopped/Failed are
                        // sinks).
                        use open_sandbox_contracts::controller::SandboxState;
                        let is_terminal = matches!(
                            status.state(),
                            SandboxState::Stopped | SandboxState::Failed
                        );
                        match store.find_routing_entry(&sandbox_id).await {
                            Ok(Some(entry)) if entry.agent_id == *agent_id => {}
                            Ok(Some(entry)) => {
                                tracing::warn!(
                                    sender = %agent_id,
                                    owner = %entry.agent_id,
                                    sandbox = %sandbox_id,
                                    "SandboxStatus from non-owning agent; dropping"
                                );
                                continue;
                            }
                            Ok(None) if is_terminal => {
                                tracing::info!(
                                    sender = %agent_id,
                                    sandbox = %sandbox_id,
                                    state = ?status.state(),
                                    "late terminal SandboxStatus after release_sandbox; persisting via exception"
                                );
                                // Fall through to save_sandbox_state.
                            }
                            Ok(None) => {
                                tracing::warn!(
                                    sender = %agent_id,
                                    sandbox = %sandbox_id,
                                    "SandboxStatus for sandbox with no routing entry; dropping"
                                );
                                continue;
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "find_routing_entry failed; dropping SandboxStatus");
                                continue;
                            }
                        }

                        let state_str = sandbox_state_to_str(status.state());
                        let error = if status.error_message.is_empty() {
                            None
                        } else {
                            Some(status.error_message.as_str())
                        };
                        if let Err(e) = store
                            .save_sandbox_state(&sandbox_id, agent_id, state_str, error)
                            .await
                        {
                            tracing::warn!(error = %e, "save_sandbox_state failed");
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
    /// v1.0.2: propagated end-to-end from CreateSandboxRequest on the
    /// API gateway through to the agent's `StartSandbox` payload.
    pub pull_policy: open_sandbox_contracts::types::PullPolicy,
}

pub struct Controller<S: ControllerStore> {
    pub(crate) registry: Arc<AgentRegistry<S>>,
    pub(crate) heartbeat_monitor: Arc<HeartbeatMonitor<S>>,
    pub(crate) scheduler: Arc<Scheduler<S>>,
    pub(crate) connections: Arc<AgentConnections>,
}

impl<S: ControllerStore + 'static> Controller<S> {
    pub fn new(store: Arc<S>, validator: impl TokenValidator + 'static) -> Self {
        let registry = Arc::new(AgentRegistry::new(store.clone(), validator));
        let heartbeat_monitor = Arc::new(HeartbeatMonitor::new(store.clone()));
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
                    pull_policy: request.pull_policy.to_wire() as i32,
                }),
            })),
        };
        if let Err(err) = self
            .connections
            .send_command(&assignment.agent_id, command)
            .await
        {
            // F8 + F5: roll back the routing entry AND release the capacity
            // the scheduler just reserved. If rollback itself fails, log but
            // surface the original send_command error.
            if let Err(rollback_err) = self
                .scheduler
                .store()
                .release_sandbox(&assignment.sandbox_id)
                .await
            {
                tracing::error!(
                    sandbox = %assignment.sandbox_id,
                    error = %rollback_err,
                    "failed to release sandbox reservation after send_command failure"
                );
            }
            return Err(err);
        }

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

    pub async fn release_sandbox(
        &self,
        sandbox_id: &SandboxId,
    ) -> Result<Option<AgentId>, ControllerError> {
        self.scheduler.store().release_sandbox(sandbox_id).await
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
        let dead = match self.heartbeat_monitor.dead_agents().await {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(error = %e, "dead_agents query failed; sweep skipped");
                return Vec::new();
            }
        };
        for agent_id in &dead {
            match self.registry.mark_agent_dead(agent_id).await {
                Ok(()) | Err(ControllerError::AgentNotFound { .. }) => {
                    // F6 + F7: state is now Dead in the store, so the next
                    // dead_agents() query naturally excludes this agent. The
                    // in-memory connections entry is the only thing left to
                    // clean up.
                    self.connections.remove(agent_id);
                }
                Err(err) => {
                    tracing::warn!(
                        agent = %agent_id,
                        error = %err,
                        "mark_agent_dead failed; next sweep will retry"
                    );
                }
            }
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
        SandboxState::Pausing => "pausing",
        SandboxState::Paused => "paused",
        SandboxState::Unpausing => "unpausing",
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
                pull_policy: Default::default(),
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
            .record_heartbeat(&agent_id)
            .await
            .unwrap();

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

    fn sandbox_status_message(sandbox_id: &SandboxId, state: SandboxState) -> AgentMessage {
        use open_sandbox_contracts::controller::SandboxStatus;
        AgentMessage {
            payload: Some(agent_message::Payload::SandboxStatus(SandboxStatus {
                sandbox_id: sandbox_id.to_string(),
                state: state as i32,
                error_message: String::new(),
            })),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn heartbeat_with_mismatched_agent_id_is_ignored() {
        // F3: agent registered as A sends Heartbeat{agent_id: B} where B is a
        // legitimately-registered peer. The controller MUST NOT call
        // store.record_heartbeat(B) — otherwise A can keep a dead B alive in
        // the liveness store indefinitely.
        let store = Arc::new(InMemoryStore::new());
        let controller = Controller::new(store.clone(), AcceptAllTokens);
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

        let agent_a = AgentId::new();
        let agent_b = AgentId::new();

        // Pre-register B directly so registry.heartbeat(&B) succeeds.
        controller
            .registry
            .register(
                agent_b.clone(),
                &JoinToken::new("token".into()),
                AgentCapacity {
                    cpu_cores: 4,
                    memory_bytes: 8_000_000_000,
                },
            )
            .await
            .unwrap();
        // B has NEVER recorded a heartbeat — confirm precondition.
        assert!(!store.heartbeated_agents().contains(&agent_b));

        let (tx, mut inbound) = connect_agent(&addr).await;
        tx.send(register_message(&agent_a, "token")).await.unwrap();
        let _ = inbound.message().await.unwrap().unwrap();

        // A sends Heartbeat{agent_id: B}.
        tx.send(heartbeat_message(&agent_b)).await.unwrap();

        // Barrier: legitimate heartbeat for A so we know the prior message has
        // been processed before we observe the store.
        tx.send(heartbeat_message(&agent_a)).await.unwrap();
        let msg = inbound.message().await.unwrap().unwrap();
        assert!(matches!(
            msg.payload,
            Some(controller_command::Payload::HeartbeatAck(_))
        ));

        let heartbeated = store.heartbeated_agents();
        assert!(
            !heartbeated.contains(&agent_b),
            "B must NOT have a recorded heartbeat; current set: {heartbeated:?}"
        );
        assert!(heartbeated.contains(&agent_a));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sandbox_status_from_non_owning_agent_is_ignored() {
        // F2: routing assigns sandbox_x → agent_A. Agent B (connected and
        // registered) sends SandboxStatus{sandbox_id: sandbox_x, state: Failed}.
        // The controller MUST reject the update because B is not the owner.
        let (controller, addr) = start_controller(AcceptAllTokens).await;

        let agent_a = AgentId::new();
        let agent_b = AgentId::new();
        let sandbox_x = SandboxId::new();

        // Register agent A in registry/connections, seed the routing entry x→A,
        // and pre-set the sandbox state to "running" so we can detect overwrites.
        controller
            .registry
            .register(
                agent_a.clone(),
                &JoinToken::new("token".into()),
                AgentCapacity {
                    cpu_cores: 4,
                    memory_bytes: 8_000_000_000,
                },
            )
            .await
            .unwrap();
        controller
            .scheduler
            .store()
            .insert_routing_entry(RoutingEntry {
                sandbox_id: sandbox_x.clone(),
                agent_id: agent_a.clone(),
            })
            .await
            .unwrap();
        controller
            .scheduler
            .store()
            .save_sandbox_state(&sandbox_x, &agent_a, "running", None)
            .await
            .unwrap();

        // Open a stream and register as B.
        let (tx, mut inbound) = connect_agent(&addr).await;
        tx.send(register_message(&agent_b, "token")).await.unwrap();
        let _ = inbound.message().await.unwrap().unwrap();

        // B sends a SandboxStatus for sandbox_x.
        tx.send(sandbox_status_message(&sandbox_x, SandboxState::Failed))
            .await
            .unwrap();

        // Barrier: heartbeat ack proves the prior message was processed.
        tx.send(heartbeat_message(&agent_b)).await.unwrap();
        let msg = inbound.message().await.unwrap().unwrap();
        assert!(matches!(
            msg.payload,
            Some(controller_command::Payload::HeartbeatAck(_))
        ));

        let row = controller
            .scheduler
            .store()
            .get_sandbox_state(&sandbox_x)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            row.state, "running",
            "non-owning agent B must not be able to overwrite sandbox_x's state"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn create_sandbox_rolls_back_routing_entry_when_send_command_fails() {
        // F8: scheduler.assign_sandbox INSERTS the routing entry to PG. If
        // send_command then fails (agent in list_active_agents but missing
        // from in-memory AgentConnections — a real race after disconnect),
        // the routing row must NOT be left orphaned.
        let store = Arc::new(InMemoryStore::new());
        let controller = Controller::new(store.clone(), AcceptAllTokens);

        // Seed an Active agent in the store but DO NOT connect it (no entry
        // in AgentConnections), so send_command will return AgentNotFound.
        let agent_id = AgentId::new();
        store
            .save_agent(crate::store::AgentRecord {
                agent_id: agent_id.clone(),
                capacity: AgentCapacity {
                    cpu_cores: 4,
                    memory_bytes: 8_000_000_000,
                },
                available: crate::store::AvailableResources {
                    cpu_millicores: 4000,
                    memory_bytes: 8_000_000_000,
                    running_sandboxes: 0,
                },
                state: crate::store::AgentState::Active,
            })
            .await
            .unwrap();

        let sandbox_id = SandboxId::new();
        let result = controller
            .create_sandbox(CreateSandboxRequest {
                sandbox_id: sandbox_id.clone(),
                image: "nginx:latest".into(),
                requirements: SandboxRequirements {
                    cpu_millicores: 1000,
                    memory_bytes: 512_000_000,
                },
                env_vars: std::collections::HashMap::new(),
                exposed_port: 8080,
                pull_policy: Default::default(),
            })
            .await;

        assert!(result.is_err(), "create_sandbox should fail when send_command fails");

        // Atomicity: NO routing entry must remain for the unreachable agent.
        assert!(
            store.routing_entries_for_agent(&agent_id).is_empty(),
            "orphan routing entry must be rolled back when send_command fails"
        );
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
            .record_heartbeat(&agent_id)
            .await
            .unwrap();

        tokio::time::advance(DEAD_AGENT_TIMEOUT + Duration::from_secs(1)).await;

        // Inject one failure into the next update_agent_state call.
        store.arm_update_agent_state_failure();

        let dead = controller.sweep_dead_agents().await;
        assert_eq!(dead, vec![agent_id.clone()]);

        // The Database error means mark_agent_dead failed. The next sweep
        // must be able to retry, so dead_agents() MUST still return this agent.
        let dead_again = controller.heartbeat_monitor.dead_agents().await.unwrap();
        assert_eq!(
            dead_again,
            vec![agent_id.clone()],
            "dead_agents must continue to surface a transient mark_agent_dead failure"
        );

        // Second sweep, without injected failure, completes the cleanup.
        let dead = controller.sweep_dead_agents().await;
        assert_eq!(dead, vec![agent_id.clone()]);
        assert!(
            controller.heartbeat_monitor.dead_agents().await.unwrap().is_empty(),
            "successful sweep should leave dead_agents empty (state flipped to Dead)"
        );
    }
}
