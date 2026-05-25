use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;

use open_sandbox_contracts::error::AgentError;
use open_sandbox_contracts::proxy::sandbox_io_service_client::SandboxIoServiceClient;
use open_sandbox_contracts::proxy::{
    HttpResponse, IoClientFrame, IoServerFrame, TunnelReady, TunnelResponse, io_client_frame,
    tunnel_request, tunnel_response,
};
use open_sandbox_contracts::types::{AgentId, SandboxId};

use crate::container::ContainerRuntime;
use crate::io_stream::drive_io_session;
use crate::tunnel::{ForwardRequest, HttpClient, TunnelForwarder};

pub struct ProxyConnection<R: ContainerRuntime, H: HttpClient> {
    agent_id: AgentId,
    forwarder: Arc<TunnelForwarder<R, H>>,
    /// Optional bearer token presented to OpenTunnel. Comp-2 A1: the proxy's
    /// public listener requires this in production so a network-reachable
    /// attacker cannot register as an arbitrary agent_id and hijack routing.
    tunnel_token: Option<String>,
}

/// Live IoStream sessions keyed by proxy-assigned stream_id. Holds
/// the sender side of the per-session client-frames channel that
/// drive_io_session reads from.
type IoSessions = Arc<Mutex<HashMap<String, mpsc::Sender<IoClientFrame>>>>;

impl<R: ContainerRuntime + 'static, H: HttpClient + 'static> ProxyConnection<R, H> {
    pub fn new(agent_id: AgentId, forwarder: Arc<TunnelForwarder<R, H>>) -> Self {
        Self::with_token(agent_id, forwarder, None)
    }

    pub fn with_token(
        agent_id: AgentId,
        forwarder: Arc<TunnelForwarder<R, H>>,
        tunnel_token: Option<String>,
    ) -> Self {
        Self {
            agent_id,
            forwarder,
            tunnel_token,
        }
    }

