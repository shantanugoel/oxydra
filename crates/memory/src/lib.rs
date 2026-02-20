mod connection;
mod errors;
mod indexing;
mod schema;

use async_trait::async_trait;
use libsql::{Builder, Connection, Database, params};
use serde_json::json;
use types::{
    Memory, MemoryConfig, MemoryError, MemoryForgetRequest, MemoryRecallRequest, MemoryRecord,
    MemoryStoreRequest,
};

use connection::{ConnectionStrategy, ensure_local_parent_directory};
use errors::{connection_error, initialization_error, query_error};
use indexing::{EmbeddingAdapter, index_prepared_document, prepare_index_document};
use schema::{
    enable_foreign_keys, ensure_migration_bookkeeping, rollback_quietly, run_pending_migrations,
    verify_required_schema,
};

pub struct LibsqlMemory {
    db: Database,
    embedding_adapter: EmbeddingAdapter,
}

impl LibsqlMemory {
    pub async fn from_config(config: &MemoryConfig) -> Result<Option<Self>, MemoryError> {
        let Some(strategy) = ConnectionStrategy::from_config(config)? else {
            return Ok(None);
        };
        Self::open(strategy).await.map(Some)
    }

    pub async fn new_local(db_path: impl Into<String>) -> Result<Self, MemoryError> {
        Self::open(ConnectionStrategy::Local {
            db_path: db_path.into(),
        })
        .await
    }

    pub async fn new_remote(
        url: impl Into<String>,
        auth_token: impl Into<String>,
    ) -> Result<Self, MemoryError> {
        Self::open(ConnectionStrategy::Remote {
            url: url.into(),
            auth_token: auth_token.into(),
        })
        .await
    }

    pub async fn list_sessions(&self, limit: Option<u64>) -> Result<Vec<String>, MemoryError> {
        let conn = self.connect()?;
        enable_foreign_keys(&conn).await?;
        let mut rows = if let Some(limit) = limit {
            let limit = i64::try_from(limit).map_err(|_| {
                query_error("session listing limit exceeds sqlite integer range".to_owned())
            })?;
            conn.query(
                "SELECT session_id FROM sessions ORDER BY updated_at DESC, session_id ASC LIMIT ?1",
                params![limit],
            )
            .await
            .map_err(|error| query_error(error.to_string()))?
        } else {
            conn.query(
                "SELECT session_id FROM sessions ORDER BY updated_at DESC, session_id ASC",
                params![],
            )
            .await
            .map_err(|error| query_error(error.to_string()))?
        };

        let mut sessions = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|error| query_error(error.to_string()))?
        {
            let session_id = row
                .get::<String>(0)
                .map_err(|error| query_error(error.to_string()))?;
            sessions.push(session_id);
        }

        Ok(sessions)
    }

    async fn open(strategy: ConnectionStrategy) -> Result<Self, MemoryError> {
        let db = match strategy {
            ConnectionStrategy::Local { db_path } => {
                ensure_local_parent_directory(&db_path)?;
                Builder::new_local(db_path)
                    .build()
                    .await
                    .map_err(|error| connection_error(error.to_string()))?
            }
            ConnectionStrategy::Remote { url, auth_token } => Builder::new_remote(url, auth_token)
                .build()
                .await
                .map_err(|error| connection_error(error.to_string()))?,
        };

        let memory = Self {
            db,
            embedding_adapter: EmbeddingAdapter,
        };
        memory.initialize().await?;
        Ok(memory)
    }

    async fn initialize(&self) -> Result<(), MemoryError> {
        let conn = self.connect()?;
        enable_foreign_keys(&conn).await?;
        ensure_migration_bookkeeping(&conn).await?;
        run_pending_migrations(&conn).await?;
        verify_required_schema(&conn).await?;
        Ok(())
    }

    fn connect(&self) -> Result<Connection, MemoryError> {
        self.db
            .connect()
            .map_err(|error| connection_error(error.to_string()))
    }
}

#[async_trait]
impl Memory for LibsqlMemory {
    async fn store(&self, request: MemoryStoreRequest) -> Result<MemoryRecord, MemoryError> {
        let conn = self.connect()?;
        enable_foreign_keys(&conn).await?;

        let session_id = request.session_id;
        let payload = request.payload;
        let sequence_u64 = request.sequence;
        let payload_json = serde_json::to_string(&payload)?;
        let session_state = serde_json::to_string(&json!({
            "last_sequence": sequence_u64
        }))?;
        let sequence = i64::try_from(sequence_u64)
            .map_err(|_| query_error("store sequence exceeds sqlite integer range".to_owned()))?;
        let prepared_index = prepare_index_document(
            &self.embedding_adapter,
            session_id.as_str(),
            sequence,
            &payload,
        )?;

        conn.execute("BEGIN IMMEDIATE TRANSACTION", params![])
            .await
            .map_err(|error| query_error(error.to_string()))?;
        let transaction_result = async {
            ensure_monotonic_sequence(&conn, session_id.as_str(), sequence).await?;

            conn.execute(
                "INSERT INTO sessions (session_id, agent_identity, created_at, updated_at)
                 VALUES (?1, NULL, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)
                 ON CONFLICT(session_id) DO UPDATE SET updated_at = CURRENT_TIMESTAMP",
                params![session_id.as_str()],
            )
            .await
            .map_err(|error| query_error(error.to_string()))?;

            conn.execute(
                "INSERT INTO conversation_events (session_id, sequence, payload_json, created_at)
                 VALUES (?1, ?2, ?3, CURRENT_TIMESTAMP)",
                params![session_id.as_str(), sequence, payload_json],
            )
            .await
            .map_err(|error| query_error(error.to_string()))?;

            conn.execute(
                "INSERT INTO session_state (session_id, state_json, updated_at)
                 VALUES (?1, ?2, CURRENT_TIMESTAMP)
                 ON CONFLICT(session_id) DO UPDATE SET
                     state_json = excluded.state_json,
                     updated_at = CURRENT_TIMESTAMP",
                params![session_id.as_str(), session_state],
            )
            .await
            .map_err(|error| query_error(error.to_string()))?;

            if let Some(index_document) = prepared_index.as_ref() {
                index_prepared_document(&conn, session_id.as_str(), sequence, index_document)
                    .await?;
            }

            Ok::<(), MemoryError>(())
        }
        .await;
        if let Err(error) = transaction_result {
            rollback_quietly(&conn).await;
            return Err(error);
        }
        conn.execute("COMMIT TRANSACTION", params![])
            .await
            .map_err(|error| query_error(error.to_string()))?;

        Ok(MemoryRecord {
            session_id,
            sequence: sequence_u64,
            payload,
        })
    }

