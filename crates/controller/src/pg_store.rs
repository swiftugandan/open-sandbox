use sqlx::PgPool;

use open_sandbox_contracts::error::ControllerError;
use open_sandbox_contracts::types::{AgentId, RoutingEntry, SandboxId};

use crate::store::*;

pub struct PgStore {
    pool: PgPool,
}

impl PgStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn migrate(&self) -> Result<(), ControllerError> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS agents (
                agent_id UUID PRIMARY KEY,
                cpu_cores INTEGER NOT NULL,
                memory_bytes BIGINT NOT NULL,
                available_cpu_millicores INTEGER NOT NULL,
                available_memory_bytes BIGINT NOT NULL,
                running_sandboxes INTEGER NOT NULL,
                state TEXT NOT NULL DEFAULT 'active'
            )",
        )
        .execute(&self.pool)
        .await
        .map_err(|e| ControllerError::Database {
            detail: e.to_string(),
        })?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS routing_entries (
                sandbox_id UUID PRIMARY KEY,
                agent_id UUID NOT NULL REFERENCES agents(agent_id)
            )",
        )
        .execute(&self.pool)
        .await
        .map_err(|e| ControllerError::Database {
            detail: e.to_string(),
        })?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS sandboxes (
                sandbox_id UUID PRIMARY KEY,
                agent_id UUID NOT NULL,
                state TEXT NOT NULL DEFAULT 'creating',
                error TEXT,
                created_at TIMESTAMPTZ NOT NULL DEFAULT now()
            )",
        )
        .execute(&self.pool)
        .await
        .map_err(|e| ControllerError::Database {
            detail: e.to_string(),
        })?;

        // Idempotent migration for pre-existing deployments. Safe to
        // run on every startup; PostgreSQL silently no-ops if the
        // column already exists.
        sqlx::query("ALTER TABLE sandboxes ADD COLUMN IF NOT EXISTS error TEXT")
            .execute(&self.pool)
            .await
            .map_err(|e| ControllerError::Database {
                detail: e.to_string(),
            })?;

        Ok(())
    }
}

impl ControllerStore for PgStore {
    async fn save_agent(&self, record: AgentRecord) -> Result<(), ControllerError> {
        sqlx::query(
            "INSERT INTO agents (agent_id, cpu_cores, memory_bytes, available_cpu_millicores, available_memory_bytes, running_sandboxes, state)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (agent_id) DO UPDATE SET
                cpu_cores = EXCLUDED.cpu_cores,
                memory_bytes = EXCLUDED.memory_bytes,
                available_cpu_millicores = EXCLUDED.available_cpu_millicores,
                available_memory_bytes = EXCLUDED.available_memory_bytes,
                running_sandboxes = EXCLUDED.running_sandboxes,
                state = EXCLUDED.state",
        )
        .bind(record.agent_id.0)
        .bind(record.capacity.cpu_cores as i32)
        .bind(record.capacity.memory_bytes as i64)
        .bind(record.available.cpu_millicores as i32)
        .bind(record.available.memory_bytes as i64)
        .bind(record.available.running_sandboxes as i32)
        .bind(state_to_str(&record.state))
        .execute(&self.pool)
        .await
        .map_err(|e| ControllerError::Database {
            detail: e.to_string(),
        })?;
        Ok(())
    }

    async fn get_agent(&self, id: &AgentId) -> Result<Option<AgentRecord>, ControllerError> {
        let row = sqlx::query_as::<_, AgentRow>(
            "SELECT agent_id, cpu_cores, memory_bytes, available_cpu_millicores, available_memory_bytes, running_sandboxes, state
             FROM agents WHERE agent_id = $1",
        )
        .bind(id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| ControllerError::Database {
            detail: e.to_string(),
        })?;
        Ok(row.map(AgentRow::into_record))
    }

    async fn remove_agent(&self, id: &AgentId) -> Result<(), ControllerError> {
        sqlx::query("DELETE FROM agents WHERE agent_id = $1")
            .bind(id.0)
            .execute(&self.pool)
            .await
            .map_err(|e| ControllerError::Database {
                detail: e.to_string(),
            })?;
        Ok(())
    }