    pub async fn run(&self, addr: &str) -> Result<(), AgentError> {
        // Comp-3 B6: client-side HTTP/2 keepalive so a silently-broken
        // tunnel (proxy frozen, NAT idle timeout, middle-box drop) is
        // detected within ~35s rather than OS TCP-keepalive minutes.
        // Mirrors the server-side keepalive comp-2 B4 added on the proxy.
        let channel = tonic::transport::Channel::from_shared(addr.to_string())
            .map_err(|e| AgentError::Internal {
                detail: e.to_string(),
            })?
            .keep_alive_while_idle(true)
            .keep_alive_timeout(std::time::Duration::from_secs(20))
            .http2_keep_alive_interval(std::time::Duration::from_secs(15))
            .connect()
            .await
            .map_err(|_| AgentError::TunnelDisconnected)?;

        let mut client = SandboxIoServiceClient::new(channel);

        let (outbound_tx, outbound_rx) = mpsc::channel(32);
        let outbound_stream = ReceiverStream::new(outbound_rx);

        let mut tunnel_request = tonic::Request::new(outbound_stream);
        if let Some(token) = &self.tunnel_token {
            let value = format!("Bearer {token}")
                .parse()
                .map_err(|e: tonic::metadata::errors::InvalidMetadataValue| {
                    AgentError::Internal {
                        detail: e.to_string(),
                    }
                })?;
            tunnel_request.metadata_mut().insert("authorization", value);
        }

        let response = client
            .open_tunnel(tunnel_request)
            .await
            .map_err(|_| AgentError::TunnelDisconnected)?;
        let mut inbound = response.into_inner();

        let ready = TunnelResponse {
            stream_id: String::new(),
            payload: Some(tunnel_response::Payload::Ready(TunnelReady {
                agent_id: self.agent_id.to_string(),
            })),
        };
        outbound_tx
            .send(ready)
            .await
            .map_err(|_| AgentError::TunnelDisconnected)?;
        tracing::info!(agent_id = %self.agent_id, "proxy tunnel established");

        let io_sessions: IoSessions = Arc::new(Mutex::new(HashMap::new()));
        let forwarder = self.forwarder.clone();

        // Comp-3 A4: track every per-session spawned task in a JoinSet so
        // we can abort them on tunnel disconnect. Without this, each
        // drive_io_session lingers in cleanup() for EXEC_KILL_GRACE
        // before noticing its in_tx closed — under reconnect storms
        // (now common since A1 added reconnect loops) this accumulates
        // unboundedly.
        let mut session_tasks: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();

        while let Ok(Some(req)) = inbound.message().await {
            let stream_id = req.stream_id.clone();
            let Some(payload) = req.payload else {
                continue;
            };

            match payload {
                tunnel_request::Payload::HttpRequest(http_req) => {
                    let sandbox_id = open_sandbox_contracts::types::SandboxId::from(
                        uuid::Uuid::parse_str(&http_req.sandbox_id).map_err(|e| {
                            AgentError::Internal {
                                detail: e.to_string(),
                            }
                        })?,
                    );

                    let forward_req = ForwardRequest {
                        method: http_req.method,
                        uri: http_req.uri,
                        headers: http_req.headers,
                        body: http_req.body,
                    };

                    let resp = match forwarder.forward(&sandbox_id, forward_req).await {
                        Ok(r) => TunnelResponse {
                            stream_id,
                            payload: Some(tunnel_response::Payload::HttpResponse(HttpResponse {
                                status_code: r.status_code,
                                headers: r.headers,
                                body: r.body,
                            })),
                        },
                        Err(e) => TunnelResponse {
                            stream_id,
                            payload: Some(tunnel_response::Payload::Close(
                                open_sandbox_contracts::proxy::StreamClose {
                                    reason: e.to_string(),
                                },
                            )),
                        },
                    };

                    let _ = outbound_tx.send(resp).await;
                }
                tunnel_request::Payload::Data(_) | tunnel_request::Payload::Close(_) => {}
                tunnel_request::Payload::IoClient(io_frame) => {
                    handle_io_client_frame(
                        &forwarder,
                        &io_sessions,
                        &outbound_tx,
                        &mut session_tasks,
                        io_frame,
                    )
                    .await;
                }
            }
        }

        // Comp-3 A4: tunnel disconnected. Abort every per-session task
        // immediately instead of letting each drive_io_session sleep
        // EXEC_KILL_GRACE after noticing its in_tx closed.
        session_tasks.shutdown().await;

        Err(AgentError::TunnelDisconnected)
    }
}

