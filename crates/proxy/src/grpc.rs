use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
use tracing::{info, warn};

use open_sandbox_contracts::proxy::{
    IoClientFrame, IoClose, IoServerFrame, TunnelRequest, TunnelResponse, io_client_frame,
    sandbox_io_service_server::{SandboxIoService, SandboxIoServiceServer},
    tunnel_request, tunnel_response,
};
use open_sandbox_contracts::types::{AgentId, SandboxId};

use crate::io_sessions::{IoSessionRecord, IoSessions};
use crate::routing_cache::RoutingCache;
use crate::routing_store::RoutingStore;
use crate::stream_mux::StreamMux;
use crate::tunnel_pool::TunnelPool;

const INTERNAL_AUTH_METADATA_KEY: &str = "authorization";

/// Which roles a `SandboxIoService` listener exposes. The proxy
/// runs two listeners — agents reach Public on a port exposed
/// publicly; the api gateway reaches Internal on a port that
/// (in production) is reachable only from the gateway's network
/// segment. Each role serves exactly one of the two RPCs; calls
/// to the wrong RPC are rejected with `Unimplemented` at the
/// role gate, *before* the bearer-token check.
///
/// `Combined` keeps a single-listener mode for unit tests and
/// for the rare developer setup where network isolation isn't
/// available; production deployments should use the two-listener
/// split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyRole {
    Public,
    Internal,
    Combined,
}

impl ProxyRole {
    fn accepts_open_tunnel(self) -> bool {
        matches!(self, ProxyRole::Public | ProxyRole::Combined)
    }
    fn accepts_open_io_stream(self) -> bool {
        matches!(self, ProxyRole::Internal | ProxyRole::Combined)
    }
}

/// Shared dependencies for the proxy's gRPC handlers.
pub struct SandboxIoHandler<S: RoutingStore> {
    mux: Arc<StreamMux>,
    pool: Arc<TunnelPool>,
    sessions: Arc<IoSessions>,
    routing: Arc<RoutingCache<S>>,
    /// Optional shared-secret token. When set, requests carrying
    /// `OpenIoStream` (gateway-originated) must include
    /// `authorization: Bearer <token>` in metadata. None disables
    /// the check (used in single-process tests where network
    /// isolation suffices).
    internal_token: Option<String>,
    /// Optional shared-secret token for the agent-side OpenTunnel RPC.
    /// Comp-2 A1: required at the binary boundary in production so a
    /// network-reachable attacker cannot register as an arbitrary
    /// agent_id and hijack routing. None disables the check (single-
    /// process tests).
    tunnel_token: Option<String>,
    /// Which RPCs this handler instance accepts. See [`ProxyRole`].
    role: ProxyRole,
}

impl<S: RoutingStore> SandboxIoHandler<S> {
    pub fn new(
        mux: Arc<StreamMux>,
        pool: Arc<TunnelPool>,
        sessions: Arc<IoSessions>,
        routing: Arc<RoutingCache<S>>,
        internal_token: Option<String>,
    ) -> Self {
        Self::with_role(
            mux,
            pool,
            sessions,
            routing,
            internal_token,
            None,
            ProxyRole::Combined,
        )
    }

    pub fn with_role(
        mux: Arc<StreamMux>,
        pool: Arc<TunnelPool>,
        sessions: Arc<IoSessions>,
        routing: Arc<RoutingCache<S>>,
        internal_token: Option<String>,
        tunnel_token: Option<String>,
        role: ProxyRole,
    ) -> Self {
        Self {
            mux,
            pool,
            sessions,
            routing,
            internal_token,
            tunnel_token,
            role,
        }
    }
}