    async fn list_active_agents(&self) -> Result<Vec<AgentRecord>, ControllerError> {
        let rows = sqlx::query_as::<_, AgentRow>(
            "SELECT agent_id, cpu_cores, memory_bytes, available_cpu_millicores, available_memory_bytes, running_sandboxes, state
             FROM agents WHERE state = 'active'",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| ControllerError::Database {
            detail: e.to_string(),
        })?;
        Ok(rows.into_iter().map(AgentRow::into_record).collect())
    }

    async fn update_agent_state(
        &self,
        id: &AgentId,
        state: AgentState,
    ) -> Result<(), ControllerError> {
        let result = sqlx::query("UPDATE agents SET state = $1 WHERE agent_id = $2")
            .bind(state_to_str(&state))
            .bind(id.0)
            .execute(&self.pool)
            .await
            .map_err(|e| ControllerError::Database {
                detail: e.to_string(),
            })?;
        if result.rows_affected() == 0 {
            return Err(ControllerError::AgentNotFound {
                agent_id: id.to_string(),
            });
        }
        Ok(())
    }

    async fn insert_routing_entry(&self, entry: RoutingEntry) -> Result<(), ControllerError> {
        sqlx::query(
            "INSERT INTO routing_entries (sandbox_id, agent_id) VALUES ($1, $2)
             ON CONFLICT (sandbox_id) DO UPDATE SET agent_id = EXCLUDED.agent_id",
        )
        .bind(entry.sandbox_id.0)
        .bind(entry.agent_id.0)
        .execute(&self.pool)
        .await
        .map_err(|e| ControllerError::Database {
            detail: e.to_string(),
        })?;
        Ok(())
    }

    async fn remove_routing_entries_for_agent(
        &self,
        agent_id: &AgentId,
    ) -> Result<(), ControllerError> {
        sqlx::query("DELETE FROM routing_entries WHERE agent_id = $1")
            .bind(agent_id.0)
            .execute(&self.pool)
            .await
            .map_err(|e| ControllerError::Database {
                detail: e.to_string(),
            })?;
        Ok(())
    }

    async fn find_routing_entry(
        &self,
        sandbox_id: &SandboxId,
    ) -> Result<Option<RoutingEntry>, ControllerError> {
        let row = sqlx::query_as::<_, RoutingRow>(
            "SELECT sandbox_id, agent_id FROM routing_entries WHERE sandbox_id = $1",
        )
        .bind(sandbox_id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| ControllerError::Database {
            detail: e.to_string(),
        })?;
        Ok(row.map(|r| RoutingEntry {
            sandbox_id: SandboxId(r.sandbox_id),
            agent_id: AgentId(r.agent_id),
        }))
    }

    async fn list_routing_entries(&self) -> Result<Vec<RoutingEntry>, ControllerError> {
        let rows = sqlx::query_as::<_, RoutingRow>(
            "SELECT sandbox_id, agent_id FROM routing_entries ORDER BY sandbox_id",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| ControllerError::Database {
            detail: e.to_string(),
        })?;
        Ok(rows
            .into_iter()
            .map(|r| RoutingEntry {
                sandbox_id: SandboxId(r.sandbox_id),
                agent_id: AgentId(r.agent_id),
            })
            .collect())
    }

    async fn remove_routing_entry(&self, sandbox_id: &SandboxId) -> Result<(), ControllerError> {
        sqlx::query("DELETE FROM routing_entries WHERE sandbox_id = $1")
            .bind(sandbox_id.0)
            .execute(&self.pool)
            .await
            .map_err(|e| ControllerError::Database {
                detail: e.to_string(),
            })?;
        Ok(())
    }

    async fn save_sandbox_state(
        &self,
        sandbox_id: &SandboxId,
        agent_id: &AgentId,
        state: &str,
        error: Option<&str>,
    ) -> Result<(), ControllerError> {
        sqlx::query(
            "INSERT INTO sandboxes (sandbox_id, agent_id, state, error)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (sandbox_id) DO UPDATE SET
                state = EXCLUDED.state,
                error = EXCLUDED.error",
        )
        .bind(sandbox_id.0)
        .bind(agent_id.0)
        .bind(state)
        .bind(error)
        .execute(&self.pool)
        .await
        .map_err(|e| ControllerError::Database {
            detail: e.to_string(),
        })?;
        Ok(())
    }

    async fn get_sandbox_state(
        &self,
        sandbox_id: &SandboxId,
    ) -> Result<Option<crate::store::SandboxStateRow>, ControllerError> {
        let row: Option<(String, Option<String>)> =
            sqlx::query_as("SELECT state, error FROM sandboxes WHERE sandbox_id = $1")
                .bind(sandbox_id.0)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| ControllerError::Database {
                    detail: e.to_string(),
                })?;
        Ok(row.map(|(state, error)| crate::store::SandboxStateRow { state, error }))
    }

    async fn mark_agent_dead_atomic(
        &self,
        agent_id: &AgentId,
    ) -> Result<(), ControllerError> {
        let mut tx = self.pool.begin().await.map_err(|e| ControllerError::Database {
            detail: e.to_string(),
        })?;

        let result = sqlx::query("UPDATE agents SET state = $1 WHERE agent_id = $2")
            .bind(state_to_str(&AgentState::Dead))
            .bind(agent_id.0)
            .execute(&mut *tx)
            .await
            .map_err(|e| ControllerError::Database {
                detail: e.to_string(),
            })?;
        if result.rows_affected() == 0 {
            return Err(ControllerError::AgentNotFound {
                agent_id: agent_id.to_string(),
            });
        }

        sqlx::query("DELETE FROM routing_entries WHERE agent_id = $1")
            .bind(agent_id.0)
            .execute(&mut *tx)
            .await
            .map_err(|e| ControllerError::Database {
                detail: e.to_string(),
            })?;

        tx.commit().await.map_err(|e| ControllerError::Database {
            detail: e.to_string(),
        })?;
        Ok(())
    }
}

fn state_to_str(state: &AgentState) -> &'static str {
    match state {
        AgentState::Active => "active",
        AgentState::Dead => "dead",
    }
}

fn state_from_str(s: &str) -> AgentState {
    match s {
        "dead" => AgentState::Dead,
        _ => AgentState::Active,
    }
}

#[derive(sqlx::FromRow)]
struct RoutingRow {
    sandbox_id: uuid::Uuid,
    agent_id: uuid::Uuid,
}

#[derive(sqlx::FromRow)]
struct AgentRow {
    agent_id: uuid::Uuid,
    cpu_cores: i32,
    memory_bytes: i64,
    available_cpu_millicores: i32,
    available_memory_bytes: i64,
    running_sandboxes: i32,
    state: String,
}

impl AgentRow {
    fn into_record(self) -> AgentRecord {
        AgentRecord {
            agent_id: AgentId(self.agent_id),
            capacity: AgentCapacity {
                cpu_cores: self.cpu_cores as u32,
                memory_bytes: self.memory_bytes as u64,
            },
            available: AvailableResources {
                cpu_millicores: self.available_cpu_millicores as u32,
                memory_bytes: self.available_memory_bytes as u64,
                running_sandboxes: self.running_sandboxes as u32,
            },
            state: state_from_str(&self.state),
        }
    }
}