async fn handle_io_client_frame<R: ContainerRuntime + 'static, H: HttpClient + 'static>(
    forwarder: &Arc<TunnelForwarder<R, H>>,
    io_sessions: &IoSessions,
    outbound_tx: &mpsc::Sender<TunnelResponse>,
    session_tasks: &mut tokio::task::JoinSet<()>,
    io_frame: IoClientFrame,
) {
    let stream_id = io_frame.stream_id.clone();

    // If this is the first frame (Start), spawn the per-session
    // drive_io_session task. Otherwise route to the existing session.
    let is_start = matches!(io_frame.payload, Some(io_client_frame::Payload::Start(_)));

    if is_start {
        let start_inner = match &io_frame.payload {
            Some(io_client_frame::Payload::Start(s)) => s,
            _ => unreachable!(),
        };

        let sandbox_id = match uuid::Uuid::parse_str(&start_inner.sandbox_id).map(SandboxId::from) {
            Ok(id) => id,
            Err(_) => {
                let _ = outbound_tx
                    .send(io_error_response(
                        &stream_id,
                        "INVALID_REQUEST",
                        "invalid sandbox_id",
                    ))
                    .await;
                return;
            }
        };

        let container_id = match forwarder.sandbox_manager().container_id_for(&sandbox_id) {
            Ok(c) => c,
            Err(e) => {
                let _ = outbound_tx
                    .send(io_error_response(
                        &stream_id,
                        "SANDBOX_NOT_FOUND",
                        &e.to_string(),
                    ))
                    .await;
                return;
            }
        };

        // Comp-3 A3: per-session client-frame buffer raised so one slow
        // drive_io_session task doesn't head-of-line block the tunnel
        // inbound loop. The tunnel-inbound path below uses try_send and
        // drops on overflow.
        const IO_SESSION_CLIENT_BUFFER: usize = 256;
        let (in_tx, in_rx) = mpsc::channel::<IoClientFrame>(IO_SESSION_CLIENT_BUFFER);
        let (server_tx, mut server_rx) = mpsc::channel::<IoServerFrame>(32);

        // Comp-3 C5: defensive — reject IoStart for an already-active
        // stream_id rather than silently overwriting and orphaning the
        // original session. Proxy currently mints sequential `io-N` ids
        // and shouldn't collide, but a malformed or compromised proxy
        // peer could.
        //
        // Scope the MutexGuard tightly so it doesn't live across the
        // outbound_tx.send().await on the conflict path (std::sync::Mutex
        // guards aren't Send).
        let is_conflict = {
            let mut sessions = io_sessions.lock().unwrap();
            if sessions.contains_key(&stream_id) {
                true
            } else {
                sessions.insert(stream_id.clone(), in_tx.clone());
                false
            }
        };
        if is_conflict {
            let _ = outbound_tx
                .send(io_error_response(
                    &stream_id,
                    "STREAM_ID_REUSED",
                    "an IoStream with this stream_id is already active",
                ))
                .await;
            return;
        }

        // Forward the initial Start frame.
        let _ = in_tx.send(io_frame).await;

        // Spawn the driver. Comp-3 A4: registered in session_tasks so
        // tunnel disconnect aborts it immediately rather than waiting
        // for the cleanup hook's EXEC_KILL_GRACE.
        let runtime = forwarder.sandbox_manager().runtime().clone();
        let registry = forwarder.registry().clone();
        let client_stream = ReceiverStream::new(in_rx).map(Ok::<_, AgentError>);
        let stream_id_for_drive = stream_id.clone();
        session_tasks.spawn(drive_io_session(
            runtime,
            registry,
            stream_id_for_drive,
            sandbox_id,
            container_id,
            client_stream,
            server_tx,
        ));

        // Spawn the outbound wrapper: server_rx → TunnelResponse::IoServer.
        let outbound = outbound_tx.clone();
        let sessions = io_sessions.clone();
        let stream_id_for_pump = stream_id.clone();
        session_tasks.spawn(async move {
            while let Some(server_frame) = server_rx.recv().await {
                let resp = TunnelResponse {
                    stream_id: stream_id_for_pump.clone(),
                    payload: Some(tunnel_response::Payload::IoServer(server_frame)),
                };
                if outbound.send(resp).await.is_err() {
                    break;
                }
            }
            // Session ended — remove from registry.
            sessions.lock().unwrap().remove(&stream_id_for_pump);
        });
    } else {
        // Subsequent frame: route to existing session.
        // Comp-3 A3: try_send so a slow drive_io_session doesn't block
        // the tunnel inbound pump (which would HoL every other session
        // on this agent). Overflow drops the frame with a warn; the
        // 256-slot buffer above absorbs short stalls.
        let tx = io_sessions.lock().unwrap().get(&stream_id).cloned();
        match tx {
            Some(t) => {
                if let Err(e) = t.try_send(io_frame) {
                    match e {
                        mpsc::error::TrySendError::Full(_) => {
                            tracing::warn!(
                                stream_id = %stream_id,
                                "agent: per-session inbound channel full; dropping client frame"
                            );
                        }
                        mpsc::error::TrySendError::Closed(_) => {}
                    }
                }
            }
            None => {
                // No session — silently drop. Could log at debug.
            }
        }
    }
}

fn io_error_response(stream_id: &str, code: &str, detail: &str) -> TunnelResponse {
    TunnelResponse {
        stream_id: stream_id.to_string(),
        payload: Some(tunnel_response::Payload::IoServer(IoServerFrame {
            stream_id: stream_id.to_string(),
            payload: Some(
                open_sandbox_contracts::proxy::io_server_frame::Payload::Error(
                    open_sandbox_contracts::proxy::IoError {
                        code: code.to_string(),
                        detail: detail.to_string(),
                    },
                ),
            ),
        })),
    }
}
