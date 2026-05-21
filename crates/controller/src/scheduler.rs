use std::sync::Arc;

use open_sandbox_contracts::error::ControllerError;
use open_sandbox_contracts::types::{AgentId, SandboxId};

use crate::store::ControllerStore;

pub struct SandboxRequirements {
    pub cpu_millicores: u32,
    pub memory_bytes: u64,
}

pub struct SandboxAssignment {
    pub agent_id: AgentId,
    pub sandbox_id: SandboxId,
}

pub struct Scheduler<S: ControllerStore> {
    store: Arc<S>,
}

impl<S: ControllerStore> Scheduler<S> {
    pub fn new(store: Arc<S>) -> Self {
        Self { store }
    }

    pub async fn assign_sandbox(
        &self,
        _sandbox_id: SandboxId,
        _requirements: &SandboxRequirements,
    ) -> Result<SandboxAssignment, ControllerError> {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::*;
    use crate::testutil::*;

    #[tokio::test]
    async fn assigns_to_least_loaded_agent() {
        let store = Arc::new(InMemoryStore::new());
        let busy_agent = AgentRecord {
            agent_id: AgentId::new(),
            capacity: AgentCapacity { cpu_cores: 4, memory_bytes: 8_000_000_000 },
            available: AvailableResources { cpu_millicores: 2000, memory_bytes: 4_000_000_000, running_sandboxes: 2 },
            state: AgentState::Active,
        };
        let idle_agent = AgentRecord {
            agent_id: AgentId::new(),
            capacity: AgentCapacity { cpu_cores: 4, memory_bytes: 8_000_000_000 },
            available: AvailableResources { cpu_millicores: 3500, memory_bytes: 7_000_000_000, running_sandboxes: 0 },
            state: AgentState::Active,
        };
        let expected_id = idle_agent.agent_id.clone();
        store.save_agent(busy_agent).await.unwrap();
        store.save_agent(idle_agent).await.unwrap();

        let scheduler = Scheduler::new(store);
        let requirements = SandboxRequirements { cpu_millicores: 1000, memory_bytes: 512_000_000 };

        let assignment = scheduler.assign_sandbox(SandboxId::new(), &requirements).await.unwrap();
        assert_eq!(assignment.agent_id, expected_id);
    }

    #[tokio::test]
    async fn no_agents_returns_error() {
        let store = Arc::new(InMemoryStore::new());
        let scheduler = Scheduler::new(store);
        let requirements = SandboxRequirements { cpu_millicores: 1000, memory_bytes: 512_000_000 };

        let result = scheduler.assign_sandbox(SandboxId::new(), &requirements).await;
        assert!(matches!(result, Err(ControllerError::NoAvailableAgents)));
    }

    #[tokio::test]
    async fn skips_agents_with_insufficient_resources() {
        let store = Arc::new(InMemoryStore::new());
        let small_agent = AgentRecord {
            agent_id: AgentId::new(),
            capacity: AgentCapacity { cpu_cores: 1, memory_bytes: 1_000_000_000 },
            available: AvailableResources { cpu_millicores: 500, memory_bytes: 256_000_000, running_sandboxes: 1 },
            state: AgentState::Active,
        };
        store.save_agent(small_agent).await.unwrap();

        let scheduler = Scheduler::new(store);
        let requirements = SandboxRequirements { cpu_millicores: 1000, memory_bytes: 512_000_000 };

        let result = scheduler.assign_sandbox(SandboxId::new(), &requirements).await;
        assert!(matches!(result, Err(ControllerError::NoAvailableAgents)));
    }

    #[tokio::test]
    async fn assignment_creates_routing_entry() {
        let store = Arc::new(InMemoryStore::new());
        let agent = AgentRecord {
            agent_id: AgentId::new(),
            capacity: AgentCapacity { cpu_cores: 4, memory_bytes: 8_000_000_000 },
            available: AvailableResources { cpu_millicores: 4000, memory_bytes: 8_000_000_000, running_sandboxes: 0 },
            state: AgentState::Active,
        };
        let agent_id = agent.agent_id.clone();
        store.save_agent(agent).await.unwrap();

        let scheduler = Scheduler::new(store.clone());
        let sandbox_id = SandboxId::new();
        let requirements = SandboxRequirements { cpu_millicores: 1000, memory_bytes: 512_000_000 };

        scheduler.assign_sandbox(sandbox_id.clone(), &requirements).await.unwrap();

        let entries = store.routing_entries_for_agent(&agent_id);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].sandbox_id, sandbox_id);
    }
}
