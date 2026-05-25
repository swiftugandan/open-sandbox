//! IoSessions — keyed by stream_id, holds the gateway-side outbound
//! channel for each live gateway-originated I/O session.
//!
//! When the agent emits an `IoServerFrame` (via
//! `TunnelResponse.io_server`), the proxy's `OpenTunnel` handler
//! looks the stream_id up here and forwards the frame to the
//! waiting gateway-side stream.
//!
//! Sibling of `StreamMux` (which handles unary HTTP request/response
//! pairs). Different shape because I/O sessions are bidirectional
//! streams, not single-response RPCs.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::mpsc;
use tonic::Status;

use open_sandbox_contracts::proxy::IoServerFrame;
use open_sandbox_contracts::types::AgentId;

use crate::tunnel_pool::TunnelGeneration;

pub struct IoSessionRecord {
    pub agent_id: AgentId,
    /// Sender to the gateway-side outbound stream (the response
    /// stream of OpenIoStream). The proxy pushes `IoServerFrame`s
    /// here that arrive from the agent.
    pub server_tx: mpsc::Sender<Result<IoServerFrame, Status>>,
    /// Tunnel generation this session was opened on. Comp-2 B1: scope
    /// cancel_agent_streams to a specific tunnel generation so an old
    /// OpenTunnel task's late cleanup doesn't kill sessions opened on the
    /// agent's reconnected tunnel.
    pub generation: TunnelGeneration,
}

pub struct IoSessions {
    inner: Mutex<HashMap<String, IoSessionRecord>>,
    counter: AtomicU64,
}

impl Default for IoSessions {
    fn default() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            counter: AtomicU64::new(0),
        }
    }
}

