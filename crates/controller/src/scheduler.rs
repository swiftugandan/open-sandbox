use std::sync::Arc;

use open_sandbox_contracts::error::ControllerError;
use open_sandbox_contracts::types::{AgentId, SandboxId};

use crate::store::ControllerStore;

pub struct SandboxRequirements {
    pub cpu_millicores: u32,
    pub memory_bytes: u64,
}

#[derive(Debug)]
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

    pub fn store(&self) -> &S {
        &self.store
    }

    pub fn store_arc(&self) -> Arc<S> {
        self.store.clone()
    }

    pub async fn assign_sandbox(
        &self,
        sandbox_id: SandboxId,
        requirements: &SandboxRequirements,
    ) -> Result<SandboxAssignment, ControllerError> {
        // F5: walk candidates in best-first order, attempting an atomic
        // reservation on each. try_assign_sandbox returns Ok(false) when a
        // concurrent assign won the capacity race; we try the next candidate
        // rather than overcommitting the same agent.
        let mut agents = self.store.list_active_agents().await?;
        agents.sort_by(|a, b| b.available.cpu_millicores.cmp(&a.available.cpu_millicores));

        for agent in agents {
            if agent.available.cpu_millicores < requirements.cpu_millicores
                || agent.available.memory_bytes < requirements.memory_bytes
            {
                continue;
            }
            let reserved = self
                .store
                .try_assign_sandbox(
                    &agent.agent_id,
                    &sandbox_id,
                    requirements.cpu_millicores,
                    requirements.memory_bytes,
                )
                .await?;
            if reserved {
                return Ok(SandboxAssignment {
                    agent_id: agent.agent_id,
                    sandbox_id,
                });
            }
        }
        Err(ControllerError::NoAvailableAgents)
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
            capacity: AgentCapacity {
                cpu_cores: 4,
                memory_bytes: 8_000_000_000,
            },
            available: AvailableResources {
                cpu_millicores: 2000,
                memory_bytes: 4_000_000_000,
                running_sandboxes: 2,
            },
            state: AgentState::Active,
        };
        let idle_agent = AgentRecord {
            agent_id: AgentId::new(),
            capacity: AgentCapacity {
                cpu_cores: 4,
                memory_bytes: 8_000_000_000,
            },
            available: AvailableResources {
                cpu_millicores: 3500,
                memory_bytes: 7_000_000_000,
                running_sandboxes: 0,
            },
            state: AgentState::Active,
        };
        let expected_id = idle_agent.agent_id.clone();
        store.save_agent(busy_agent).await.unwrap();
        store.save_agent(idle_agent).await.unwrap();

        let scheduler = Scheduler::new(store);
        let requirements = SandboxRequirements {
            cpu_millicores: 1000,
            memory_bytes: 512_000_000,
        };

        let assignment = scheduler
            .assign_sandbox(SandboxId::new(), &requirements)
            .await
            .unwrap();
        assert_eq!(assignment.agent_id, expected_id);
    }

    #[tokio::test]
    async fn no_agents_returns_error() {
        let store = Arc::new(InMemoryStore::new());
        let scheduler = Scheduler::new(store);
        let requirements = SandboxRequirements {
            cpu_millicores: 1000,
            memory_bytes: 512_000_000,
        };

        let result = scheduler
            .assign_sandbox(SandboxId::new(), &requirements)
            .await;
        assert!(matches!(result, Err(ControllerError::NoAvailableAgents)));
    }

    #[tokio::test]
    async fn skips_agents_with_insufficient_resources() {
        let store = Arc::new(InMemoryStore::new());
        let small_agent = AgentRecord {
            agent_id: AgentId::new(),
            capacity: AgentCapacity {
                cpu_cores: 1,
                memory_bytes: 1_000_000_000,
            },
            available: AvailableResources {
                cpu_millicores: 500,
                memory_bytes: 256_000_000,
                running_sandboxes: 1,
            },
            state: AgentState::Active,
        };
        store.save_agent(small_agent).await.unwrap();

        let scheduler = Scheduler::new(store);
        let requirements = SandboxRequirements {
            cpu_millicores: 1000,
            memory_bytes: 512_000_000,
        };

        let result = scheduler
            .assign_sandbox(SandboxId::new(), &requirements)
            .await;
        assert!(matches!(result, Err(ControllerError::NoAvailableAgents)));
    }

    #[tokio::test]
    async fn back_to_back_assigns_debit_capacity_no_overcommit() {
        // F5: scheduler must reserve capacity on assign so concurrent /
        // back-to-back create_sandbox calls don't all pile onto the same
        // 'max available' agent. With one 4000-millicore agent and three
        // 1500-millicore sandboxes, only two should fit; the third must
        // return NoAvailableAgents.
        let store = Arc::new(InMemoryStore::new());
        let agent = AgentRecord {
            agent_id: AgentId::new(),
            capacity: AgentCapacity {
                cpu_cores: 4,
                memory_bytes: 8_000_000_000,
            },
            available: AvailableResources {
                cpu_millicores: 4000,
                memory_bytes: 8_000_000_000,
                running_sandboxes: 0,
            },
            state: AgentState::Active,
        };
        store.save_agent(agent).await.unwrap();
        let scheduler = Scheduler::new(store);
        let req = SandboxRequirements {
            cpu_millicores: 1500,
            memory_bytes: 512_000_000,
        };

        let a1 = scheduler.assign_sandbox(SandboxId::new(), &req).await;
        let a2 = scheduler.assign_sandbox(SandboxId::new(), &req).await;
        let a3 = scheduler.assign_sandbox(SandboxId::new(), &req).await;

        assert!(a1.is_ok(), "first assign should succeed");
        assert!(a2.is_ok(), "second assign should succeed (3000/4000 used)");
        assert!(
            matches!(a3, Err(ControllerError::NoAvailableAgents)),
            "third assign must fail — 4500 > 4000 millicores: {a3:?}"
        );
    }

    #[tokio::test]
    async fn release_sandbox_restores_capacity() {
        // F5: release_sandbox credits the reserved capacity back so a
        // delete_sandbox / send_command rollback path leaves the agent
        // schedulable again.
        let store = Arc::new(InMemoryStore::new());
        let agent_id = AgentId::new();
        let agent = AgentRecord {
            agent_id: agent_id.clone(),
            capacity: AgentCapacity {
                cpu_cores: 4,
                memory_bytes: 8_000_000_000,
            },
            available: AvailableResources {
                cpu_millicores: 4000,
                memory_bytes: 8_000_000_000,
                running_sandboxes: 0,
            },
            state: AgentState::Active,
        };
        store.save_agent(agent).await.unwrap();
        let scheduler = Scheduler::new(store.clone());
        let req = SandboxRequirements {
            cpu_millicores: 3000,
            memory_bytes: 512_000_000,
        };

        let a1 = scheduler.assign_sandbox(SandboxId::new(), &req).await.unwrap();
        // After reservation: 1000 millicores left, can't fit another 3000.
        let blocked = scheduler.assign_sandbox(SandboxId::new(), &req).await;
        assert!(matches!(blocked, Err(ControllerError::NoAvailableAgents)));

        // Release and retry — now there's room.
        let released = store.release_sandbox(&a1.sandbox_id).await.unwrap();
        assert_eq!(released, Some(agent_id));
        let retry = scheduler.assign_sandbox(SandboxId::new(), &req).await;
        assert!(retry.is_ok(), "capacity should be restored after release: {retry:?}");
    }

    #[tokio::test]
    async fn assignment_creates_routing_entry() {
        let store = Arc::new(InMemoryStore::new());
        let agent = AgentRecord {
            agent_id: AgentId::new(),
            capacity: AgentCapacity {
                cpu_cores: 4,
                memory_bytes: 8_000_000_000,
            },
            available: AvailableResources {
                cpu_millicores: 4000,
                memory_bytes: 8_000_000_000,
                running_sandboxes: 0,
            },
            state: AgentState::Active,
        };
        let agent_id = agent.agent_id.clone();
        store.save_agent(agent).await.unwrap();

        let scheduler = Scheduler::new(store.clone());
        let sandbox_id = SandboxId::new();
        let requirements = SandboxRequirements {
            cpu_millicores: 1000,
            memory_bytes: 512_000_000,
        };

        scheduler
            .assign_sandbox(sandbox_id.clone(), &requirements)
            .await
            .unwrap();

        let entries = store.routing_entries_for_agent(&agent_id);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].sandbox_id, sandbox_id);
    }
}
