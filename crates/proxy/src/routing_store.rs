use open_sandbox_contracts::error::ProxyError;
use open_sandbox_contracts::types::{AgentId, RoutingEntry, SandboxId};

pub trait RoutingStore: Send + Sync {
    fn lookup(
        &self,
        sandbox_id: &SandboxId,
    ) -> impl Future<Output = Result<Option<AgentId>, ProxyError>> + Send;

    /// Resolve a subdomain prefix (first 12 hex chars of the
    /// sandbox UUID) back to the full routing entry. Used by the
    /// HTTP path where the proxy only has the Host header to work
    /// with. `None` means no matching sandbox exists.
    fn lookup_by_subdomain(
        &self,
        subdomain: &str,
    ) -> impl Future<Output = Result<Option<RoutingEntry>, ProxyError>> + Send;

    fn load_all(&self) -> impl Future<Output = Result<Vec<RoutingEntry>, ProxyError>> + Send;
}