#[tonic::async_trait]
impl<S: RoutingStore + 'static> SandboxIoService for SandboxIoHandler<S> {
    type OpenTunnelStream = ReceiverStream<Result<TunnelRequest, Status>>;

    async fn open_tunnel(
        &self,
        request: Request<Streaming<TunnelResponse>>,
    ) -> Result<Response<Self::OpenTunnelStream>, Status> {
        if !self.role.accepts_open_tunnel() {
            return Err(Status::unimplemented(
                "OpenTunnel is not served on this proxy listener; \
                 agents must dial the public listener",
            ));
        }
        // Comp-2 A1: require a shared-secret tunnel-join token when set.
        // Without this, a network-reachable attacker can call OpenTunnel
        // claiming any agent_id (or even spam reconnects) and hijack
        // routing for that agent's sandboxes.
        if let Some(expected) = &self.tunnel_token {
            let got = request
                .metadata()
                .get(INTERNAL_AUTH_METADATA_KEY)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.strip_prefix("Bearer "));
            if got != Some(expected.as_str()) {
                return Err(Status::unauthenticated(
                    "missing or invalid tunnel-join token",
                ));
            }
        }
        let mut inbound = request.into_inner();
        let (result_tx, outbound_rx) = mpsc::channel::<Result<TunnelRequest, Status>>(32);
        let (request_tx, mut request_rx) = mpsc::channel::<TunnelRequest>(32);
        tokio::spawn(async move {
            while let Some(req) = request_rx.recv().await {
                if result_tx.send(Ok(req)).await.is_err() {
                    break;
                }
            }
        });

        let pool = self.pool.clone();
        let mux = self.mux.clone();
        let sessions = self.sessions.clone();
        let routing = self.routing.clone();

        tokio::spawn(async move {
            let mut registered_agent_id: Option<AgentId> = None;
            // Comp-2 B1: remember our generation so cleanup only affects the
            // tunnel we registered (not any later reconnect that took over).
            let mut my_generation: Option<crate::tunnel_pool::TunnelGeneration> = None;

            while let Ok(Some(msg)) = inbound.message().await {
                let Some(payload) = msg.payload else {
                    continue;
                };

                match payload {
                    tunnel_response::Payload::Ready(ready) => {
                        let Ok(agent_uuid) = uuid::Uuid::parse_str(&ready.agent_id) else {
                            break;
                        };
                        let agent_id = AgentId::from(agent_uuid);
                        let generation = pool.register(agent_id.clone(), request_tx.clone());
                        registered_agent_id = Some(agent_id);
                        my_generation = Some(generation);
                    }
                    // Comp-2 A2: dispatch frames carry stream_id only, but a
                    // malicious/confused agent could deliver frames for streams
                    // owned by other agents. Verify carrier ownership in
                    // deliver_response / deliver_server_frame / fail_stream by
                    // passing the registered_agent_id. Frames before Ready
                    // (registered_agent_id == None) are dropped.
                    tunnel_response::Payload::HttpResponse(resp) => {
                        let Some(ref agent_id) = registered_agent_id else {
                            warn!("dropping HttpResponse received before Ready");
                            continue;
                        };
                        mux.deliver_response(&msg.stream_id, agent_id, resp);
                    }
                    tunnel_response::Payload::Close(close) => {
                        let Some(ref agent_id) = registered_agent_id else {
                            warn!("dropping Close received before Ready");
                            continue;
                        };
                        mux.fail_stream(&msg.stream_id, agent_id, close.reason);
                    }
                    tunnel_response::Payload::Data(_) => {}
                    tunnel_response::Payload::IoServer(frame) => {
                        let Some(ref agent_id) = registered_agent_id else {
                            warn!("dropping IoServer frame received before Ready");
                            continue;
                        };
                        // Comp-2 B2: deliver_server_frame is now sync + uses
                        // try_send under the hood. A slow per-session
                        // consumer no longer blocks this tunnel-inbound loop
                        // (which would HoL-block every other session on this
                        // agent's tunnel). Drops on a full per-session
                        // channel are logged inside deliver_server_frame.
                        let _ = sessions.deliver_server_frame(
                            &msg.stream_id, agent_id, frame,
                        );
                    }
                }
            }

            // Comp-2 B1: scope all cleanup to our specific generation so a
            // newer tunnel for the same AgentId (an agent reconnect that
            // overwrote our pool entry) is left intact.
            if let (Some(agent_id), Some(generation)) = (registered_agent_id, my_generation) {
                mux.cancel_agent_streams_at_generation(&agent_id, generation);
                sessions.cancel_agent_streams_at_generation(&agent_id, generation);
                // Comp-2 A6/C6: only evict cache entries if our tunnel was
                // still the current one. If the agent reconnected on a newer
                // generation, the new tunnel's cache entries must be
                // preserved.
                let was_current = pool.remove_if_current(&agent_id, generation);
                if was_current {
                    routing.remove_for_agent(&agent_id);
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(outbound_rx)))
    }

    type OpenIoStreamStream = ReceiverStream<Result<IoServerFrame, Status>>;

    async fn open_io_stream(
        &self,
        request: Request<Streaming<IoClientFrame>>,
    ) -> Result<Response<Self::OpenIoStreamStream>, Status> {
        if !self.role.accepts_open_io_stream() {
            return Err(Status::unimplemented(
                "OpenIoStream is not served on this proxy listener; \
                 the api gateway must dial the internal listener",
            ));
        }
        // Internal authn: gateway sends `authorization: Bearer
        // <token>`. Network isolation (separate listener) is the
        // primary defense; the token is defense in depth and
        // supports cross-host topologies.
        if let Some(expected) = &self.internal_token {
            let got = request
                .metadata()
                .get(INTERNAL_AUTH_METADATA_KEY)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.strip_prefix("Bearer "));
            if got != Some(expected.as_str()) {
                return Err(Status::unauthenticated(
                    "missing or invalid internal authorization",
                ));
            }
        }

        let inbound = request.into_inner();
        // Comp-2 B2: per-session buffer raised to IO_SESSION_BUFFER so a
        // momentarily slow gateway consumer doesn't immediately trigger the
        // try_send drop policy in io_sessions::deliver_server_frame. The
        // tradeoff is more memory per stalled session, bounded at
        // IO_SESSION_BUFFER * sizeof(IoServerFrame).
        const IO_SESSION_BUFFER: usize = 256;
        let (server_tx, server_rx) =
            mpsc::channel::<Result<IoServerFrame, Status>>(IO_SESSION_BUFFER);

        // tonic bidi-streaming requires that we return the Response
        // BEFORE the client can flush messages on the request body.
        // So we must NOT await inbound.message() inline here — we
        // spawn a task that does the IoStart handshake + frame
        // pumping, and return the server_rx immediately.
        let pool = self.pool.clone();
        let sessions = self.sessions.clone();
        let routing = self.routing.clone();

        tokio::spawn(dispatch_io_stream(
            pool, sessions, routing, inbound, server_tx,
        ));

        Ok(Response::new(ReceiverStream::new(server_rx)))
    }
}

