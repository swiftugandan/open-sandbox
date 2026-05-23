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

pub struct IoSessionRecord {
    pub agent_id: AgentId,
    /// Sender to the gateway-side outbound stream (the response
    /// stream of OpenIoStream). The proxy pushes `IoServerFrame`s
    /// here that arrive from the agent.
    pub server_tx: mpsc::Sender<Result<IoServerFrame, Status>>,
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

    /// Forward an `IoServerFrame` from the agent's tunnel into the
    /// gateway-side outbound stream. Returns true if a session was
    /// found and the frame was queued; false if no such session
    /// exists (the gateway already disconnected, etc.).
    ///
    /// Uses `try_send` to avoid blocking the agent tunnel pump when
    /// the gateway is slow. The bounded channel sized in
    /// `OpenIoStream` (32 frames) provides headroom; if it's full,
    /// the agent's tunnel-side pump backpressures naturally because
    /// we drop the frame here and the agent retries — wait, that's
    /// wrong. Better: use `send().await` so backpressure propagates.
    ///
    /// Actually the cleanest model is: the tunnel-side dispatch
    /// task awaits this send. If the gateway is slow, the await
    /// blocks the tunnel dispatcher, which blocks the agent's
    /// outbound, which blocks the in-container process. That's the
    /// backpressure chain we want.
    pub async fn deliver_server_frame(&self, stream_id: &str, frame: IoServerFrame) -> bool {
        let sender = {
            let guard = self.inner.lock().unwrap();
            guard.get(stream_id).map(|r| r.server_tx.clone())
        };
        match sender {
            Some(tx) => tx.send(Ok(frame)).await.is_ok(),
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

    /// When an agent disconnects, terminate all sessions routing
    /// through it. The gateway sees an Unavailable status on each.
    pub fn cancel_agent_streams(&self, agent_id: &AgentId) {
        let mut guard = self.inner.lock().unwrap();
        let stream_ids: Vec<String> = guard
            .iter()
            .filter(|(_, r)| r.agent_id == *agent_id)
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
        assert!(!s.deliver_server_frame("bogus", frame).await);
    }

    #[tokio::test]
    async fn deliver_to_known_stream_forwards_frame() {
        let s = IoSessions::new();
        let (tx, mut rx) = mpsc::channel(8);
        s.insert(
            "io-0".into(),
            IoSessionRecord {
                agent_id: AgentId::new(),
                server_tx: tx,
            },
        );
        let frame = IoServerFrame {
            stream_id: "io-0".into(),
            payload: Some(io_server_frame::Payload::Exited(IoExited {
                exit_code: 0,
                command_not_found: false,
            })),
        };
        assert!(s.deliver_server_frame("io-0", frame).await);
        let received = rx.recv().await.unwrap().unwrap();
        assert_eq!(received.stream_id, "io-0");
    }

    #[tokio::test]
    async fn cancel_agent_streams_fails_only_that_agents_sessions() {
        let s = IoSessions::new();
        let agent_a = AgentId::new();
        let agent_b = AgentId::new();
        let (tx_a, mut rx_a) = mpsc::channel(8);
        let (tx_b, mut rx_b) = mpsc::channel(8);
        s.insert(
            "io-a".into(),
            IoSessionRecord {
                agent_id: agent_a.clone(),
                server_tx: tx_a,
            },
        );
        s.insert(
            "io-b".into(),
            IoSessionRecord {
                agent_id: agent_b.clone(),
                server_tx: tx_b,
            },
        );

        s.cancel_agent_streams(&agent_a);

        let received_a = rx_a.try_recv().unwrap();
        assert!(received_a.is_err());

        // B still alive.
        assert_eq!(s.len(), 1);
        assert!(rx_b.try_recv().is_err()); // nothing yet
    }
}
