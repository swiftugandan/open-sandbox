use std::collections::HashMap;
use std::sync::Mutex;

use open_sandbox_contracts::error::ControllerError;
use open_sandbox_contracts::types::{AgentId, RoutingEntry, SandboxId};

use crate::store::*;
use crate::token::TokenValidator;

pub struct InMemoryStore {
    agents: Mutex<HashMap<AgentId, AgentRecord>>,
    routing: Mutex<Vec<RoutingEntry>>,
    sandbox_states: Mutex<HashMap<SandboxId, SandboxStateRow>>,
}

impl InMemoryStore {
    pub fn new() -> Self {
        Self {
            agents: Mutex::new(HashMap::new()),
            routing: Mutex::new(Vec::new()),
            sandbox_states: Mutex::new(HashMap::new()),
        }
    }

    pub fn routing_entries_for_agent(&self, agent_id: &AgentId) -> Vec<RoutingEntry> {
        self.routing
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.agent_id == *agent_id)
            .cloned()
            .collect()
    }
}

impl ControllerStore for InMemoryStore {
    async fn save_agent(&self, record: AgentRecord) -> Result<(), ControllerError> {
        self.agents
            .lock()
            .unwrap()
            .insert(record.agent_id.clone(), record);
        Ok(())
    }

    async fn get_agent(&self, id: &AgentId) -> Result<Option<AgentRecord>, ControllerError> {
        Ok(self.agents.lock().unwrap().get(id).cloned())
    }

    async fn remove_agent(&self, id: &AgentId) -> Result<(), ControllerError> {
        self.agents.lock().unwrap().remove(id);
        Ok(())
    }

    async fn list_active_agents(&self) -> Result<Vec<AgentRecord>, ControllerError> {
        Ok(self
            .agents
            .lock()
            .unwrap()
            .values()
            .filter(|a| a.state == AgentState::Active)
            .cloned()
            .collect())
    }

    async fn update_agent_state(
        &self,
        id: &AgentId,
        state: AgentState,
    ) -> Result<(), ControllerError> {
        match self.agents.lock().unwrap().get_mut(id) {
            Some(agent) => {
                agent.state = state;
                Ok(())
            }
            None => Err(ControllerError::AgentNotFound {
                agent_id: id.to_string(),
            }),
        }
    }

    async fn insert_routing_entry(&self, entry: RoutingEntry) -> Result<(), ControllerError> {
        self.routing.lock().unwrap().push(entry);
        Ok(())
    }

    async fn remove_routing_entries_for_agent(
        &self,
        agent_id: &AgentId,
    ) -> Result<(), ControllerError> {
        self.routing
            .lock()
            .unwrap()
            .retain(|e| e.agent_id != *agent_id);
        Ok(())
    }

    async fn find_routing_entry(
        &self,
        sandbox_id: &SandboxId,
    ) -> Result<Option<RoutingEntry>, ControllerError> {
        Ok(self
            .routing
            .lock()
            .unwrap()
            .iter()
            .find(|e| e.sandbox_id == *sandbox_id)
            .cloned())
    }

    async fn list_routing_entries(&self) -> Result<Vec<RoutingEntry>, ControllerError> {
        Ok(self.routing.lock().unwrap().clone())
    }

    async fn remove_routing_entry(&self, sandbox_id: &SandboxId) -> Result<(), ControllerError> {
        self.routing
            .lock()
            .unwrap()
            .retain(|e| e.sandbox_id != *sandbox_id);
        Ok(())
    }

    async fn save_sandbox_state(
        &self,
        sandbox_id: &SandboxId,
        _agent_id: &AgentId,
        state: &str,
        error: Option<&str>,
    ) -> Result<(), ControllerError> {
        self.sandbox_states.lock().unwrap().insert(
            sandbox_id.clone(),
            SandboxStateRow {
                state: state.to_string(),
                error: error.map(|s| s.to_string()),
            },
        );
        Ok(())
    }

    async fn get_sandbox_state(
        &self,
        sandbox_id: &SandboxId,
    ) -> Result<Option<SandboxStateRow>, ControllerError> {
        Ok(self.sandbox_states.lock().unwrap().get(sandbox_id).cloned())
    }

    async fn mark_agent_dead_atomic(
        &self,
        agent_id: &AgentId,
    ) -> Result<(), ControllerError> {
        // In-memory atomicity: hold both locks for the whole critical section.
        // Ordering is fixed (agents → routing) so deadlock is impossible.
        let mut agents = self.agents.lock().unwrap();
        let mut routing = self.routing.lock().unwrap();
        match agents.get_mut(agent_id) {
            Some(agent) => {
                agent.state = AgentState::Dead;
                routing.retain(|e| e.agent_id != *agent_id);
                Ok(())
            }
            None => Err(ControllerError::AgentNotFound {
                agent_id: agent_id.to_string(),
            }),
        }
    }
}

