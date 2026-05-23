use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::mpsc;

use open_sandbox_contracts::proxy::TunnelRequest;
use open_sandbox_contracts::types::AgentId;

/// Opaque generation counter that disambiguates successive registrations of
/// the same AgentId. Comp-2 B1: when an agent reconnects, the new tunnel
/// gets a fresh generation; the old OpenTunnel task can then refuse to run
/// its cleanup if the live entry has moved past its own generation.
pub type TunnelGeneration = u64;

pub struct AgentTunnel {
    pub request_tx: mpsc::Sender<TunnelRequest>,
    pub generation: TunnelGeneration,
}

pub struct TunnelPool {
    tunnels: Mutex<HashMap<AgentId, AgentTunnel>>,
    next_generation: AtomicU64,
}

impl Default for TunnelPool {
    fn default() -> Self {
        Self {
            tunnels: Mutex::new(HashMap::new()),
            next_generation: AtomicU64::new(1),
        }
    }
}

impl TunnelPool {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a tunnel for the agent, returning its generation. The caller
    /// MUST pass this generation back to `remove_if_current` on disconnect
    /// so a stale cleanup doesn't deregister a fresher tunnel.
    pub fn register(
        &self,
        agent_id: AgentId,
        request_tx: mpsc::Sender<TunnelRequest>,
    ) -> TunnelGeneration {
        let generation = self.next_generation.fetch_add(1, Ordering::SeqCst);
        self.tunnels.lock().unwrap().insert(
            agent_id,
            AgentTunnel {
                request_tx,
                generation,
            },
        );
        generation
    }

    /// Unconditional removal — used by tests and by sweep_dead_agents-style
    /// paths that aren't subject to the reconnect race.
    pub fn remove(&self, agent_id: &AgentId) {
        self.tunnels.lock().unwrap().remove(agent_id);
    }

    /// Remove only if the live entry's generation matches `generation`.
    /// Returns true if removal happened. Comp-2 B1: this is the OpenTunnel
    /// task's safe cleanup primitive — if a newer tunnel has overwritten
    /// the entry, the live (higher-generation) tunnel must be preserved.
    pub fn remove_if_current(&self, agent_id: &AgentId, generation: TunnelGeneration) -> bool {
        let mut guard = self.tunnels.lock().unwrap();
        match guard.get(agent_id) {
            Some(t) if t.generation == generation => {
                guard.remove(agent_id);
                true
            }
            _ => false,
        }
    }

    pub fn get_sender(&self, agent_id: &AgentId) -> Option<mpsc::Sender<TunnelRequest>> {
        self.tunnels
            .lock()
            .unwrap()
            .get(agent_id)
            .map(|t| t.request_tx.clone())
    }

    /// Returns the (sender, generation) pair so callers can stamp a new
    /// session with the tunnel that created it. The generation is used to
    /// scope cancellation on tunnel disconnect.
    pub fn get_sender_with_generation(
        &self,
        agent_id: &AgentId,
    ) -> Option<(mpsc::Sender<TunnelRequest>, TunnelGeneration)> {
        self.tunnels
            .lock()
            .unwrap()
            .get(agent_id)
            .map(|t| (t.request_tx.clone(), t.generation))
    }

    pub fn active_count(&self) -> usize {
        self.tunnels.lock().unwrap().len()
    }

    pub fn contains(&self, agent_id: &AgentId) -> bool {
        self.tunnels.lock().unwrap().contains_key(agent_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn register_and_retrieve_tunnel() {
        let pool = TunnelPool::new();
        let agent_id = AgentId::new();
        let (tx, _rx) = mpsc::channel(32);

        pool.register(agent_id.clone(), tx);
        assert!(pool.contains(&agent_id));
        assert!(pool.get_sender(&agent_id).is_some());
    }

    #[tokio::test]
    async fn get_sender_returns_none_for_unknown_agent() {
        let pool = TunnelPool::new();
        assert!(pool.get_sender(&AgentId::new()).is_none());
    }

    #[tokio::test]
    async fn remove_drops_tunnel() {
        let pool = TunnelPool::new();
        let agent_id = AgentId::new();
        let (tx, _rx) = mpsc::channel(32);

        pool.register(agent_id.clone(), tx);
        pool.remove(&agent_id);
        assert!(!pool.contains(&agent_id));
        assert!(pool.get_sender(&agent_id).is_none());
    }

    #[tokio::test]
    async fn active_count_tracks_tunnels() {
        let pool = TunnelPool::new();
        assert_eq!(pool.active_count(), 0);

        let a1 = AgentId::new();
        let a2 = AgentId::new();
        let (tx1, _rx1) = mpsc::channel(32);
        let (tx2, _rx2) = mpsc::channel(32);

        pool.register(a1.clone(), tx1);
        assert_eq!(pool.active_count(), 1);

        pool.register(a2.clone(), tx2);
        assert_eq!(pool.active_count(), 2);

        pool.remove(&a1);
        assert_eq!(pool.active_count(), 1);
    }

    #[tokio::test]
    async fn send_request_through_tunnel() {
        let pool = TunnelPool::new();
        let agent_id = AgentId::new();
        let (tx, mut rx) = mpsc::channel(32);

        pool.register(agent_id.clone(), tx);

        let sender = pool.get_sender(&agent_id).unwrap();
        let req = TunnelRequest {
            stream_id: "s1".into(),
            payload: None,
        };
        sender.send(req).await.unwrap();

        let received = rx.recv().await.unwrap();
        assert_eq!(received.stream_id, "s1");
    }
}