async fn dispatch_io_stream<S: RoutingStore + 'static>(
    pool: Arc<TunnelPool>,
    sessions: Arc<IoSessions>,
    routing: Arc<RoutingCache<S>>,
    mut inbound: Streaming<IoClientFrame>,
    server_tx: mpsc::Sender<Result<IoServerFrame, Status>>,
) {
    // Read first IoStart frame.
    let first = match inbound.message().await {
        Ok(Some(f)) => f,
        Ok(None) => {
            let _ = server_tx
                .send(Err(Status::invalid_argument(
                    "OpenIoStream closed before first frame",
                )))
                .await;
            return;
        }
        Err(e) => {
            let _ = server_tx.send(Err(Status::internal(e.to_string()))).await;
            return;
        }
    };

    let start = match first.payload {
        Some(io_client_frame::Payload::Start(s)) => s,
        _ => {
            let _ = server_tx
                .send(Err(Status::invalid_argument("first frame must be IoStart")))
                .await;
            return;
        }
    };

    let sandbox_uuid = match uuid::Uuid::parse_str(&start.sandbox_id) {
        Ok(u) => u,
        Err(_) => {
            let _ = server_tx
                .send(Err(Status::invalid_argument("invalid sandbox_id")))
                .await;
            return;
        }
    };
    let sandbox_id = SandboxId::from(sandbox_uuid);

    let route = match routing.lookup_or_fetch(&sandbox_id.subdomain()).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            let _ = server_tx
                .send(Err(Status::not_found(format!(
                    "sandbox {sandbox_id} not in routing table"
                ))))
                .await;
            return;
        }
        Err(e) => {
            let _ = server_tx
                .send(Err(Status::internal(format!("routing lookup failed: {e}"))))
                .await;
            return;
        }
    };
    let agent_id = route.agent_id;

    let (agent_sender, generation) = match pool.get_sender_with_generation(&agent_id) {
        Some(s) => s,
        None => {
            let _ = server_tx
                .send(Err(Status::unavailable(format!(
                    "agent {agent_id} not connected"
                ))))
                .await;
            return;
        }
    };

    let stream_id = sessions.next_stream_id();
    sessions.insert(
        stream_id.clone(),
        IoSessionRecord {
            agent_id: agent_id.clone(),
            server_tx: server_tx.clone(),
            // Comp-2 B1: stamp the session with the tunnel generation so an
            // old-tunnel cleanup can't kill it after the agent reconnects.
            generation,
        },
    );

    info!(
        stream_id = %stream_id,
        sandbox_id = %sandbox_id,
        agent_id = %agent_id,
        "io_session.start"
    );

    // Forward the original IoStart frame to the agent first.
    let start_frame = TunnelRequest {
        stream_id: stream_id.clone(),
        payload: Some(tunnel_request::Payload::IoClient(IoClientFrame {
            stream_id: stream_id.clone(),
            payload: Some(io_client_frame::Payload::Start(start)),
        })),
    };
    if agent_sender.send(start_frame).await.is_err() {
        sessions.remove(&stream_id);
        let _ = server_tx
            .send(Err(Status::unavailable("agent tunnel dropped")))
            .await;
        return;
    }

    // Spawn the inbound→agent pump. Decoupled from the lifetime of
    // this function so we can keep the response channel alive
    // while the agent finishes work — typical for unary REST
    // callers (write_file, write_files, read_file) that close
    // their request stream immediately after sending all frames.
    let pool_for_pump = pool.clone();
    let agent_id_for_pump = agent_id.clone();
    let stream_id_for_pump = stream_id.clone();
    tokio::spawn(async move {
        let mut saw_explicit_close = false;
        loop {
            let frame = match inbound.message().await {
                Ok(Some(f)) => f,
                _ => break,
            };
            // Only a *full* close (stdin_eof=false) counts as the
            // client's "session over" signal. A half-close
            // (stdin_eof=true) is the SDK's stdin-EOF affordance —
            // the process keeps running and may still emit output.
            // Without this distinction, any client that closes its
            // local stdin (e.g. piped or backgrounded ssh-style use)
            // would defeat the synthetic-Close cleanup on later
            // disconnect.
            if let Some(io_client_frame::Payload::Close(c)) = &frame.payload
                && !c.stdin_eof
            {
                saw_explicit_close = true;
            }
            let wrapped = TunnelRequest {
                stream_id: stream_id_for_pump.clone(),
                payload: Some(tunnel_request::Payload::IoClient(IoClientFrame {
                    stream_id: stream_id_for_pump.clone(),
                    payload: frame.payload,
                })),
            };
            let Some(sender) = pool_for_pump.get_sender(&agent_id_for_pump) else {
                warn!(
                    stream_id = %stream_id_for_pump,
                    agent_id = %agent_id_for_pump,
                    "agent tunnel dropped mid-session"
                );
                return;
            };
            if sender.send(wrapped).await.is_err() {
                return;
            }
        }
        // Gateway closed its request stream (or the stream was
        // canceled). If the caller never sent an explicit IoClose
        // (e.g. a WebSocket peer that simply dropped the socket) the
        // agent has no way to know
        // the session is over — it would keep the in-container
        // process running until natural exit. Forward a synthetic
        // Close so the agent's pump_exec_session fires its
        // ExecRegistry cleanup.
        if !saw_explicit_close {
            let synthetic_close = TunnelRequest {
                stream_id: stream_id_for_pump.clone(),
                payload: Some(tunnel_request::Payload::IoClient(IoClientFrame {
                    stream_id: stream_id_for_pump.clone(),
                    payload: Some(io_client_frame::Payload::Close(IoClose {
                        stdin_eof: false,
                    })),
                })),
            };
            if let Some(sender) = pool_for_pump.get_sender(&agent_id_for_pump) {
                let _ = sender.send(synthetic_close).await;
            }
        }
    });

    // Hold the session record alive until the gateway-side
    // response receiver is dropped. This fires when:
    //  - unary_via_io_stream sees an Exited/Error terminal frame
    //    and returns (dropping its mpsc::Receiver), OR
    //  - the public WebSocket caller hung up (gateway closes
    //    its proxy stream).
    // Without this wait, dispatch_io_stream would return as soon
    // as the gateway's request stream closes, dropping the
    // server_tx and causing the gateway to see "proxy stream
    // ended without terminal frame".
    server_tx.closed().await;
    sessions.remove(&stream_id);
    info!(
        stream_id = %stream_id,
        "io_session.client_closed"
    );
}