pub struct AcceptAllTokens;

impl TokenValidator for AcceptAllTokens {
    fn validate(&self, _token: &str) -> bool {
        true
    }
}

pub struct RejectAllTokens;

impl TokenValidator for RejectAllTokens {
    fn validate(&self, _token: &str) -> bool {
        false
    }
}

/// Wraps an inner store and injects a Database error on the chosen method
/// the next time it's called, then passes through.
pub struct FailNextStore {
    inner: InMemoryStore,
    fail_update_agent_state: Mutex<bool>,
    fail_remove_routing_entries_for_agent: Mutex<bool>,
}

impl FailNextStore {
    pub fn new() -> Self {
        Self {
            inner: InMemoryStore::new(),
            fail_update_agent_state: Mutex::new(false),
            fail_remove_routing_entries_for_agent: Mutex::new(false),
        }
    }

    pub fn arm_update_agent_state_failure(&self) {
        *self.fail_update_agent_state.lock().unwrap() = true;
    }

    pub fn arm_remove_routing_entries_for_agent_failure(&self) {
        *self.fail_remove_routing_entries_for_agent.lock().unwrap() = true;
    }

    pub fn inner(&self) -> &InMemoryStore {
        &self.inner
    }
}

impl Default for FailNextStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ControllerStore for FailNextStore {
    async fn save_agent(&self, record: AgentRecord) -> Result<(), ControllerError> {
        self.inner.save_agent(record).await
    }

    async fn get_agent(&self, id: &AgentId) -> Result<Option<AgentRecord>, ControllerError> {
        self.inner.get_agent(id).await
    }

    async fn remove_agent(&self, id: &AgentId) -> Result<(), ControllerError> {
        self.inner.remove_agent(id).await
    }

    async fn list_active_agents(&self) -> Result<Vec<AgentRecord>, ControllerError> {
        self.inner.list_active_agents().await
    }

    async fn update_agent_state(
        &self,
        id: &AgentId,
        state: AgentState,
    ) -> Result<(), ControllerError> {
        let should_fail = {
            let mut armed = self.fail_update_agent_state.lock().unwrap();
            let was = *armed;
            *armed = false;
            was
        };
        if should_fail {
            return Err(ControllerError::Database {
                detail: "injected failure".into(),
            });
        }
        self.inner.update_agent_state(id, state).await
    }

    async fn insert_routing_entry(&self, entry: RoutingEntry) -> Result<(), ControllerError> {
        self.inner.insert_routing_entry(entry).await
    }

    async fn remove_routing_entries_for_agent(
        &self,
        agent_id: &AgentId,
    ) -> Result<(), ControllerError> {
        self.inner.remove_routing_entries_for_agent(agent_id).await
    }

    async fn find_routing_entry(
        &self,
        sandbox_id: &SandboxId,
    ) -> Result<Option<RoutingEntry>, ControllerError> {
        self.inner.find_routing_entry(sandbox_id).await
    }

    async fn list_routing_entries(&self) -> Result<Vec<RoutingEntry>, ControllerError> {
        self.inner.list_routing_entries().await
    }

    async fn remove_routing_entry(&self, sandbox_id: &SandboxId) -> Result<(), ControllerError> {
        self.inner.remove_routing_entry(sandbox_id).await
    }

    async fn save_sandbox_state(
        &self,
        sandbox_id: &SandboxId,
        agent_id: &AgentId,
        state: &str,
        error: Option<&str>,
    ) -> Result<(), ControllerError> {
        self.inner
            .save_sandbox_state(sandbox_id, agent_id, state, error)
            .await
    }

    async fn get_sandbox_state(
        &self,
        sandbox_id: &SandboxId,
    ) -> Result<Option<SandboxStateRow>, ControllerError> {
        self.inner.get_sandbox_state(sandbox_id).await
    }

    async fn mark_agent_dead_atomic(
        &self,
        agent_id: &AgentId,
    ) -> Result<(), ControllerError> {
        let (should_fail_state, should_fail_remove) = {
            let mut state_armed = self.fail_update_agent_state.lock().unwrap();
            let mut remove_armed = self.fail_remove_routing_entries_for_agent.lock().unwrap();
            let s = *state_armed;
            let r = *remove_armed;
            *state_armed = false;
            *remove_armed = false;
            (s, r)
        };
        if should_fail_state || should_fail_remove {
            // Simulate the txn aborting: no side-effects. Verify the agent
            // exists so the returned error semantics match a real failure
            // (Database vs AgentNotFound).
            if self.inner.get_agent(agent_id).await?.is_none() {
                return Err(ControllerError::AgentNotFound {
                    agent_id: agent_id.to_string(),
                });
            }
            return Err(ControllerError::Database {
                detail: "injected mark_agent_dead_atomic failure".into(),
            });
        }
        self.inner.mark_agent_dead_atomic(agent_id).await
    }
}
