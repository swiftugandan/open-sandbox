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

    /// Proxy-owned schema migrations.
    ///
    /// The `routing_entries` table itself is created by the controller's
    /// migrate(); the proxy only adds the auxiliary index it needs for
    /// fast subdomain lookups on cache miss. Comp-2 A3: without this
    /// index `lookup_by_subdomain` is a sequential scan because
    /// `replace(sandbox_id::text, '-', '')` is a computed expression and
    /// can't use the primary-key index.
    ///
    /// Idempotent — safe to run on every startup. Tolerates being called
    /// before the controller has run its own migrate() by retrying inside
    /// the caller (see crates/cli/src/run.rs proxy startup retry loop).
    pub async fn migrate(&self) -> Result<(), ProxyError> {
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS routing_entries_subdomain_idx
             ON routing_entries (replace(sandbox_id::text, '-', '') text_pattern_ops)",
        )
        .execute(&self.pool)
        .await
        .map_err(|e| ProxyError::Internal {
            detail: format!("create routing_entries_subdomain_idx: {e}"),
        })?;
        Ok(())
    }
}

impl RoutingStore for PgRoutingStore {
    async fn lookup(&self, sandbox_id: &SandboxId) -> Result<Option<AgentId>, ProxyError> {
        let row: Option<(uuid::Uuid,)> =
            sqlx::query_as("SELECT agent_id FROM routing_entries WHERE sandbox_id = $1")
                .bind(sandbox_id.0)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| ProxyError::Internal {
                    detail: e.to_string(),
                })?;
        Ok(row.map(|(id,)| AgentId(id)))
    }

    async fn lookup_by_subdomain(
        &self,
        subdomain: &str,
    ) -> Result<Option<RoutingEntry>, ProxyError> {
        // sandbox UUIDs cast to text use the canonical dashed form
        // (8-4-4-4-12). The subdomain is the FIRST 12 hex characters
        // of the COMPACT form, so it overlaps the first chunk plus
        // the first 4 chars of the next group: positions 0..8 and
        // 9..13 in the dashed form. Strip dashes for the comparison
        // so a single prefix match is enough.
        let pattern = format!("{}%", subdomain);
        let row: Option<(uuid::Uuid, uuid::Uuid)> = sqlx::query_as(
            "SELECT sandbox_id, agent_id
             FROM routing_entries
             WHERE replace(sandbox_id::text, '-', '') LIKE $1
             LIMIT 1",
        )
        .bind(&pattern)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| ProxyError::Internal {
            detail: e.to_string(),
        })?;
        Ok(row.map(|(s, a)| RoutingEntry {
            sandbox_id: SandboxId(s),
            agent_id: AgentId(a),
        }))
    }

    async fn load_all(&self) -> Result<Vec<RoutingEntry>, ProxyError> {
        let rows: Vec<(uuid::Uuid, uuid::Uuid)> =
            sqlx::query_as("SELECT sandbox_id, agent_id FROM routing_entries")
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
