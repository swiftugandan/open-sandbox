use std::collections::HashMap;
use std::sync::Mutex;

use open_sandbox_contracts::error::ControllerError;
use open_sandbox_contracts::types::{AgentId, RoutingEntry, SandboxId};

use crate::store::*;
use crate::token::TokenValidator;

pub struct InMemoryStore {
    agents: Mutex<HashMap<AgentId, AgentRecord>>,
    routing: Mutex<Vec<RoutingEntry>>,
}

impl InMemoryStore {
    pub fn new() -> Self {
        Self {
            agents: Mutex::new(HashMap::new()),
            routing: Mutex::new(Vec::new()),
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

    async fn remove_routing_entry(&self, sandbox_id: &SandboxId) -> Result<(), ControllerError> {
        self.routing
            .lock()
            .unwrap()
            .retain(|e| e.sandbox_id != *sandbox_id);
        Ok(())
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