    async fn recall(&self, request: MemoryRecallRequest) -> Result<Vec<MemoryRecord>, MemoryError> {
        let conn = self.connect()?;
        enable_foreign_keys(&conn).await?;

        let mut rows = if let Some(limit) = request.limit {
            let limit = i64::try_from(limit)
                .map_err(|_| query_error("recall limit exceeds sqlite integer range".to_owned()))?;
            conn.query(
                "SELECT sequence, payload_json FROM conversation_events
                 WHERE session_id = ?1
                 ORDER BY sequence DESC
                 LIMIT ?2",
                params![request.session_id.as_str(), limit],
            )
            .await
            .map_err(|error| query_error(error.to_string()))?
        } else {
            conn.query(
                "SELECT sequence, payload_json FROM conversation_events
                 WHERE session_id = ?1
                 ORDER BY sequence ASC",
                params![request.session_id.as_str()],
            )
            .await
            .map_err(|error| query_error(error.to_string()))?
        };

        let mut records = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|error| query_error(error.to_string()))?
        {
            let sequence = row
                .get::<i64>(0)
                .map_err(|error| query_error(error.to_string()))?;
            let sequence = u64::try_from(sequence)
                .map_err(|_| query_error("stored sequence is negative".to_owned()))?;
            let payload_json = row
                .get::<String>(1)
                .map_err(|error| query_error(error.to_string()))?;
            let payload = serde_json::from_str(&payload_json)?;
            records.push(MemoryRecord {
                session_id: request.session_id.clone(),
                sequence,
                payload,
            });
        }

        if request.limit.is_some() {
            records.reverse();
        }

        if records.is_empty() {
            return Err(MemoryError::NotFound {
                session_id: request.session_id,
            });
        }

        Ok(records)
    }

    async fn forget(&self, request: MemoryForgetRequest) -> Result<(), MemoryError> {
        let conn = self.connect()?;
        enable_foreign_keys(&conn).await?;

        conn.execute("BEGIN IMMEDIATE TRANSACTION", params![])
            .await
            .map_err(|error| query_error(error.to_string()))?;
        let transaction_result = async {
            let deleted_sessions = conn
                .execute(
                    "DELETE FROM sessions WHERE session_id = ?1",
                    params![request.session_id.as_str()],
                )
                .await
                .map_err(|error| query_error(error.to_string()))?;

            if deleted_sessions == 0 {
                return Err(MemoryError::NotFound {
                    session_id: request.session_id.clone(),
                });
            }

            Ok::<(), MemoryError>(())
        }
        .await;
        if let Err(error) = transaction_result {
            rollback_quietly(&conn).await;
            return Err(error);
        }
        conn.execute("COMMIT TRANSACTION", params![])
            .await
            .map_err(|error| query_error(error.to_string()))?;
        Ok(())
    }
}

pub struct UnconfiguredMemory;

impl UnconfiguredMemory {
    pub fn new() -> Self {
        Self
    }

    fn backend_unavailable() -> MemoryError {
        initialization_error(
            "memory backend is not configured; explicit local/remote selection is required"
                .to_owned(),
        )
    }
}

impl Default for UnconfiguredMemory {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Memory for UnconfiguredMemory {
    async fn store(&self, _request: MemoryStoreRequest) -> Result<MemoryRecord, MemoryError> {
        Err(Self::backend_unavailable())
    }

    async fn recall(
        &self,
        _request: MemoryRecallRequest,
    ) -> Result<Vec<MemoryRecord>, MemoryError> {
        Err(Self::backend_unavailable())
    }

    async fn forget(&self, _request: MemoryForgetRequest) -> Result<(), MemoryError> {
        Err(Self::backend_unavailable())
    }
}

async fn ensure_monotonic_sequence(
    conn: &Connection,
    session_id: &str,
    sequence: i64,
) -> Result<(), MemoryError> {
    let mut rows = conn
        .query(
            "SELECT COALESCE(MAX(sequence), -1) FROM conversation_events WHERE session_id = ?1",
            params![session_id],
        )
        .await
        .map_err(|error| query_error(error.to_string()))?;

    let row = rows
        .next()
        .await
        .map_err(|error| query_error(error.to_string()))?
        .ok_or_else(|| query_error("failed to inspect existing sequence state".to_owned()))?;
    let max_sequence = row
        .get::<i64>(0)
        .map_err(|error| query_error(error.to_string()))?;

    if max_sequence >= 0 && sequence <= max_sequence {
        return Err(query_error(format!(
            "store sequence {sequence} must be greater than existing max sequence {max_sequence} for session `{session_id}`"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