pub fn sandbox_io_service<S: RoutingStore + 'static>(
    mux: Arc<StreamMux>,
    pool: Arc<TunnelPool>,
    sessions: Arc<IoSessions>,
    routing: Arc<RoutingCache<S>>,
    internal_token: Option<String>,
) -> SandboxIoServiceServer<SandboxIoHandler<S>> {
    sandbox_io_service_with_role(
        mux,
        pool,
        sessions,
        routing,
        internal_token,
        None,
        ProxyRole::Combined,
    )
}

pub fn sandbox_io_service_with_role<S: RoutingStore + 'static>(
    mux: Arc<StreamMux>,
    pool: Arc<TunnelPool>,
    sessions: Arc<IoSessions>,
    routing: Arc<RoutingCache<S>>,
    internal_token: Option<String>,
    tunnel_token: Option<String>,
    role: ProxyRole,
) -> SandboxIoServiceServer<SandboxIoHandler<S>> {
    SandboxIoServiceServer::new(SandboxIoHandler::with_role(
        mux,
        pool,
        sessions,
        routing,
        internal_token,
        tunnel_token,
        role,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::InMemoryRoutingStore;
    use open_sandbox_contracts::proxy::{
        ExecParams, IoStart, TunnelReady, io_start,
        sandbox_io_service_client::SandboxIoServiceClient,
    };
    use std::collections::HashMap;
    use tokio_stream::wrappers::TcpListenerStream;

    async fn start_proxy_grpc(
        mux: Arc<StreamMux>,
        pool: Arc<TunnelPool>,
        sessions: Arc<IoSessions>,
        routing: Arc<RoutingCache<InMemoryRoutingStore>>,
        internal_token: Option<String>,
    ) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = format!("http://{}", listener.local_addr().unwrap());

        let service = sandbox_io_service(mux, pool, sessions, routing, internal_token);
        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(service)
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
                .unwrap();
        });

        addr
    }

    fn empty_cache() -> Arc<RoutingCache<InMemoryRoutingStore>> {
        Arc::new(RoutingCache::new(InMemoryRoutingStore::default()))
    }

    fn iostart_frame(sandbox_id: &SandboxId) -> IoClientFrame {
        IoClientFrame {
            stream_id: String::new(),
            payload: Some(io_client_frame::Payload::Start(IoStart {
                sandbox_id: sandbox_id.to_string(),
                params: Some(io_start::Params::Exec(ExecParams {
                    command: vec!["echo".into(), "hi".into()],
                    cwd: String::new(),
                    env: HashMap::new(),
                })),
            })),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn open_io_stream_on_public_listener_returns_unimplemented() {
        // The public listener (Role::Public) exposes only
        // OpenTunnel — agent ingress. Any caller that reaches it
        // and tries to dispatch an OpenIoStream call must be
        // rejected at the role gate, before the bearer-token
        // check. This is the network-isolation primary defense
        // that the v1.0 amendment dropped; v1.0.1 restores it.
        let pool = Arc::new(TunnelPool::new());
        let mux = Arc::new(StreamMux::new(pool.clone()));
        let sessions = Arc::new(IoSessions::new());
        let routing = empty_cache();
        let addr =
            start_proxy_grpc_with_role(mux, pool, sessions, routing, None, ProxyRole::Public).await;
        let channel = tonic::transport::Channel::from_shared(addr)
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut client = SandboxIoServiceClient::new(channel);
        let (tx, rx) = mpsc::channel::<IoClientFrame>(1);
        tx.send(iostart_frame(&SandboxId::new())).await.unwrap();
        drop(tx);
        let resp = client.open_io_stream(ReceiverStream::new(rx)).await;
        match resp {
            Err(status) => {
                // Expect UNIMPLEMENTED or UNAUTHENTICATED. Any
                // other code means the request reached real
                // dispatch logic, which is the bug this test
                // guards against.
                assert!(
                    matches!(
                        status.code(),
                        tonic::Code::Unimplemented | tonic::Code::Unauthenticated
                    ),
                    "expected Unimplemented/Unauthenticated on public listener, got {:?}",
                    status.code()
                );
            }
            Ok(mut response) => {
                // Some servers may return a streaming response
                // immediately and then fail on the first frame.
                let inbound = response.get_mut();
                match inbound.message().await {
                    Err(status) => {
                        assert!(
                            matches!(
                                status.code(),
                                tonic::Code::Unimplemented | tonic::Code::Unauthenticated
                            ),
                            "expected Unimplemented/Unauthenticated on first frame from public listener, got {:?}",
                            status.code()
                        );
                    }
                    Ok(None) => {
                        panic!("public listener returned a clean empty stream; expected rejection")
                    }
                    Ok(Some(_)) => {
                        panic!("public listener processed an IoStart frame; expected rejection")
                    }
                }
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn open_tunnel_on_internal_listener_returns_unimplemented() {
        // Symmetric guard: the internal listener (Role::Internal)
        // hosts only OpenIoStream — gateway egress. Agents that
        // mis-target it must be rejected at the role gate.
        let pool = Arc::new(TunnelPool::new());
        let mux = Arc::new(StreamMux::new(pool.clone()));
        let sessions = Arc::new(IoSessions::new());
        let routing = empty_cache();
        let addr =
            start_proxy_grpc_with_role(mux, pool, sessions, routing, None, ProxyRole::Internal)
                .await;
        let channel = tonic::transport::Channel::from_shared(addr)
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut client = SandboxIoServiceClient::new(channel);
        let (_outbound_tx, outbound_rx) = mpsc::channel::<TunnelResponse>(1);
        let outbound = ReceiverStream::new(outbound_rx);
        let resp = client.open_tunnel(outbound).await;
        match resp {
            Err(status) => {
                assert!(
                    matches!(
                        status.code(),
                        tonic::Code::Unimplemented | tonic::Code::Unauthenticated
                    ),
                    "expected Unimplemented/Unauthenticated on internal listener, got {:?}",
                    status.code()
                );
            }
            Ok(_) => panic!("internal listener accepted an OpenTunnel call; expected rejection"),
        }
    }

    async fn start_proxy_grpc_with_role(
        mux: Arc<StreamMux>,
        pool: Arc<TunnelPool>,
        sessions: Arc<IoSessions>,
        routing: Arc<RoutingCache<InMemoryRoutingStore>>,
        internal_token: Option<String>,
        role: ProxyRole,
    ) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = format!("http://{}", listener.local_addr().unwrap());
        let service = sandbox_io_service_with_role(
            mux,
            pool,
            sessions,
            routing,
            internal_token,
            None, // tunnel_token disabled in unit tests
            role,
        );
        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(service)
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
                .unwrap();
        });
        addr
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_connects_and_registers_tunnel() {
        let pool = Arc::new(TunnelPool::new());
        let mux = Arc::new(StreamMux::new(pool.clone()));
        let sessions = Arc::new(IoSessions::new());
        let routing = empty_cache();
        let addr = start_proxy_grpc(mux, pool.clone(), sessions, routing, None).await;

        let channel = tonic::transport::Channel::from_shared(addr)
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut client = SandboxIoServiceClient::new(channel);

        let (outbound_tx, outbound_rx) = mpsc::channel(32);
        let outbound = ReceiverStream::new(outbound_rx);
        let _response = client.open_tunnel(outbound).await.unwrap();

        let agent_id = AgentId::new();
        let ready = TunnelResponse {
            stream_id: String::new(),
            payload: Some(tunnel_response::Payload::Ready(TunnelReady {
                agent_id: agent_id.to_string(),
            })),
        };
        outbound_tx.send(ready).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(pool.contains(&agent_id));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn open_io_stream_unknown_sandbox_returns_not_found() {
        let pool = Arc::new(TunnelPool::new());
        let mux = Arc::new(StreamMux::new(pool.clone()));
        let sessions = Arc::new(IoSessions::new());
        let routing = empty_cache();
        let addr = start_proxy_grpc(mux, pool, sessions, routing, None).await;

        let channel = tonic::transport::Channel::from_shared(addr)
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut client = SandboxIoServiceClient::new(channel);

        let sandbox_id = SandboxId::new();
        let (client_tx, client_rx) = mpsc::channel::<IoClientFrame>(8);
        client_tx.send(iostart_frame(&sandbox_id)).await.unwrap();
        let outbound = ReceiverStream::new(client_rx);

        let result = client.open_io_stream(outbound).await;
        match result {
            Err(status) => assert_eq!(status.code(), tonic::Code::NotFound),
            Ok(mut resp) => {
                // Some grpc implementations don't surface NotFound
                // until the first message — try to recv.
                let first = resp.get_mut().message().await;
                match first {
                    Err(e) => assert_eq!(e.code(), tonic::Code::NotFound),
                    Ok(_) => panic!("expected NotFound"),
                }
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn open_io_stream_with_invalid_token_returns_unauthenticated() {
        let pool = Arc::new(TunnelPool::new());
        let mux = Arc::new(StreamMux::new(pool.clone()));
        let sessions = Arc::new(IoSessions::new());
        let routing = empty_cache();
        let addr = start_proxy_grpc(
            mux,
            pool,
            sessions,
            routing,
            Some("expected-secret".to_string()),
        )
        .await;

        let channel = tonic::transport::Channel::from_shared(addr)
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut client = SandboxIoServiceClient::new(channel);

        let sandbox_id = SandboxId::new();
        let (client_tx, client_rx) = mpsc::channel::<IoClientFrame>(8);
        client_tx.send(iostart_frame(&sandbox_id)).await.unwrap();
        let mut req = tonic::Request::new(ReceiverStream::new(client_rx));
        req.metadata_mut().insert(
            INTERNAL_AUTH_METADATA_KEY,
            "Bearer wrong-secret".parse().unwrap(),
        );

        let result = client.open_io_stream(req).await;
        match result {
            Err(status) => assert_eq!(status.code(), tonic::Code::Unauthenticated),
            Ok(mut resp) => {
                let first = resp.get_mut().message().await;
                assert!(first.is_err());
                assert_eq!(first.unwrap_err().code(), tonic::Code::Unauthenticated);
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn open_io_stream_round_trip_via_agent_tunnel() {
        // Wire: gateway client -> proxy -> agent tunnel -> echo back
        let pool = Arc::new(TunnelPool::new());
        let mux = Arc::new(StreamMux::new(pool.clone()));
        let sessions = Arc::new(IoSessions::new());
        let routing = empty_cache();

        // Pre-populate routing for a sandbox.
        let sandbox_id = SandboxId::new();
        let agent_id = AgentId::new();
        routing.insert(sandbox_id.clone(), agent_id.clone());

        let addr = start_proxy_grpc(mux, pool.clone(), sessions.clone(), routing, None).await;

        // Connect "agent" side.
        let agent_channel = tonic::transport::Channel::from_shared(addr.clone())
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut agent_client = SandboxIoServiceClient::new(agent_channel);
        let (agent_tx, agent_rx) = mpsc::channel::<TunnelResponse>(32);
        let agent_stream = ReceiverStream::new(agent_rx);
        let mut agent_inbound = agent_client
            .open_tunnel(agent_stream)
            .await
            .unwrap()
            .into_inner();
        // Send Ready frame.
        agent_tx
            .send(TunnelResponse {
                stream_id: String::new(),
                payload: Some(tunnel_response::Payload::Ready(TunnelReady {
                    agent_id: agent_id.to_string(),
                })),
            })
            .await
            .unwrap();
        // Give the pool a beat to register.
        for _ in 0..20 {
            if pool.contains(&agent_id) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(pool.contains(&agent_id));

        // Now connect "gateway" side and open io_stream.
        let gw_channel = tonic::transport::Channel::from_shared(addr)
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut gw_client = SandboxIoServiceClient::new(gw_channel);
        let (gw_tx, gw_rx) = mpsc::channel::<IoClientFrame>(32);
        gw_tx.send(iostart_frame(&sandbox_id)).await.unwrap();
        let mut gw_inbound = gw_client
            .open_io_stream(ReceiverStream::new(gw_rx))
            .await
            .unwrap()
            .into_inner();

        // The agent should now see an IoClient(Start) on its
        // tunnel inbound. Echo back an IoServer frame.
        let agent_frame = agent_inbound.message().await.unwrap().unwrap();
        match &agent_frame.payload {
            Some(tunnel_request::Payload::IoClient(IoClientFrame {
                payload: Some(io_client_frame::Payload::Start(_)),
                ..
            })) => {}
            other => panic!("expected IoClient(Start), got {other:?}"),
        }
        let stream_id_on_tunnel = agent_frame.stream_id.clone();

        // Agent sends back an IoServer Exited.
        agent_tx
            .send(TunnelResponse {
                stream_id: stream_id_on_tunnel.clone(),
                payload: Some(tunnel_response::Payload::IoServer(IoServerFrame {
                    stream_id: stream_id_on_tunnel,
                    payload: Some(
                        open_sandbox_contracts::proxy::io_server_frame::Payload::Exited(
                            open_sandbox_contracts::proxy::IoExited {
                                exit_code: 0,
                                command_not_found: false,
                            },
                        ),
                    ),
                })),
            })
            .await
            .unwrap();

        // Gateway should receive it.
        let received = gw_inbound.message().await.unwrap().unwrap();
        match received.payload {
            Some(open_sandbox_contracts::proxy::io_server_frame::Payload::Exited(e)) => {
                assert_eq!(e.exit_code, 0);
            }
            other => panic!("expected Exited, got {other:?}"),
        }
    }
}
