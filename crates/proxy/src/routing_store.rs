use open_sandbox_contracts::error::ProxyError;
use open_sandbox_contracts::types::{AgentId, RoutingEntry, SandboxId};

pub trait RoutingStore: Send + Sync {
    fn lookup(
        &self,
        sandbox_id: &SandboxId,
    ) -> impl Future<Output = Result<Option<AgentId>, ProxyError>> + Send;

    fn load_all(&self) -> impl Future<Output = Result<Vec<RoutingEntry>, ProxyError>> + Send;
}
