//! Held-open gRPC pool from the API gateway to the proxy.
//!
//! Each pool holds N tonic Channels to the proxy's
//! `SandboxIoService.OpenIoStream`. Channels are HTTP/2-multiplexing
//! by construction; the default pool size (4) gives headroom for
//! ~400 concurrent streams without a fresh connection per session.
//!
//! Calls go out with `authorization: Bearer <internal-token>`
//! metadata so the proxy can verify (alongside the network
//! isolation provided by its separate internal listener).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;

use open_sandbox_contracts::error::ApiError;
use open_sandbox_contracts::proxy::sandbox_io_service_client::SandboxIoServiceClient;
use open_sandbox_contracts::proxy::{IoClientFrame, IoServerFrame};

/// Default number of held-open channels in the pool. Each carries
/// up to ~100 concurrent HTTP/2 streams (the tonic default), so
/// 4 channels comfortably handles 400 concurrent I/O sessions.
pub const DEFAULT_POOL_SIZE: usize = 4;

pub struct ProxyClientPool {
    channels: Vec<Channel>,
    rr: AtomicUsize,
    internal_token: Option<String>,
}

impl ProxyClientPool {
    /// Connect `size` channels to the proxy. `internal_token` is
    /// sent as `Authorization: Bearer <token>` metadata on every
    /// OpenIoStream call; `None` skips the auth header (test mode).
    pub async fn connect(
        proxy_url: &str,
        size: usize,
        internal_token: Option<String>,
    ) -> Result<Self, ApiError> {
        let mut channels = Vec::with_capacity(size.max(1));
        for _ in 0..size.max(1) {
            let channel = Channel::from_shared(proxy_url.to_string())
                .map_err(|e| ApiError::ProxyUnavailable {
                    detail: e.to_string(),
                })?
                .connect()
                .await
                .map_err(|e| ApiError::ProxyUnavailable {
                    detail: e.to_string(),
                })?;
            channels.push(channel);
        }
        Ok(Self {
            channels,
            rr: AtomicUsize::new(0),
            internal_token,
        })
    }

    fn next_channel(&self) -> Channel {
        let i = self.rr.fetch_add(1, Ordering::Relaxed) % self.channels.len();
        self.channels[i].clone()
    }

    /// Open a streaming I/O session. Caller pushes `IoClientFrame`s
    /// into `client_tx` (typically: IoStart first, then Stdin /
    /// Signal / Close); responses arrive on the returned receiver.
    ///
    /// Drop of `client_tx` ends the session; drop of the returned
    /// receiver causes the gateway to abandon listening (the
    /// underlying stream is dropped which the proxy detects).
    pub async fn open_io_stream(
        &self,
        client_rx: mpsc::Receiver<IoClientFrame>,
    ) -> Result<mpsc::Receiver<Result<IoServerFrame, ApiError>>, ApiError> {
        let channel = self.next_channel();
        let mut client = SandboxIoServiceClient::new(channel);

        let outbound = ReceiverStream::new(client_rx);
        let mut request = tonic::Request::new(outbound);
        if let Some(tok) = &self.internal_token {
            let header = format!("Bearer {tok}");
            let parsed: tonic::metadata::MetadataValue<_> =
                header.parse().map_err(|e: tonic::metadata::errors::InvalidMetadataValue| ApiError::Internal {
                    detail: format!("invalid internal-token: {e}"),
                })?;
            request.metadata_mut().insert("authorization", parsed);
        }

        let response = client.open_io_stream(request).await.map_err(|status| {
            match status.code() {
                tonic::Code::NotFound => ApiError::SandboxGone {
                    sandbox_id: status.message().to_string(),
                },
                tonic::Code::Unauthenticated => ApiError::IoStreamFailed {
                    detail: format!("proxy auth: {}", status.message()),
                },
                tonic::Code::Unavailable => ApiError::ProxyUnavailable {
                    detail: status.message().to_string(),
                },
                _ => ApiError::IoStreamFailed {
                    detail: status.message().to_string(),
                },
            }
        })?;

        let mut inbound = response.into_inner();

        // Pump tonic Stream into a friendlier mpsc.
        let (server_tx, server_rx) = mpsc::channel::<Result<IoServerFrame, ApiError>>(32);
        tokio::spawn(async move {
            loop {
                match inbound.message().await {
                    Ok(Some(frame)) => {
                        if server_tx.send(Ok(frame)).await.is_err() {
                            return;
                        }
                    }
                    Ok(None) => return,
                    Err(status) => {
                        let mapped = match status.code() {
                            tonic::Code::NotFound => ApiError::SandboxGone {
                                sandbox_id: status.message().to_string(),
                            },
                            tonic::Code::Unavailable => ApiError::ProxyUnavailable {
                                detail: status.message().to_string(),
                            },
                            _ => ApiError::IoStreamFailed {
                                detail: status.message().to_string(),
                            },
                        };
                        let _ = server_tx.send(Err(mapped)).await;
                        return;
                    }
                }
            }
        });

        Ok(server_rx)
    }
}

/// Convenience: wrap a Pool as an Arc since handlers hold a State.
pub type SharedProxyClient = Arc<ProxyClientPool>;
