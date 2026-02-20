use std::{collections::HashSet, path::Path};

use async_trait::async_trait;
use libsql::{Builder, Connection, Database, params};
use serde_json::json;
use types::{
    Memory, MemoryConfig, MemoryError, MemoryForgetRequest, MemoryRecallRequest, MemoryRecord,
    MemoryStoreRequest,
};

const MIGRATION_BOOKKEEPING_TABLE: &str = "memory_migrations";
const REQUIRED_TABLES: &[&str] = &[
    MIGRATION_BOOKKEEPING_TABLE,
    "sessions",
    "conversation_events",
    "session_state",
    "files",
    "chunks",
    "chunks_vec",
    "chunks_fts",
];
const REQUIRED_INDEXES: &[&str] = &[
    "idx_conversation_events_session_sequence",
    "idx_sessions_updated_at",
    "idx_files_session_source_uri",
    "idx_chunks_session_created_at",
    "idx_chunks_file_id",
    "idx_chunks_session_content_hash",
];
const REQUIRED_TRIGGERS: &[&str] = &[
    "trg_chunks_fts_ai",
    "trg_chunks_fts_au",
    "trg_chunks_fts_ad",
];

#[derive(Debug, Clone, Copy)]
struct Migration {
    version: &'static str,
    sql: &'static str,
}

const MIGRATIONS: &[Migration] = &[
    Migration {
        version: "0001_create_sessions_table",
        sql: include_str!("../migrations/0001_create_sessions_table.sql"),
    },
    Migration {
        version: "0002_create_conversation_events_table",
        sql: include_str!("../migrations/0002_create_conversation_events_table.sql"),
    },
    Migration {
        version: "0003_create_conversation_events_restore_index",
        sql: include_str!("../migrations/0003_create_conversation_events_restore_index.sql"),
    },
    Migration {
        version: "0004_create_session_state_table",
        sql: include_str!("../migrations/0004_create_session_state_table.sql"),
    },
    Migration {
        version: "0005_create_sessions_updated_at_index",
        sql: include_str!("../migrations/0005_create_sessions_updated_at_index.sql"),
    },
    Migration {
        version: "0006_create_files_table",
        sql: include_str!("../migrations/0006_create_files_table.sql"),
    },
    Migration {
        version: "0007_create_chunks_table",
        sql: include_str!("../migrations/0007_create_chunks_table.sql"),
    },
    Migration {
        version: "0008_create_chunks_vec_table",
        sql: include_str!("../migrations/0008_create_chunks_vec_table.sql"),
    },
    Migration {
        version: "0009_create_chunks_fts_table",
        sql: include_str!("../migrations/0009_create_chunks_fts_table.sql"),
    },
    Migration {
        version: "0010_create_files_session_source_index",
        sql: include_str!("../migrations/0010_create_files_session_source_index.sql"),
    },
    Migration {
        version: "0011_create_chunks_session_recency_index",
        sql: include_str!("../migrations/0011_create_chunks_session_recency_index.sql"),
    },
    Migration {
        version: "0012_create_chunks_file_lookup_index",
        sql: include_str!("../migrations/0012_create_chunks_file_lookup_index.sql"),
    },
    Migration {
        version: "0013_create_chunks_session_hash_index",
        sql: include_str!("../migrations/0013_create_chunks_session_hash_index.sql"),
    },
    Migration {
        version: "0014_create_chunks_fts_insert_trigger",
        sql: include_str!("../migrations/0014_create_chunks_fts_insert_trigger.sql"),
    },
    Migration {
        version: "0015_create_chunks_fts_update_trigger",
        sql: include_str!("../migrations/0015_create_chunks_fts_update_trigger.sql"),
    },
    Migration {
        version: "0016_create_chunks_fts_delete_trigger",
        sql: include_str!("../migrations/0016_create_chunks_fts_delete_trigger.sql"),
    },
];

#[derive(Debug, Clone, PartialEq, Eq)]
enum ConnectionStrategy {
    Local { db_path: String },
    Remote { url: String, auth_token: String },
}