impl IoSessions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn next_stream_id(&self) -> String {
        let id = self.counter.fetch_add(1, Ordering::SeqCst);
        format!("io-{id}")
    }

    pub fn insert(&self, stream_id: String, record: IoSessionRecord) {
        self.inner.lock().unwrap().insert(stream_id, record);
    }

    pub fn remove(&self, stream_id: &str) -> Option<IoSessionRecord> {
        self.inner.lock().unwrap().remove(stream_id)
    }

    /// Forward an `IoServerFrame` from the agent's tunnel into the gateway-
    /// side outbound stream.
    ///
    /// Comp-2 A2: requires `from_agent` to match the session's owning agent
    /// (frame-ownership check; mismatches are dropped with a warn).
    ///
    /// Comp-2 B2 / comp-3 A3: uses `try_send` rather than `send().await`,
    /// trading session-level backpressure for tunnel-wide HoL isolation.
    /// One slow gateway consumer no longer blocks the tunnel pump (which
    /// would freeze every OTHER session on the same agent). On a full
    /// per-session channel (capacity raised to IO_SESSION_BUFFER frames in
    /// grpc::dispatch_io_stream), the offending frame is dropped with a
    /// warn; the session continues if the consumer recovers.
    ///
    /// Returns true on forward, false if the stream is unknown, the
    /// carrier doesn't own it, OR the per-session channel is full.
    pub fn deliver_server_frame(
        &self,
        stream_id: &str,
        from_agent: &AgentId,
        frame: IoServerFrame,
    ) -> bool {
        let sender = {
            let guard = self.inner.lock().unwrap();
            match guard.get(stream_id) {
                Some(r) if r.agent_id == *from_agent => Some(r.server_tx.clone()),
                Some(r) => {
                    tracing::warn!(
                        stream_id = %stream_id,
                        carrier = %from_agent,
                        owner = %r.agent_id,
                        "deliver_server_frame: carrier does not own session; dropping"
                    );
                    None
                }
                None => None,
            }
        };
        match sender {
            Some(tx) => match tx.try_send(Ok(frame)) {
                Ok(()) => true,
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    tracing::warn!(
                        stream_id = %stream_id,
                        "deliver_server_frame: per-session channel full; dropping frame to preserve tunnel HoL isolation"
                    );
                    false
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => false,
            },
            None => false,
        }
    }

    /// Fail a session (e.g., the agent dropped). Sends an error
    /// status to the gateway-side stream and removes the session.
    pub fn fail_stream(&self, stream_id: &str, status: Status) {
        let record = self.inner.lock().unwrap().remove(stream_id);
        if let Some(rec) = record {
            // Best-effort: gateway may have already gone.
            let _ = rec.server_tx.try_send(Err(status));
        }
    }

    /// When an agent's tunnel disconnects, terminate all sessions opened on
    /// that specific generation. Comp-2 B1: sessions opened on a newer
    /// generation (an agent reconnect) are NOT canceled.
    pub fn cancel_agent_streams_at_generation(
        &self,
        agent_id: &AgentId,
        generation: TunnelGeneration,
    ) {
        let mut guard = self.inner.lock().unwrap();
        let stream_ids: Vec<String> = guard
            .iter()
            .filter(|(_, r)| r.agent_id == *agent_id && r.generation == generation)
            .map(|(k, _)| k.clone())
            .collect();
        for sid in stream_ids {
            if let Some(rec) = guard.remove(&sid) {
                let _ = rec.server_tx.try_send(Err(Status::unavailable(format!(
                    "agent {agent_id} disconnected"
                ))));
            }
        }
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().unwrap().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use open_sandbox_contracts::proxy::{IoExited, io_server_frame};

    #[tokio::test]
    async fn deliver_to_unknown_stream_returns_false() {
        let s = IoSessions::new();
        let frame = IoServerFrame {
            stream_id: "bogus".into(),
            payload: None,
        };
        assert!(!s.deliver_server_frame("bogus", &AgentId::new(), frame));
    }

    #[tokio::test]
    async fn deliver_to_known_stream_forwards_frame() {
        let s = IoSessions::new();
        let (tx, mut rx) = mpsc::channel(8);
        let agent_id = AgentId::new();
        s.insert(
            "io-0".into(),
            IoSessionRecord {
                agent_id: agent_id.clone(),
                server_tx: tx,
                generation: 1,
            },
        );
        let frame = IoServerFrame {
            stream_id: "io-0".into(),
            payload: Some(io_server_frame::Payload::Exited(IoExited {
                exit_code: 0,
                command_not_found: false,
            })),
        };
        assert!(s.deliver_server_frame("io-0", &agent_id, frame));
        let received = rx.recv().await.unwrap().unwrap();
        assert_eq!(received.stream_id, "io-0");
    }

    #[tokio::test]
    async fn deliver_from_wrong_agent_is_dropped() {
        // Comp-2 A2: a frame delivered by a different agent than the
        // session's owner must NOT be forwarded.
        let s = IoSessions::new();
        let (tx, mut rx) = mpsc::channel(8);
        let owner = AgentId::new();
        let attacker = AgentId::new();
        s.insert(
            "io-0".into(),
            IoSessionRecord {
                agent_id: owner,
                server_tx: tx,
                generation: 1,
            },
        );
        let frame = IoServerFrame {
            stream_id: "io-0".into(),
            payload: None,
        };
        assert!(!s.deliver_server_frame("io-0", &attacker, frame));
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn cancel_only_cancels_matching_generation() {
        // Comp-2 B1: an old-tunnel cleanup must not kill sessions opened on a
        // later (reconnected) tunnel for the same AgentId.
        let s = IoSessions::new();
        let agent = AgentId::new();
        let (tx_old, mut rx_old) = mpsc::channel(8);
        let (tx_new, mut rx_new) = mpsc::channel(8);
        s.insert(
            "io-old".into(),
            IoSessionRecord {
                agent_id: agent.clone(),
                server_tx: tx_old,
                generation: 1,
            },
        );
        s.insert(
            "io-new".into(),
            IoSessionRecord {
                agent_id: agent.clone(),
                server_tx: tx_new,
                generation: 2,
            },
        );

        // Old tunnel disconnect cancels only generation 1.
        s.cancel_agent_streams_at_generation(&agent, 1);

        assert!(rx_old.try_recv().unwrap().is_err());
        assert!(rx_new.try_recv().is_err()); // new still alive
        assert_eq!(s.len(), 1);
    }
}
