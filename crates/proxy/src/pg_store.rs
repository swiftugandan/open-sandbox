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
        let row: Option<(uuid::Uuid,)> = sqlx::query_as(
            "SELECT agent_id FROM routing_entries WHERE sandbox_id = $1",
        )
        .bind(sandbox_id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| ProxyError::Internal {
            detail: e.to_string(),
        })?;
        Ok(row.map(|(id,)| AgentId(id)))
    }

    async fn load_all(&self) -> Result<Vec<RoutingEntry>, ProxyError> {
        let rows: Vec<(uuid::Uuid, uuid::Uuid)> = sqlx::query_as(
            "SELECT sandbox_id, agent_id FROM routing_entries",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| ProxyError::Internal {
            detail: e.to_string(),
        })?;
        Ok(rows
            .into_iter()
            .map(|(s, a)| RoutingEntry {
                sandbox_id: SandboxId(s),
                agent_id: AgentId(a),
            })
            .collect())
    }
}
