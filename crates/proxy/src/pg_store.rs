use sqlx::PgPool;

use open_sandbox_contracts::error::ProxyError;
use open_sandbox_contracts::types::{AgentId, RoutingEntry, SandboxId};

use crate::routing_store::RoutingStore;

pub struct PgRoutingStore {
    pool: PgPool,
}

impl PgRoutingStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

impl RoutingStore for PgRoutingStore {
    async fn lookup(&self, sandbox_id: &SandboxId) -> Result<Option<AgentId>, ProxyError> {
        todo!()
    }

    async fn load_all(&self) -> Result<Vec<RoutingEntry>, ProxyError> {
        todo!()
    }
}
