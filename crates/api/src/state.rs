//! Shared state across all gateway handlers.
//!
//! Generic over the lifecycle service (so tests can plug in a
//! Mock); the proxy client is a concrete `ProxyClientPool`
//! reachable via `SharedProxyClient = Arc<ProxyClientPool>`.

use std::sync::Arc;

use crate::proxy_client::SharedProxyClient;
use crate::service::SandboxService;

pub struct ApiState<S: SandboxService> {
    pub lifecycle: Arc<S>,
    pub proxy: SharedProxyClient,
    /// Single static API key for v1.0. Future amendments can swap
    /// this for a key-set lookup or a Validator trait.
    pub api_key: String,
}