impl ConnectionStrategy {
    fn from_config(config: &MemoryConfig) -> Result<Option<Self>, MemoryError> {
        if !config.enabled {
            return Ok(None);
        }

        if let Some(url) = config
            .remote_url
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            let auth_token = config
                .auth_token
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    initialization_error(format!(
                        "remote memory mode requires auth_token when remote_url is set ({url})"
                    ))
                })?;
            return Ok(Some(Self::Remote {
                url: url.to_owned(),
                auth_token: auth_token.to_owned(),
            }));
        }

        let db_path = config.db_path.trim();
        if db_path.is_empty() {
            return Err(initialization_error(
                "local memory mode requires a non-empty db_path".to_owned(),
            ));
        }

        Ok(Some(Self::Local {
            db_path: db_path.to_owned(),
        }))
    }
}

pub struct LibsqlMemory {
    db: Database,
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

        let memory = Self { db };
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

        let payload_json = serde_json::to_string(&request.payload)?;
        let session_state = serde_json::to_string(&json!({
            "last_sequence": request.sequence
        }))?;
        let sequence = i64::try_from(request.sequence)
            .map_err(|_| query_error("store sequence exceeds sqlite integer range".to_owned()))?;

        conn.execute("BEGIN IMMEDIATE TRANSACTION", params![])
            .await
            .map_err(|error| query_error(error.to_string()))?;
        let transaction_result = async {
            ensure_monotonic_sequence(&conn, request.session_id.as_str(), sequence).await?;

            conn.execute(
                "INSERT INTO sessions (session_id, agent_identity, created_at, updated_at)
                 VALUES (?1, NULL, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)
                 ON CONFLICT(session_id) DO UPDATE SET updated_at = CURRENT_TIMESTAMP",
                params![request.session_id.as_str()],
            )
            .await
            .map_err(|error| query_error(error.to_string()))?;

            conn.execute(
                "INSERT INTO conversation_events (session_id, sequence, payload_json, created_at)
                 VALUES (?1, ?2, ?3, CURRENT_TIMESTAMP)",
                params![request.session_id.as_str(), sequence, payload_json],
            )
            .await
            .map_err(|error| query_error(error.to_string()))?;

            conn.execute(
                "INSERT INTO session_state (session_id, state_json, updated_at)
                 VALUES (?1, ?2, CURRENT_TIMESTAMP)
                 ON CONFLICT(session_id) DO UPDATE SET
                     state_json = excluded.state_json,
                     updated_at = CURRENT_TIMESTAMP",
                params![request.session_id.as_str(), session_state],
            )
            .await
            .map_err(|error| query_error(error.to_string()))?;

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
            session_id: request.session_id,
            sequence: request.sequence,
            payload: request.payload,
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

async fn rollback_quietly(conn: &Connection) {
    let _ = conn.execute("ROLLBACK TRANSACTION", params![]).await;
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

fn ensure_local_parent_directory(db_path: &str) -> Result<(), MemoryError> {
    let path = Path::new(db_path);
    let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return Ok(());
    };
    std::fs::create_dir_all(parent).map_err(|error| {
        initialization_error(format!(
            "failed to prepare local memory directory `{}`: {error}",
            parent.display()
        ))
    })?;
    Ok(())
}

async fn enable_foreign_keys(conn: &Connection) -> Result<(), MemoryError> {
    conn.execute("PRAGMA foreign_keys = ON", params![])
        .await
        .map_err(|error| initialization_error(error.to_string()))?;
    Ok(())
}

async fn ensure_migration_bookkeeping(conn: &Connection) -> Result<(), MemoryError> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS memory_migrations (
            version TEXT PRIMARY KEY,
            applied_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        )",
        params![],
    )
    .await
    .map_err(|error| migration_error(error.to_string()))?;
    Ok(())
}

