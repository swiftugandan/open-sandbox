use open_sandbox_contracts::error::ControllerError;
use open_sandbox_contracts::types::{AgentId, RoutingEntry};

#[derive(Debug, Clone)]
pub struct AgentCapacity {
    pub cpu_cores: u32,
    pub memory_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct AvailableResources {
    pub cpu_millicores: u32,
    pub memory_bytes: u64,
    pub running_sandboxes: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentState {
    Active,
    Dead,
}

#[derive(Debug, Clone)]
pub struct AgentRecord {
    pub agent_id: AgentId,
    pub capacity: AgentCapacity,
    pub available: AvailableResources,
    pub state: AgentState,
}

pub trait ControllerStore: Send + Sync {
    fn save_agent(&self, record: AgentRecord) -> impl Future<Output = Result<(), ControllerError>> + Send;
    fn get_agent(&self, id: &AgentId) -> impl Future<Output = Result<Option<AgentRecord>, ControllerError>> + Send;
    fn remove_agent(&self, id: &AgentId) -> impl Future<Output = Result<(), ControllerError>> + Send;
    fn list_active_agents(&self) -> impl Future<Output = Result<Vec<AgentRecord>, ControllerError>> + Send;
    fn update_agent_state(&self, id: &AgentId, state: AgentState) -> impl Future<Output = Result<(), ControllerError>> + Send;
    fn insert_routing_entry(&self, entry: RoutingEntry) -> impl Future<Output = Result<(), ControllerError>> + Send;
    fn remove_routing_entries_for_agent(&self, agent_id: &AgentId) -> impl Future<Output = Result<(), ControllerError>> + Send;
}
