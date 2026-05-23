use sqlx::PgPool;
use sqlx::postgres::PgListener;

use open_sandbox_contracts::error::ProxyError;
use open_sandbox_contracts::types::{AgentId, RoutingEntry, SandboxId};

use crate::routing_store::RoutingStore;

/// LISTEN channel name the controller emits routing-table change notices on.
/// Mirrors `crates/controller/src/pg_store.rs::ROUTING_CHANGED_CHANNEL`.
/// Kept here as a separate constant so the proxy compiles without depending
/// on the controller crate. Comp-2 A6/B3/C1.
pub const ROUTING_CHANGED_CHANNEL: &str = "routing_changed";

#[derive(Debug, Clone)]
pub enum RoutingChange {
    Insert {
        sandbox_id: SandboxId,
        agent_id: AgentId,
    },
    Remove {
        sandbox_id: SandboxId,
    },
}

impl RoutingChange {
    /// Parse a payload string like `{"op":"insert","sandbox_id":"<uuid>","agent_id":"<uuid>"}`.
    /// Returns `None` if the payload doesn't match the canonical shape — old
    /// senders or hand-crafted notifies are silently ignored.
    pub fn parse(payload: &str) -> Option<Self> {
        let op = extract_json_field(payload, "op")?;
        let sandbox_id_str = extract_json_field(payload, "sandbox_id")?;
        let sandbox_uuid = uuid::Uuid::parse_str(&sandbox_id_str).ok()?;
        let sandbox_id = SandboxId(sandbox_uuid);
        match op.as_str() {
            "insert" => {
                let agent_id_str = extract_json_field(payload, "agent_id")?;
                let agent_uuid = uuid::Uuid::parse_str(&agent_id_str).ok()?;
                Some(RoutingChange::Insert {
                    sandbox_id,
                    agent_id: AgentId(agent_uuid),
                })
            }
            "remove" => Some(RoutingChange::Remove { sandbox_id }),
            _ => None,
        }
    }
}

/// Trivial JSON-ish extractor — sufficient for the controller's
/// fixed-shape payload. Avoids pulling in a full JSON parser for one match.
fn extract_json_field(payload: &str, field: &str) -> Option<String> {
    let needle = format!("\"{field}\":\"");
    let start = payload.find(&needle)? + needle.len();
    let rest = &payload[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

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
    /// Subscribe to `routing_changed`. Returns a sqlx PgListener that
    /// callers can `recv()` notifications from. Comp-2 A6/B3/C1.
    pub async fn routing_changed_listener(&self) -> Result<PgListener, ProxyError> {
        let mut listener = PgListener::connect_with(&self.pool)
            .await
            .map_err(|e| ProxyError::Internal {
                detail: format!("PgListener connect: {e}"),
            })?;
        listener
            .listen(ROUTING_CHANGED_CHANNEL)
            .await
            .map_err(|e| ProxyError::Internal {
                detail: format!("LISTEN {ROUTING_CHANGED_CHANNEL}: {e}"),
            })?;
        Ok(listener)
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_insert_payload() {
        let sid = uuid::Uuid::new_v4();
        let aid = uuid::Uuid::new_v4();
        let payload = format!(
            r#"{{"op":"insert","sandbox_id":"{sid}","agent_id":"{aid}"}}"#
        );
        match RoutingChange::parse(&payload) {
            Some(RoutingChange::Insert {
                sandbox_id,
                agent_id,
            }) => {
                assert_eq!(sandbox_id.0, sid);
                assert_eq!(agent_id.0, aid);
            }
            other => panic!("expected Insert, got {other:?}"),
        }
    }

    #[test]
    fn parses_remove_payload() {
        let sid = uuid::Uuid::new_v4();
        let payload = format!(r#"{{"op":"remove","sandbox_id":"{sid}"}}"#);
        match RoutingChange::parse(&payload) {
            Some(RoutingChange::Remove { sandbox_id }) => {
                assert_eq!(sandbox_id.0, sid);
            }
            other => panic!("expected Remove, got {other:?}"),
        }
    }

    #[test]
    fn returns_none_for_unknown_op() {
        let sid = uuid::Uuid::new_v4();
        let payload = format!(r#"{{"op":"???","sandbox_id":"{sid}"}}"#);
        assert!(RoutingChange::parse(&payload).is_none());
    }

    #[test]
    fn returns_none_for_malformed_payload() {
        assert!(RoutingChange::parse("not json").is_none());
        assert!(RoutingChange::parse(r#"{"op":"insert"}"#).is_none()); // missing sandbox_id
    }
}
