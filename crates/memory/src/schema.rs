use std::collections::HashSet;

use libsql::{Connection, params};
use types::MemoryError;

use crate::errors::{initialization_error, migration_error};

const MIGRATION_BOOKKEEPING_TABLE: &str = "memory_migrations";
pub(crate) const REQUIRED_TABLES: &[&str] = &[
    MIGRATION_BOOKKEEPING_TABLE,
    "sessions",
    "conversation_events",
    "session_state",
    "files",
    "chunks",
    "chunks_vec",
    "chunks_fts",
    "schedules",
    "schedule_runs",
];
pub(crate) const REQUIRED_INDEXES: &[&str] = &[
    "idx_conversation_events_session_sequence",
    "idx_sessions_updated_at",
    "idx_files_session_source_uri",
    "idx_chunks_session_created_at",
    "idx_chunks_file_id",
    "idx_chunks_session_content_hash",
    "idx_schedules_user_id",
    "idx_schedules_due",
    "idx_schedules_status",
    "idx_schedules_name",
    "idx_schedule_runs_lookup",
];
pub(crate) const REQUIRED_TRIGGERS: &[&str] = &[
    "trg_chunks_fts_ai",
    "trg_chunks_fts_au",
    "trg_chunks_fts_ad",
];

#[derive(Debug, Clone, Copy)]
pub(crate) struct Migration {
    pub(crate) version: &'static str,
    pub(crate) sql: &'static str,
}

pub(crate) const MIGRATIONS: &[Migration] = &[
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
    Migration {
        version: "0017_create_schedules_table",
        sql: include_str!("../migrations/0017_create_schedules_table.sql"),
    },
    Migration {
        version: "0018_create_schedules_indexes",
        sql: include_str!("../migrations/0018_create_schedules_indexes.sql"),
    },
    Migration {
        version: "0019_create_schedule_runs_table",
        sql: include_str!("../migrations/0019_create_schedule_runs_table.sql"),
    },
];

pub(crate) async fn rollback_quietly(conn: &Connection) {
    let _ = conn.execute("ROLLBACK TRANSACTION", params![]).await;
}

pub(crate) async fn enable_foreign_keys(conn: &Connection) -> Result<(), MemoryError> {
    conn.execute("PRAGMA foreign_keys = ON", params![])
        .await
        .map_err(|error| initialization_error(error.to_string()))?;
    // busy_timeout is per-connection: wait up to 5 s when the database is locked
    // instead of immediately returning SQLITE_BUSY.
    // busy_timeout returns a result row, so we use query() not execute().
    let mut rows = conn
        .query("PRAGMA busy_timeout = 5000", params![])
        .await
        .map_err(|error| initialization_error(error.to_string()))?;
    let _ = rows.next().await;
    Ok(())
}

/// Apply database-level pragmas that improve concurrent access.
///
/// - **WAL mode** allows readers to proceed while a writer holds the lock,
///   dramatically reducing lock contention compared to the default
///   `journal_mode=delete`.
///
/// This only needs to be called once (typically during initialisation) because
/// `journal_mode` is persisted across connections. The per-connection
/// `busy_timeout` pragma is applied in [`enable_foreign_keys`] which is called
/// on every new connection.
pub(crate) async fn enable_wal_mode(conn: &Connection) -> Result<(), MemoryError> {
    // journal_mode returns a result row, so we must use query() not execute().
    let mut rows = conn
        .query("PRAGMA journal_mode = WAL", params![])
        .await
        .map_err(|error| initialization_error(format!("failed to enable WAL mode: {error}")))?;
    // Consume the result row to complete the pragma.
    let _ = rows.next().await;
    Ok(())
}

pub(crate) async fn ensure_migration_bookkeeping(conn: &Connection) -> Result<(), MemoryError> {
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

pub(crate) async fn run_pending_migrations(conn: &Connection) -> Result<(), MemoryError> {
    let applied_versions = applied_migration_versions(conn).await?;
    for migration in MIGRATIONS {
        if applied_versions.contains(migration.version) {
            continue;
        }

        conn.execute("BEGIN IMMEDIATE TRANSACTION", params![])
            .await
            .map_err(|error| migration_error(error.to_string()))?;
        let migration_result = async {
            conn.execute_batch(migration.sql)
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

pub(crate) async fn applied_migration_versions(
    conn: &Connection,
) -> Result<HashSet<String>, MemoryError> {
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

pub(crate) async fn verify_required_schema(conn: &Connection) -> Result<(), MemoryError> {
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

pub(crate) async fn schema_exists(
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
