use std::sync::Arc;

use open_sandbox_contracts::error::ControllerError;
use open_sandbox_contracts::types::{AgentId, JoinToken};

use crate::store::{AgentCapacity, AgentRecord, AgentState, AvailableResources, ControllerStore};
use crate::token::TokenValidator;

#[derive(Debug)]
pub enum RegistrationResult {
    Accepted,
    Rejected { reason: String },
}

pub struct AgentRegistry<S: ControllerStore> {
    store: Arc<S>,
    validator: Box<dyn TokenValidator>,
}

impl<S: ControllerStore> AgentRegistry<S> {
    pub fn new(store: Arc<S>, validator: impl TokenValidator + 'static) -> Self {
        Self {
            store,
            validator: Box::new(validator),
        }
    }

    pub async fn register(
        &self,
        agent_id: AgentId,
        token: &JoinToken,
        capacity: AgentCapacity,
    ) -> Result<RegistrationResult, ControllerError> {
        if !self.validator.validate(token.as_str()) {
            return Ok(RegistrationResult::Rejected {
                reason: "invalid join token".into(),
            });
        }

        let record = AgentRecord {
            agent_id,
            available: AvailableResources {
                cpu_millicores: capacity.cpu_cores * 1000,
                memory_bytes: capacity.memory_bytes,
                running_sandboxes: 0,
            },
            capacity,
            state: AgentState::Active,
        };
        self.store.save_agent(record).await?;

        Ok(RegistrationResult::Accepted)
    }

    pub async fn heartbeat(&self, agent_id: &AgentId) -> Result<(), ControllerError> {
        match self.store.get_agent(agent_id).await? {
            Some(_) => Ok(()),
            None => Err(ControllerError::AgentNotFound {
                agent_id: agent_id.to_string(),
            }),
        }
    }

    pub async fn mark_agent_dead(&self, agent_id: &AgentId) -> Result<(), ControllerError> {
        self.store.mark_agent_dead_atomic(agent_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::AgentState;
    use crate::testutil::*;
    use open_sandbox_contracts::types::{RoutingEntry, SandboxId};

    fn test_capacity() -> AgentCapacity {
        AgentCapacity {
            cpu_cores: 4,
            memory_bytes: 8_000_000_000,
        }
    }

    #[tokio::test]
    async fn register_with_valid_token_is_accepted() {
        let store = Arc::new(InMemoryStore::new());
        let registry = AgentRegistry::new(store, AcceptAllTokens);

        let result = registry
            .register(
                AgentId::new(),
                &JoinToken::new("valid-token".into()),
                test_capacity(),
            )
            .await
            .unwrap();

        assert!(matches!(result, RegistrationResult::Accepted));
    }

    #[tokio::test]
    async fn register_with_invalid_token_is_rejected() {
        let store = Arc::new(InMemoryStore::new());
        let registry = AgentRegistry::new(store, RejectAllTokens);

        let result = registry
            .register(
                AgentId::new(),
                &JoinToken::new("bad-token".into()),
                test_capacity(),
            )
            .await
            .unwrap();

        assert!(matches!(result, RegistrationResult::Rejected { .. }));
    }

    #[tokio::test]
    async fn registered_agent_is_persisted() {
        let store = Arc::new(InMemoryStore::new());
        let registry = AgentRegistry::new(store.clone(), AcceptAllTokens);
        let agent_id = AgentId::new();

        registry
            .register(
                agent_id.clone(),
                &JoinToken::new("token".into()),
                test_capacity(),
            )
            .await
            .unwrap();

        let stored = store.get_agent(&agent_id).await.unwrap();
        assert!(stored.is_some());
        let record = stored.unwrap();
        assert_eq!(record.agent_id, agent_id);
        assert_eq!(record.state, AgentState::Active);
    }

    #[tokio::test]
    async fn heartbeat_from_registered_agent_succeeds() {
        let store = Arc::new(InMemoryStore::new());
        let registry = AgentRegistry::new(store, AcceptAllTokens);
        let agent_id = AgentId::new();

        registry
            .register(
                agent_id.clone(),
                &JoinToken::new("token".into()),
                test_capacity(),
            )
            .await
            .unwrap();

        assert!(registry.heartbeat(&agent_id).await.is_ok());
    }

    #[tokio::test]
    async fn heartbeat_from_unknown_agent_fails() {
        let store = Arc::new(InMemoryStore::new());
        let registry = AgentRegistry::new(store, AcceptAllTokens);

        let result = registry.heartbeat(&AgentId::new()).await;
        assert!(matches!(
            result.unwrap_err(),
            ControllerError::AgentNotFound { .. }
        ));
    }

    #[tokio::test]
    async fn mark_dead_rolls_back_state_when_routing_delete_fails() {
        // F7: mark_agent_dead must be atomic. If the second write fails, the
        // first (state→Dead) must be rolled back so the system never observes
        // 'agent is Dead but routing entries still point at it'.
        let store = Arc::new(FailNextStore::new());
        let registry = AgentRegistry::new(store.clone(), AcceptAllTokens);
        let agent_id = AgentId::new();

        registry
            .register(
                agent_id.clone(),
                &JoinToken::new("token".into()),
                test_capacity(),
            )
            .await
            .unwrap();
        store
            .insert_routing_entry(RoutingEntry {
                sandbox_id: SandboxId::new(),
                agent_id: agent_id.clone(),
            })
            .await
            .unwrap();

        store.arm_remove_routing_entries_for_agent_failure();

        let err = registry.mark_agent_dead(&agent_id).await.unwrap_err();
        assert!(matches!(err, ControllerError::Database { .. }));

        // Atomicity: agent state MUST still be Active, routing entry MUST persist.
        let agent = store.get_agent(&agent_id).await.unwrap().unwrap();
        assert_eq!(
            agent.state,
            AgentState::Active,
            "agent state must be rolled back when the second write fails"
        );
        assert_eq!(
            store.inner().routing_entries_for_agent(&agent_id).len(),
            1,
            "routing entry must be untouched when the operation fails"
        );
    }

    #[tokio::test]
    async fn mark_dead_updates_state_and_removes_routing() {
        let store = Arc::new(InMemoryStore::new());
        let registry = AgentRegistry::new(store.clone(), AcceptAllTokens);
        let agent_id = AgentId::new();

        registry
            .register(
                agent_id.clone(),
                &JoinToken::new("token".into()),
                test_capacity(),
            )
            .await
            .unwrap();

        store
            .insert_routing_entry(RoutingEntry {
                sandbox_id: SandboxId::new(),
                agent_id: agent_id.clone(),
            })
            .await
            .unwrap();

        registry.mark_agent_dead(&agent_id).await.unwrap();

        let agent = store.get_agent(&agent_id).await.unwrap().unwrap();
        assert_eq!(agent.state, AgentState::Dead);
        assert!(store.routing_entries_for_agent(&agent_id).is_empty());
    }
}
