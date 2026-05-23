use open_sandbox_contracts::error::ControllerError;
use open_sandbox_contracts::types::{AgentId, RoutingEntry, SandboxId};

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
    fn save_agent(
        &self,
        record: AgentRecord,
    ) -> impl Future<Output = Result<(), ControllerError>> + Send;
    fn get_agent(
        &self,
        id: &AgentId,
    ) -> impl Future<Output = Result<Option<AgentRecord>, ControllerError>> + Send;
    fn remove_agent(
        &self,
        id: &AgentId,
    ) -> impl Future<Output = Result<(), ControllerError>> + Send;
    fn list_active_agents(
        &self,
    ) -> impl Future<Output = Result<Vec<AgentRecord>, ControllerError>> + Send;
    fn update_agent_state(
        &self,
        id: &AgentId,
        state: AgentState,
    ) -> impl Future<Output = Result<(), ControllerError>> + Send;
    fn insert_routing_entry(
        &self,
        entry: RoutingEntry,
    ) -> impl Future<Output = Result<(), ControllerError>> + Send;
    fn remove_routing_entries_for_agent(
        &self,
        agent_id: &AgentId,
    ) -> impl Future<Output = Result<(), ControllerError>> + Send;
    fn find_routing_entry(
        &self,
        sandbox_id: &SandboxId,
    ) -> impl Future<Output = Result<Option<RoutingEntry>, ControllerError>> + Send;
    fn list_routing_entries(
        &self,
    ) -> impl Future<Output = Result<Vec<RoutingEntry>, ControllerError>> + Send;
    fn remove_routing_entry(
        &self,
        sandbox_id: &SandboxId,
    ) -> impl Future<Output = Result<(), ControllerError>> + Send;
    fn save_sandbox_state(
        &self,
        sandbox_id: &SandboxId,
        agent_id: &AgentId,
        state: &str,
        error: Option<&str>,
    ) -> impl Future<Output = Result<(), ControllerError>> + Send;
    fn get_sandbox_state(
        &self,
        sandbox_id: &SandboxId,
    ) -> impl Future<Output = Result<Option<SandboxStateRow>, ControllerError>> + Send;

    /// Atomically transition an agent to Dead AND remove all of its routing
    /// entries. Either both writes persist, or neither does — implementations
    /// MUST NOT leave the system in a state where the agent is marked Dead in
    /// storage but routing entries still reference it (or vice-versa).
    ///
    /// AgentNotFound is returned only when the agent did not exist before the
    /// call; transient errors return ControllerError::Database so the caller
    /// can retry. See REVIEW_LOG.md F7 for the original failure mode.
    fn mark_agent_dead_atomic(
        &self,
        agent_id: &AgentId,
    ) -> impl Future<Output = Result<(), ControllerError>> + Send;

    /// Atomically reserve capacity on the chosen agent AND insert the routing
    /// entry. Returns Ok(true) on success; Ok(false) if the agent's available
    /// capacity is below the request (a concurrent assign won the race). The
    /// requirements are persisted alongside the routing entry so that
    /// release_sandbox can credit the correct amount back without the caller
    /// needing to track it. See REVIEW_LOG.md F5.
    fn try_assign_sandbox(
        &self,
        agent_id: &AgentId,
        sandbox_id: &SandboxId,
        cpu_millicores: u32,
        memory_bytes: u64,
    ) -> impl Future<Output = Result<bool, ControllerError>> + Send;

    /// Atomically release the capacity reserved by try_assign_sandbox AND
    /// remove the routing entry. Returns the agent that was holding the
    /// sandbox, or None if the sandbox had no reservation (idempotent). See
    /// REVIEW_LOG.md F5.
    fn release_sandbox(
        &self,
        sandbox_id: &SandboxId,
    ) -> impl Future<Output = Result<Option<AgentId>, ControllerError>> + Send;
}

#[derive(Debug, Clone)]
pub struct SandboxStateRow {
    pub state: String,
    pub error: Option<String>,
}
