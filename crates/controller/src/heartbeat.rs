use std::collections::HashMap;
use std::sync::Mutex;

use open_sandbox_contracts::types::AgentId;

pub struct HeartbeatMonitor {
    heartbeats: Mutex<HashMap<AgentId, tokio::time::Instant>>,
}

impl HeartbeatMonitor {
    pub fn new() -> Self {
        Self {
            heartbeats: Mutex::new(HashMap::new()),
        }
    }

    pub fn record_heartbeat(&self, _agent_id: AgentId) {
        todo!()
    }

    pub fn remove(&self, _agent_id: &AgentId) {
        todo!()
    }

    pub fn dead_agents(&self) -> Vec<AgentId> {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use open_sandbox_contracts::constants::DEAD_AGENT_TIMEOUT;

    #[tokio::test(start_paused = true)]
    async fn agent_with_recent_heartbeat_is_alive() {
        let monitor = HeartbeatMonitor::new();
        let agent_id = AgentId::new();

        monitor.record_heartbeat(agent_id.clone());

        let dead = monitor.dead_agents();
        assert!(!dead.contains(&agent_id));
    }

    #[tokio::test(start_paused = true)]
    async fn agent_missing_heartbeats_is_detected_dead() {
        let monitor = HeartbeatMonitor::new();
        let agent_id = AgentId::new();

        monitor.record_heartbeat(agent_id.clone());
        tokio::time::advance(DEAD_AGENT_TIMEOUT + Duration::from_secs(1)).await;

        let dead = monitor.dead_agents();
        assert!(dead.contains(&agent_id));
    }

    #[tokio::test(start_paused = true)]
    async fn heartbeat_resets_dead_timer() {
        let monitor = HeartbeatMonitor::new();
        let agent_id = AgentId::new();

        monitor.record_heartbeat(agent_id.clone());
        tokio::time::advance(DEAD_AGENT_TIMEOUT - Duration::from_secs(1)).await;
        monitor.record_heartbeat(agent_id.clone());
        tokio::time::advance(Duration::from_secs(2)).await;

        let dead = monitor.dead_agents();
        assert!(!dead.contains(&agent_id));
    }

    #[tokio::test(start_paused = true)]
    async fn removed_agent_not_reported_as_dead() {
        let monitor = HeartbeatMonitor::new();
        let agent_id = AgentId::new();

        monitor.record_heartbeat(agent_id.clone());
        monitor.remove(&agent_id);
        tokio::time::advance(DEAD_AGENT_TIMEOUT + Duration::from_secs(1)).await;

        let dead = monitor.dead_agents();
        assert!(!dead.contains(&agent_id));
    }
}