async fn run_pending_migrations(conn: &Connection) -> Result<(), MemoryError> {
    let applied_versions = applied_migration_versions(conn).await?;
    for migration in MIGRATIONS {
        if applied_versions.contains(migration.version) {
            continue;
        }

        conn.execute("BEGIN IMMEDIATE TRANSACTION", params![])
            .await
            .map_err(|error| migration_error(error.to_string()))?;
        let migration_result = async {
            conn.execute(migration.sql, params![])
                .await
                .map_err(|error| migration_error(error.to_string()))?;
            conn.execute(
                "INSERT INTO memory_migrations (version) VALUES (?1)",
                params![migration.version],
            )
            .await
            .map_err(|error| migration_error(error.to_string()))?;
            Ok::<(), MemoryError>(())
        }
        .await;
        if let Err(error) = migration_result {
            rollback_quietly(conn).await;
            return Err(error);
        }
        conn.execute("COMMIT TRANSACTION", params![])
            .await
            .map_err(|error| migration_error(error.to_string()))?;
    }
    Ok(())
}

async fn applied_migration_versions(conn: &Connection) -> Result<HashSet<String>, MemoryError> {
    let mut rows = conn
        .query(
            "SELECT version FROM memory_migrations ORDER BY version ASC",
            params![],
        )
        .await
        .map_err(|error| migration_error(error.to_string()))?;

    let mut versions = HashSet::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|error| migration_error(error.to_string()))?
    {
        let version = row
            .get::<String>(0)
            .map_err(|error| migration_error(error.to_string()))?;
        versions.insert(version);
    }
    Ok(versions)
}

async fn verify_required_schema(conn: &Connection) -> Result<(), MemoryError> {
    for table in REQUIRED_TABLES {
        if !schema_exists(conn, "table", table).await? {
            return Err(initialization_error(format!(
                "required table `{table}` is missing after migration"
            )));
        }
    }
    for index in REQUIRED_INDEXES {
        if !schema_exists(conn, "index", index).await? {
            return Err(initialization_error(format!(
                "required index `{index}` is missing after migration"
            )));
        }
    }
    for trigger in REQUIRED_TRIGGERS {
        if !schema_exists(conn, "trigger", trigger).await? {
            return Err(initialization_error(format!(
                "required trigger `{trigger}` is missing after migration"
            )));
        }
    }
    Ok(())
}

async fn schema_exists(
    conn: &Connection,
    schema_type: &str,
    object_name: &str,
) -> Result<bool, MemoryError> {
    let mut rows = conn
        .query(
            "SELECT 1 FROM sqlite_master WHERE type = ?1 AND name = ?2 LIMIT 1",
            params![schema_type, object_name],
        )
        .await
        .map_err(|error| initialization_error(error.to_string()))?;
    rows.next()
        .await
        .map(|row| row.is_some())
        .map_err(|error| initialization_error(error.to_string()))
}

fn connection_error(message: String) -> MemoryError {
    MemoryError::Connection { message }
}

fn initialization_error(message: String) -> MemoryError {
    MemoryError::Initialization { message }
}

fn migration_error(message: String) -> MemoryError {
    MemoryError::Migration { message }
}

