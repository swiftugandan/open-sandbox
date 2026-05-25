//! In-binary ACME (Let's Encrypt) wrapper for the proxy's public gRPC
//! listener. Comp-2 C5 / comp-9 #1.
//!
//! Operator opts in by setting `TUNNEL_ACME_DOMAIN` + `ACME_EMAIL`. The
//! proxy's public listener then terminates TLS using a Let's Encrypt cert
//! it issues itself via TLS-ALPN-01. Without those env vars the proxy
//! falls back to plaintext h2c (development behavior).
//!
//! The cert cache lives at `ACME_CACHE_DIR` (default `./acme-cache`).
//! Setting `ACME_STAGING=1` switches to LE staging — useful for first
//! deploys to avoid burning production rate limits.

use std::io;
use std::path::PathBuf;

use rustls_acme::AcmeConfig;
use rustls_acme::caches::DirCache;
use rustls_acme::is_tls_alpn_challenge;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_rustls::LazyConfigAcceptor;
use tokio_rustls::server::TlsStream;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;
use tracing::{info, warn};

pub struct AcmeSettings {
    pub domain: String,
    pub email: String,
    pub cache_dir: PathBuf,
    pub staging: bool,
}

impl AcmeSettings {
    /// Read ACME settings from environment. Returns `Some` when both
    /// `TUNNEL_ACME_DOMAIN` and `ACME_EMAIL` are set; `None` otherwise.
    pub fn from_env() -> Option<Self> {
        let domain = std::env::var("TUNNEL_ACME_DOMAIN").ok()?;
        let email = std::env::var("ACME_EMAIL").ok()?;
        let cache_dir = std::env::var("ACME_CACHE_DIR")
            .ok()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("./acme-cache"));
        let staging = matches!(
            std::env::var("ACME_STAGING").ok().as_deref(),
            Some("1") | Some("true") | Some("yes")
        );
        Some(Self {
            domain,
            email,
            cache_dir,
            staging,
        })
    }
}

/// Build an incoming-connection stream for tonic that terminates TLS via
/// rustls-acme. Drives the ACME state machine + accept loop in background
/// tasks so the cert rotates without the operator restarting the proxy.
///
/// Pattern from rustls-acme's `examples/low_level_tokio.rs`: use
/// tokio-rustls' `LazyConfigAcceptor` to sniff the ClientHello, route
/// TLS-ALPN-01 challenges to the challenge config (no application stream
/// emitted), and route real connections to the default cert config.
pub fn acme_incoming(
    listener: tokio::net::TcpListener,
    settings: AcmeSettings,
) -> ReceiverStream<Result<TlsStream<TcpStream>, io::Error>> {
    info!(
        domain = %settings.domain,
        email = %settings.email,
        cache_dir = ?settings.cache_dir,
        staging = settings.staging,
        "ACME configured"
    );

    let mut state = AcmeConfig::new([settings.domain.as_str()])
        .contact([format!("mailto:{}", settings.email)])
        .cache(DirCache::new(settings.cache_dir))
        .directory_lets_encrypt(!settings.staging)
        .state();
    let challenge_config = state.challenge_rustls_config();
    let default_config = state.default_rustls_config();

    // Background ACME driver — issuance, renewal, and challenge response.
    tokio::spawn(async move {
        loop {
            match state.next().await {
                Some(Ok(ok)) => info!(event = ?ok, "ACME state"),
                Some(Err(err)) => warn!(error = %err, "ACME error"),
                None => {
                    warn!("ACME state stream ended; renewal halted");
                    break;
                }
            }
        }
    });

    let (tx, rx) = mpsc::channel::<Result<TlsStream<TcpStream>, io::Error>>(32);

    // Accept loop: each new TCP connection is handshaked in its own task
    // (so one slow client doesn't head-of-line the listener) and either
    // routed to tonic via tx, or absorbed as an ALPN challenge response.
    tokio::spawn(async move {
        loop {
            let (tcp, peer) = match listener.accept().await {
                Ok(ok) => ok,
                Err(e) => {
                    warn!(error = %e, "TCP accept failed");
                    continue;
                }
            };
            let challenge_config = challenge_config.clone();
            let default_config = default_config.clone();
            let tx = tx.clone();
            tokio::spawn(async move {
                let start = match LazyConfigAcceptor::new(Default::default(), tcp).await {
                    Ok(s) => s,
                    Err(e) => {
                        warn!(peer = %peer, error = %e, "TLS handshake start failed");
                        return;
                    }
                };
                if is_tls_alpn_challenge(&start.client_hello()) {
                    info!(peer = %peer, "TLS-ALPN-01 challenge received");
                    if let Ok(mut tls) = start.into_stream(challenge_config).await {
                        let _ = tls.shutdown().await;
                    }
                } else {
                    match start.into_stream(default_config).await {
                        Ok(tls) => {
                            if tx.send(Ok(tls)).await.is_err() {
                                return; // listener consumer dropped
                            }
                        }
                        Err(e) => {
                            warn!(peer = %peer, error = %e, "TLS handshake failed");
                        }
                    }
                }
            });
        }
    });

    ReceiverStream::new(rx)
}
