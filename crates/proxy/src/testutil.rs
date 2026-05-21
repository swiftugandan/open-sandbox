use std::collections::HashMap;
use std::sync::Mutex;

use open_sandbox_contracts::error::ProxyError;
use open_sandbox_contracts::types::{AgentId, RoutingEntry, SandboxId};

use crate::routing_store::RoutingStore;

#[derive(Clone)]
pub struct InMemoryRoutingStore {
    entries: std::sync::Arc<Mutex<HashMap<SandboxId, AgentId>>>,
}

impl Default for InMemoryRoutingStore {
    fn default() -> Self {
        Self {
            entries: std::sync::Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl InMemoryRoutingStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_entry(&self, sandbox_id: SandboxId, agent_id: AgentId) {
        self.entries.lock().unwrap().insert(sandbox_id, agent_id);
    }

    pub fn clear(&self) {
        self.entries.lock().unwrap().clear();
    }
}

impl RoutingStore for InMemoryRoutingStore {
    async fn lookup(&self, sandbox_id: &SandboxId) -> Result<Option<AgentId>, ProxyError> {
        Ok(self.entries.lock().unwrap().get(sandbox_id).cloned())
    }

    async fn load_all(&self) -> Result<Vec<RoutingEntry>, ProxyError> {
        Ok(self
            .entries
            .lock()
            .unwrap()
            .iter()
            .map(|(sandbox_id, agent_id)| RoutingEntry {
                sandbox_id: sandbox_id.clone(),
                agent_id: agent_id.clone(),
            })
            .collect())
    }
}