fn query_error(message: String) -> MemoryError {
    MemoryError::Query { message }
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    use libsql::{Builder, params};
    use serde_json::json;
    use types::{
        Memory, MemoryConfig, MemoryError, MemoryForgetRequest, MemoryRecallRequest,
        MemoryStoreRequest,
    };

    use super::{
        ConnectionStrategy, LibsqlMemory, MIGRATIONS, REQUIRED_INDEXES, REQUIRED_TABLES,
        REQUIRED_TRIGGERS, applied_migration_versions, enable_foreign_keys,
        ensure_migration_bookkeeping, run_pending_migrations, schema_exists,
    };

    #[tokio::test]
    async fn from_config_returns_none_when_memory_is_disabled() {
        let backend = LibsqlMemory::from_config(&MemoryConfig::default())
            .await
            .expect("disabled memory should not fail");
        assert!(backend.is_none());
    }

    #[tokio::test]
    async fn remote_connection_strategy_requires_auth_token() {
        let config = MemoryConfig {
            enabled: true,
            db_path: ".oxydra/memory.db".to_owned(),
            remote_url: Some("libsql://example-org.turso.io".to_owned()),
            auth_token: None,
            retrieval: types::RetrievalConfig::default(),
        };
        let error = ConnectionStrategy::from_config(&config)
            .expect_err("remote mode without auth token should fail");
        assert!(matches!(error, MemoryError::Initialization { .. }));
    }

    #[tokio::test]
    async fn enabled_local_memory_initializes_with_missing_parent_directory() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should move forward")
            .as_nanos();
        let mut root = env::temp_dir();
        root.push(format!(
            "oxydra-memory-local-default-{}-{unique}",
            std::process::id()
        ));
        let db_path = root.join("nested").join("memory.db");
        let config = local_memory_config(db_path.to_string_lossy().as_ref());

        let backend = LibsqlMemory::from_config(&config)
            .await
            .expect("local memory should initialize")
            .expect("memory should be enabled");
        backend
            .store(MemoryStoreRequest {
                session_id: "session-local-default".to_owned(),
                sequence: 1,
                payload: json!({"role":"user","content":"hello"}),
            })
            .await
            .expect("store should succeed for local mode");

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn store_recall_and_forget_are_durable_across_restarts() {
        let db_path = temp_db_path("roundtrip");
        let config = local_memory_config(&db_path);

        let backend = LibsqlMemory::from_config(&config)
            .await
            .expect("local memory should initialize")
            .expect("memory should be enabled");
        backend
            .store(MemoryStoreRequest {
                session_id: "session-1".to_owned(),
                sequence: 1,
                payload: json!({"role":"user","content":"hello"}),
            })
            .await
            .expect("first message should store");
        backend
            .store(MemoryStoreRequest {
                session_id: "session-1".to_owned(),
                sequence: 2,
                payload: json!({"role":"assistant","content":"hi"}),
            })
            .await
            .expect("second message should store");
        drop(backend);

        let reopened = LibsqlMemory::from_config(&config)
            .await
            .expect("reopen should succeed")
            .expect("memory should remain enabled");
        let records = reopened
            .recall(MemoryRecallRequest {
                session_id: "session-1".to_owned(),
                limit: None,
            })
            .await
            .expect("stored messages should recall");
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].sequence, 1);
        assert_eq!(records[1].sequence, 2);
        let latest = reopened
            .recall(MemoryRecallRequest {
                session_id: "session-1".to_owned(),
                limit: Some(1),
            })
            .await
            .expect("limited recall should be deterministic");
        assert_eq!(latest.len(), 1);
        assert_eq!(latest[0].sequence, 2);

        reopened
            .forget(MemoryForgetRequest {
                session_id: "session-1".to_owned(),
            })
            .await
            .expect("forget should remove session");
        let error = reopened
            .recall(MemoryRecallRequest {
                session_id: "session-1".to_owned(),
                limit: None,
            })
            .await
            .expect_err("forgotten session should no longer be recallable");
        assert!(matches!(error, MemoryError::NotFound { .. }));

        let _ = fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn list_sessions_returns_recent_first_and_honors_limit() {
        let db_path = temp_db_path("session-list");
        let config = local_memory_config(&db_path);
        let backend = LibsqlMemory::from_config(&config)
            .await
            .expect("local memory should initialize")
            .expect("memory should be enabled");
        backend
            .store(MemoryStoreRequest {
                session_id: "session-a".to_owned(),
                sequence: 1,
                payload: json!({"role":"user","content":"first"}),
            })
            .await
            .expect("first session should store");
        backend
            .store(MemoryStoreRequest {
                session_id: "session-b".to_owned(),
                sequence: 1,
                payload: json!({"role":"user","content":"second"}),
            })
            .await
            .expect("second session should store");

        let conn = backend.connect().expect("backend should connect");
        conn.execute(
            "UPDATE sessions SET updated_at = ?2 WHERE session_id = ?1",
            params!["session-a", "2025-01-01 00:00:01"],
        )
        .await
        .expect("session-a timestamp should update");
        conn.execute(
            "UPDATE sessions SET updated_at = ?2 WHERE session_id = ?1",
            params!["session-b", "2025-01-01 00:00:02"],
        )
        .await
        .expect("session-b timestamp should update");

        let sessions = backend
            .list_sessions(None)
            .await
            .expect("session listing should succeed");
        assert_eq!(
            sessions,
            vec!["session-b".to_owned(), "session-a".to_owned()]
        );
        let limited = backend
            .list_sessions(Some(1))
            .await
            .expect("limited session listing should succeed");
        assert_eq!(limited, vec!["session-b".to_owned()]);

        drop(conn);
        let _ = fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn store_rejects_non_monotonic_sequence_for_session() {
        let db_path = temp_db_path("non-monotonic-sequence");
        let config = local_memory_config(&db_path);
        let backend = LibsqlMemory::from_config(&config)
            .await
            .expect("local memory should initialize")
            .expect("memory should be enabled");

        backend
            .store(MemoryStoreRequest {
                session_id: "session-non-monotonic".to_owned(),
                sequence: 2,
                payload: json!({"role":"user","content":"second"}),
            })
            .await
            .expect("first event should store");

        let error = backend
            .store(MemoryStoreRequest {
                session_id: "session-non-monotonic".to_owned(),
                sequence: 1,
                payload: json!({"role":"assistant","content":"first"}),
            })
            .await
            .expect_err("decreasing sequence should fail");
        assert!(matches!(error, MemoryError::Query { .. }));

        let records = backend
            .recall(MemoryRecallRequest {
                session_id: "session-non-monotonic".to_owned(),
                limit: None,
            })
            .await
            .expect("existing events should remain queryable");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].sequence, 2);

        let _ = fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn initialization_applies_pending_migrations_and_verifies_schema() {
        let db_path = temp_db_path("migrations");
        let config = local_memory_config(&db_path);

        let db = Builder::new_local(db_path.clone())
            .build()
            .await
            .expect("seed db should initialize");
        let conn = db.connect().expect("seed db should connect");
        enable_foreign_keys(&conn)
            .await
            .expect("seed db should enable fk support");
        ensure_migration_bookkeeping(&conn)
            .await
            .expect("seed db should create migration bookkeeping");
        conn.execute(MIGRATIONS[0].sql, params![])
            .await
            .expect("seed migration should apply");
        conn.execute(
            "INSERT INTO memory_migrations (version) VALUES (?1)",
            params![MIGRATIONS[0].version],
        )
        .await
        .expect("seed migration should be marked");
        drop(conn);
        drop(db);

        let backend = LibsqlMemory::from_config(&config)
            .await
            .expect("runtime migration pass should succeed")
            .expect("memory should be enabled");
        let conn = backend
            .connect()
            .expect("backend should connect for verification");
        let versions = applied_migration_versions(&conn)
            .await
            .expect("applied versions should be queryable");
        for migration in MIGRATIONS {
            assert!(
                versions.contains(migration.version),
                "missing migration {}",
                migration.version
            );
        }
        for table in REQUIRED_TABLES {
            assert!(
                schema_exists(&conn, "table", table)
                    .await
                    .expect("schema check should work"),
                "missing table `{table}`"
            );
        }
        for index in REQUIRED_INDEXES {
            assert!(
                schema_exists(&conn, "index", index)
                    .await
                    .expect("schema check should work"),
                "missing index `{index}`"
            );
        }
        for trigger in REQUIRED_TRIGGERS {
            assert!(
                schema_exists(&conn, "trigger", trigger)
                    .await
                    .expect("schema check should work"),
                "missing trigger `{trigger}`"
            );
        }

        let _ = fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn chunks_fts_triggers_keep_index_synchronized() {
        let db_path = temp_db_path("chunks-fts-sync");
        let config = local_memory_config(&db_path);
        let backend = LibsqlMemory::from_config(&config)
            .await
            .expect("local memory should initialize")
            .expect("memory should be enabled");

        backend
            .store(MemoryStoreRequest {
                session_id: "session-fts".to_owned(),
                sequence: 1,
                payload: json!({"role":"user","content":"seed"}),
            })
            .await
            .expect("seed session should store");

        let conn = backend.connect().expect("backend should connect");
        conn.execute(
            "INSERT INTO files (session_id, source_uri, content_hash, metadata_json)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                "session-fts",
                "conversation://session-fts",
                "file-hash",
                "{\"kind\":\"conversation\"}"
            ],
        )
        .await
        .expect("file row should insert");
        conn.execute(
            "INSERT INTO chunks (
                chunk_id, session_id, file_id, sequence_start, sequence_end, chunk_text, metadata_json, content_hash
             ) VALUES (
                ?1, ?2, (SELECT file_id FROM files WHERE session_id = ?2 LIMIT 1), ?3, ?4, ?5, ?6, ?7
             )",
            params![
                "chunk-1",
                "session-fts",
                1_i64,
                1_i64,
                "hello world",
                "{\"source\":\"conversation\"}",
                "chunk-hash-1"
            ],
        )
        .await
        .expect("chunk row should insert");

        assert_eq!(count_fts_matches(&conn, "hello").await, 1);

        conn.execute(
            "UPDATE chunks SET chunk_text = ?2, content_hash = ?3 WHERE chunk_id = ?1",
            params!["chunk-1", "updated content", "chunk-hash-2"],
        )
        .await
        .expect("chunk row should update");
        assert_eq!(count_fts_matches(&conn, "hello").await, 0);
        assert_eq!(count_fts_matches(&conn, "updated").await, 1);

        conn.execute("DELETE FROM chunks WHERE chunk_id = ?1", params!["chunk-1"])
            .await
            .expect("chunk row should delete");
        assert_eq!(count_fts_matches(&conn, "updated").await, 0);

        drop(conn);
        let _ = fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn local_mode_surfaces_unreachable_database_path_errors() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should move forward")
            .as_nanos();
        let mut blocker = env::temp_dir();
        blocker.push(format!(
            "oxydra-memory-local-unreachable-{}-{unique}",
            std::process::id()
        ));
        fs::write(&blocker, "blocker file").expect("blocker file should be writable");

        let db_path = blocker.join("memory.db").to_string_lossy().to_string();
        let error = match LibsqlMemory::new_local(db_path).await {
            Ok(_) => panic!("path with file parent should fail"),
            Err(error) => error,
        };
        assert!(matches!(error, MemoryError::Initialization { .. }));

        let _ = fs::remove_file(blocker);
    }

    #[tokio::test]
    async fn migration_failures_are_surfaced_without_silent_success() {
        let db_path = temp_db_path("migration-failure");
        let db = Builder::new_local(db_path.clone())
            .build()
            .await
            .expect("seed db should initialize");
        let conn = db.connect().expect("seed db should connect");
        enable_foreign_keys(&conn)
            .await
            .expect("seed db should enable fk support");
        ensure_migration_bookkeeping(&conn)
            .await
            .expect("seed db should create migration bookkeeping");
        conn.execute("BEGIN IMMEDIATE TRANSACTION", params![])
            .await
            .expect("test transaction should start");

        let error = run_pending_migrations(&conn)
            .await
            .expect_err("nested migration transaction should fail");
        assert!(matches!(error, MemoryError::Migration { .. }));

        conn.execute("ROLLBACK TRANSACTION", params![])
            .await
            .expect("test transaction should roll back");
        drop(conn);
        drop(db);
        let _ = fs::remove_file(db_path);
    }

    fn local_memory_config(db_path: &str) -> MemoryConfig {
        MemoryConfig {
            enabled: true,
            db_path: db_path.to_owned(),
            remote_url: None,
            auth_token: None,
            retrieval: types::RetrievalConfig::default(),
        }
    }

    fn temp_db_path(label: &str) -> String {
        let mut path = env::temp_dir();
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should move forward")
            .as_nanos();
        path.push(format!(
            "oxydra-memory-{label}-{}-{unique}.db",
            std::process::id()
        ));
        path.to_string_lossy().to_string()
    }

    async fn count_fts_matches(conn: &libsql::Connection, term: &str) -> i64 {
        let mut rows = conn
            .query(
                "SELECT COUNT(*) FROM chunks_fts WHERE chunks_fts MATCH ?1",
                params![term],
            )
            .await
            .expect("count query should run");
        let row = rows
            .next()
            .await
            .expect("count row should be readable")
            .expect("count row should exist");
        row.get::<i64>(0).expect("count column should be readable")
    }
}
