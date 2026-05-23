use std::sync::Arc;

use open_sandbox_contracts::error::ControllerError;
use open_sandbox_contracts::types::AgentId;

use crate::store::ControllerStore;

/// Persisted heartbeat tracker.
///
/// All liveness state lives in the store (see `ControllerStore::record_heartbeat`
/// and `dead_agents`) so a controller restart does not lose track of which
/// agents have crashed, and multiple controller replicas observe the same
/// liveness view. See REVIEW_LOG.md F6.
pub struct HeartbeatMonitor<S: ControllerStore> {
    store: Arc<S>,
}

impl<S: ControllerStore> HeartbeatMonitor<S> {
    pub fn new(store: Arc<S>) -> Self {
        Self { store }
    }

    pub async fn record_heartbeat(&self, agent_id: &AgentId) -> Result<(), ControllerError> {
        self.store.record_heartbeat(agent_id).await
    }

    pub async fn dead_agents(&self) -> Result<Vec<AgentId>, ControllerError> {
        self.store.dead_agents().await
    }

    /// Test-visible accessor for the set of agents currently tracked. Returns
    /// the active agents (i.e. those eligible for dead-agent sweeps).
    pub async fn tracked_agents(&self) -> Result<Vec<AgentId>, ControllerError> {
        Ok(self
            .store
            .list_active_agents()
            .await?
            .into_iter()
            .map(|a| a.agent_id)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::*;
    use crate::testutil::*;
    use open_sandbox_contracts::constants::DEAD_AGENT_TIMEOUT;
    use std::time::Duration;

    async fn seed_active_agent(store: &InMemoryStore) -> AgentId {
        let agent_id = AgentId::new();
        store
            .save_agent(AgentRecord {
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
            })
            .await
            .unwrap();
        agent_id
    }

    #[tokio::test(start_paused = true)]
    async fn agent_with_recent_heartbeat_is_alive() {
        let store = Arc::new(InMemoryStore::new());
        let agent_id = seed_active_agent(&store).await;
        let monitor = HeartbeatMonitor::new(store);

        monitor.record_heartbeat(&agent_id).await.unwrap();
        let dead = monitor.dead_agents().await.unwrap();
        assert!(!dead.contains(&agent_id));
    }

    #[tokio::test(start_paused = true)]
    async fn agent_missing_heartbeats_is_detected_dead() {
        let store = Arc::new(InMemoryStore::new());
        let agent_id = seed_active_agent(&store).await;
        let monitor = HeartbeatMonitor::new(store);

        monitor.record_heartbeat(&agent_id).await.unwrap();
        tokio::time::advance(DEAD_AGENT_TIMEOUT + Duration::from_secs(1)).await;

        let dead = monitor.dead_agents().await.unwrap();
        assert!(dead.contains(&agent_id));
    }

    #[tokio::test(start_paused = true)]
    async fn heartbeat_resets_dead_timer() {
        let store = Arc::new(InMemoryStore::new());
        let agent_id = seed_active_agent(&store).await;
        let monitor = HeartbeatMonitor::new(store);

        monitor.record_heartbeat(&agent_id).await.unwrap();
        tokio::time::advance(DEAD_AGENT_TIMEOUT - Duration::from_secs(1)).await;
        monitor.record_heartbeat(&agent_id).await.unwrap();
        tokio::time::advance(Duration::from_secs(2)).await;

        let dead = monitor.dead_agents().await.unwrap();
        assert!(!dead.contains(&agent_id));
    }

    #[tokio::test(start_paused = true)]
    async fn dead_agent_excluded_when_state_flipped_to_dead() {
        // F6 + F7 interaction: once mark_agent_dead_atomic flips state to
        // Dead, dead_agents() must stop returning the agent — otherwise
        // sweep retries forever.
        let store = Arc::new(InMemoryStore::new());
        let agent_id = seed_active_agent(&store).await;
        let monitor = HeartbeatMonitor::new(store.clone());

        monitor.record_heartbeat(&agent_id).await.unwrap();
        tokio::time::advance(DEAD_AGENT_TIMEOUT + Duration::from_secs(1)).await;

        // Flip to Dead via the atomic path.
        store.mark_agent_dead_atomic(&agent_id).await.unwrap();

        let dead = monitor.dead_agents().await.unwrap();
        assert!(
            !dead.contains(&agent_id),
            "agent already flipped to Dead should not be re-surfaced by dead_agents()"
        );
    }
}
