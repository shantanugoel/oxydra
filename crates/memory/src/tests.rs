use std::{
    env, fs,
    time::{SystemTime, UNIX_EPOCH},
};

use libsql::{Builder, params};
use serde_json::json;
use types::{
    Memory, MemoryConfig, MemoryError, MemoryForgetRequest, MemoryHybridQueryRequest,
    MemoryRecallRequest, MemoryRetrieval, MemoryStoreRequest, MemorySummaryReadRequest,
    MemorySummaryWriteRequest, Message, MessageRole,
};

use crate::{
    LibsqlMemory,
    connection::ConnectionStrategy,
    schema::{
        MIGRATIONS, REQUIRED_INDEXES, REQUIRED_TABLES, REQUIRED_TRIGGERS,
        applied_migration_versions, enable_foreign_keys, ensure_migration_bookkeeping,
        run_pending_migrations, schema_exists,
    },
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
async fn store_indexes_message_content_into_chunks_and_vectors() {
    let db_path = temp_db_path("store-indexes-message");
    let config = local_memory_config(&db_path);
    let backend = LibsqlMemory::from_config(&config)
        .await
        .expect("local memory should initialize")
        .expect("memory should be enabled");

    let payload = serde_json::to_value(Message {
        role: MessageRole::Assistant,
        content: Some(
            "hybrid indexing pipeline should normalize and chunk this message body".to_owned(),
        ),
        tool_calls: vec![],
        tool_call_id: None,
    })
    .expect("message payload should serialize");
    backend
        .store(MemoryStoreRequest {
            session_id: "session-indexing".to_owned(),
            sequence: 1,
            payload,
        })
        .await
        .expect("store should also index chunk content");

    let conn = backend.connect().expect("backend should connect");
    let mut chunk_count_rows = conn
        .query(
            "SELECT COUNT(*) FROM chunks WHERE session_id = ?1",
            params!["session-indexing"],
        )
        .await
        .expect("chunk count query should run");
    let chunk_count_row = chunk_count_rows
        .next()
        .await
        .expect("chunk count row should read")
        .expect("chunk count row should exist");
    let chunk_count = chunk_count_row
        .get::<i64>(0)
        .expect("chunk count should be readable");
    assert!(chunk_count > 0);

    let mut vector_count_rows = conn
        .query(
            "SELECT COUNT(*) FROM chunks_vec
             WHERE chunk_id IN (SELECT chunk_id FROM chunks WHERE session_id = ?1)",
            params!["session-indexing"],
        )
        .await
        .expect("vector count query should run");
    let vector_count_row = vector_count_rows
        .next()
        .await
        .expect("vector count row should read")
        .expect("vector count row should exist");
    let vector_count = vector_count_row
        .get::<i64>(0)
        .expect("vector count should be readable");
    assert_eq!(vector_count, chunk_count);

    let mut metadata_rows = conn
        .query(
            "SELECT metadata_json FROM chunks WHERE session_id = ?1 LIMIT 1",
            params!["session-indexing"],
        )
        .await
        .expect("chunk metadata query should run");
    let metadata_row = metadata_rows
        .next()
        .await
        .expect("chunk metadata row should read")
        .expect("chunk metadata row should exist");
    let metadata_json = metadata_row
        .get::<String>(0)
        .expect("metadata json should be readable");
    let metadata: serde_json::Value =
        serde_json::from_str(&metadata_json).expect("metadata json should parse");
    assert_eq!(
        metadata.get("role").and_then(serde_json::Value::as_str),
        Some("assistant")
    );

    drop(conn);
    let _ = fs::remove_file(db_path);
}

#[tokio::test]
async fn store_deduplicates_reindexing_by_chunk_hash() {
    let db_path = temp_db_path("dedupe-indexing");
    let config = local_memory_config(&db_path);
    let backend = LibsqlMemory::from_config(&config)
        .await
        .expect("local memory should initialize")
        .expect("memory should be enabled");

    for sequence in [1_u64, 2_u64] {
        backend
            .store(MemoryStoreRequest {
                session_id: "session-dedupe".to_owned(),
                sequence,
                payload: json!({
                    "role":"user",
                    "content":"same content should not duplicate vector indexing"
                }),
            })
            .await
            .expect("store should succeed");
    }

    let conn = backend.connect().expect("backend should connect");
    let mut chunk_count_rows = conn
        .query(
            "SELECT COUNT(*) FROM chunks WHERE session_id = ?1",
            params!["session-dedupe"],
        )
        .await
        .expect("chunk count query should run");
    let chunk_count_row = chunk_count_rows
        .next()
        .await
        .expect("chunk count row should read")
        .expect("chunk count row should exist");
    let chunk_count = chunk_count_row
        .get::<i64>(0)
        .expect("chunk count should be readable");
    assert_eq!(chunk_count, 1);

    let mut sequence_rows = conn
        .query(
            "SELECT sequence_end FROM chunks WHERE session_id = ?1 LIMIT 1",
            params!["session-dedupe"],
        )
        .await
        .expect("sequence query should run");
    let sequence_row = sequence_rows
        .next()
        .await
        .expect("sequence row should read")
        .expect("sequence row should exist");
    let sequence_end = sequence_row
        .get::<i64>(0)
        .expect("sequence_end should be readable");
    assert_eq!(sequence_end, 2);

    let mut event_rows = conn
        .query(
            "SELECT COUNT(*) FROM conversation_events WHERE session_id = ?1",
            params!["session-dedupe"],
        )
        .await
        .expect("event count query should run");
    let event_row = event_rows
        .next()
        .await
        .expect("event count row should read")
        .expect("event count row should exist");
    let event_count = event_row
        .get::<i64>(0)
        .expect("event count should be readable");
    assert_eq!(event_count, 2);

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
async fn legacy_database_migrates_to_hybrid_schema_without_data_loss() {
    let db_path = temp_db_path("legacy-schema-migration-continuity");
    let config = local_memory_config(&db_path);

    let db = Builder::new_local(db_path.clone())
        .build()
        .await
        .expect("legacy seed db should initialize");
    let conn = db.connect().expect("legacy seed db should connect");
    enable_foreign_keys(&conn)
        .await
        .expect("legacy seed db should enable fk support");
    ensure_migration_bookkeeping(&conn)
        .await
        .expect("legacy seed db should create migration bookkeeping");
    for migration in MIGRATIONS.iter().take(5) {
        conn.execute(migration.sql, params![])
            .await
            .expect("legacy migration should apply");
        conn.execute(
            "INSERT INTO memory_migrations (version) VALUES (?1)",
            params![migration.version],
        )
        .await
        .expect("legacy migration should be marked");
    }

    conn.execute(
        "INSERT INTO sessions (session_id, agent_identity, created_at, updated_at)
         VALUES (?1, ?2, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)",
        params!["session-legacy", "legacy-agent"],
    )
    .await
    .expect("legacy session row should insert");
    conn.execute(
        "INSERT INTO conversation_events (session_id, sequence, payload_json, created_at)
         VALUES (?1, ?2, ?3, CURRENT_TIMESTAMP)",
        params![
            "session-legacy",
            1_i64,
            json!({"role":"user","content":"legacy user"}).to_string()
        ],
    )
    .await
    .expect("legacy event row should insert");
    conn.execute(
        "INSERT INTO conversation_events (session_id, sequence, payload_json, created_at)
         VALUES (?1, ?2, ?3, CURRENT_TIMESTAMP)",
        params![
            "session-legacy",
            2_i64,
            json!({"role":"assistant","content":"legacy assistant"}).to_string()
        ],
    )
    .await
    .expect("legacy event row should insert");
    conn.execute(
        "INSERT INTO session_state (session_id, state_json, updated_at)
         VALUES (?1, ?2, CURRENT_TIMESTAMP)",
        params![
            "session-legacy",
            json!({"last_sequence":2,"legacy_key":"preserve"}).to_string()
        ],
    )
    .await
    .expect("legacy session_state row should insert");
    drop(conn);
    drop(db);

    let backend = LibsqlMemory::from_config(&config)
        .await
        .expect("hybrid-schema migration pass should succeed")
        .expect("memory should be enabled");

    let recalled = backend
        .recall(MemoryRecallRequest {
            session_id: "session-legacy".to_owned(),
            limit: None,
        })
        .await
        .expect("legacy events should remain queryable after migration");
    assert_eq!(recalled.len(), 2);
    assert_eq!(recalled[0].sequence, 1);
    assert_eq!(recalled[1].sequence, 2);
    assert_eq!(
        recalled[0].payload,
        json!({"role":"user","content":"legacy user"})
    );
    assert_eq!(
        recalled[1].payload,
        json!({"role":"assistant","content":"legacy assistant"})
    );

    backend
        .store(MemoryStoreRequest {
            session_id: "session-legacy".to_owned(),
            sequence: 3,
            payload: json!({"role":"user","content":"post-migration continuation"}),
        })
        .await
        .expect("post-migration store should append to existing session");
    let recalled_after_append = backend
        .recall(MemoryRecallRequest {
            session_id: "session-legacy".to_owned(),
            limit: None,
        })
        .await
        .expect("post-migration recall should still succeed");
    assert_eq!(recalled_after_append.len(), 3);
    assert_eq!(recalled_after_append[2].sequence, 3);

    let conn = backend.connect().expect("backend should connect");
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
    for table in ["files", "chunks", "chunks_vec", "chunks_fts"] {
        assert!(
            schema_exists(&conn, "table", table)
                .await
                .expect("schema check should work"),
            "missing table `{table}`"
        );
    }

    let mut rows = conn
        .query(
            "SELECT json_extract(state_json, '$.legacy_key'), json_extract(state_json, '$.last_sequence')
             FROM session_state WHERE session_id = ?1",
            params!["session-legacy"],
        )
        .await
        .expect("state query should run");
    let row = rows
        .next()
        .await
        .expect("state row should be readable")
        .expect("state row should exist");
    let legacy_key = row
        .get::<String>(0)
        .expect("legacy key should deserialize as string");
    let last_sequence = row
        .get::<i64>(1)
        .expect("last_sequence should deserialize as integer");
    assert_eq!(legacy_key, "preserve");
    assert_eq!(last_sequence, 3);

    drop(conn);
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
async fn hybrid_query_merges_vector_and_fts_with_weighted_rank() {
    let db_path = temp_db_path("hybrid-query-merge");
    let config = local_memory_config(&db_path);
    let backend = LibsqlMemory::from_config(&config)
        .await
        .expect("local memory should initialize")
        .expect("memory should be enabled");
    let conn = backend.connect().expect("backend should connect");
    insert_test_session(&conn, "session-hybrid").await;
    insert_chunk_with_embedding(
        &conn,
        "chunk-a",
        "session-hybrid",
        1,
        "alpha vector anchor",
        r#"{"tag":"vector-only"}"#,
        &[1.0, 0.0],
    )
    .await;
    insert_chunk_with_embedding(
        &conn,
        "chunk-b",
        "session-hybrid",
        2,
        "banana keyword",
        r#"{"tag":"hybrid-top"}"#,
        &[0.0, 1.0],
    )
    .await;
    insert_chunk_with_embedding(
        &conn,
        "chunk-c",
        "session-hybrid",
        3,
        "banana keyword",
        r#"{"tag":"recency-tie-break"}"#,
        &[-1.0, 0.0],
    )
    .await;
    drop(conn);

    let results = MemoryRetrieval::hybrid_query(
        &backend,
        MemoryHybridQueryRequest {
            session_id: "session-hybrid".to_owned(),
            query: "banana".to_owned(),
            query_embedding: Some(vec![1.0, 0.0]),
            top_k: Some(3),
            vector_weight: Some(0.5),
            fts_weight: Some(0.5),
        },
    )
    .await
    .expect("hybrid query should merge vector and fts candidates");

    assert_eq!(results.len(), 3);
    assert_eq!(results[0].chunk_id, "chunk-b");
    assert_eq!(results[1].chunk_id, "chunk-c");
    assert_eq!(results[2].chunk_id, "chunk-a");
    assert_approx_eq(results[0].score, 0.75);
    assert_approx_eq(results[0].vector_score, 0.5);
    assert_approx_eq(results[0].fts_score, 1.0);
    assert_approx_eq(results[1].score, results[2].score);
    assert_eq!(results[1].sequence_end, Some(3));
    assert_eq!(
        results[0]
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get("tag"))
            .and_then(serde_json::Value::as_str),
        Some("hybrid-top")
    );

    let _ = fs::remove_file(db_path);
}

#[tokio::test]
async fn hybrid_query_breaks_ties_by_recency_then_chunk_id() {
    let db_path = temp_db_path("hybrid-query-tie-break");
    let config = local_memory_config(&db_path);
    let backend = LibsqlMemory::from_config(&config)
        .await
        .expect("local memory should initialize")
        .expect("memory should be enabled");
    let conn = backend.connect().expect("backend should connect");
    insert_test_session(&conn, "session-hybrid-tie").await;
    insert_chunk_with_embedding(
        &conn,
        "chunk-a",
        "session-hybrid-tie",
        5,
        "vector candidate a",
        r#"{"tag":"a"}"#,
        &[1.0, 0.0],
    )
    .await;
    insert_chunk_with_embedding(
        &conn,
        "chunk-b",
        "session-hybrid-tie",
        5,
        "vector candidate b",
        r#"{"tag":"b"}"#,
        &[1.0, 0.0],
    )
    .await;
    insert_chunk_with_embedding(
        &conn,
        "chunk-c",
        "session-hybrid-tie",
        3,
        "vector candidate c",
        r#"{"tag":"c"}"#,
        &[1.0, 0.0],
    )
    .await;
    drop(conn);

    let results = MemoryRetrieval::hybrid_query(
        &backend,
        MemoryHybridQueryRequest {
            session_id: "session-hybrid-tie".to_owned(),
            query: "unmatched-token".to_owned(),
            query_embedding: Some(vec![1.0, 0.0]),
            top_k: Some(3),
            vector_weight: Some(1.0),
            fts_weight: Some(0.0),
        },
    )
    .await
    .expect("vector-only hybrid query should succeed");

    assert_eq!(results.len(), 3);
    assert_eq!(results[0].chunk_id, "chunk-a");
    assert_eq!(results[1].chunk_id, "chunk-b");
    assert_eq!(results[2].chunk_id, "chunk-c");
    assert_eq!(results[0].sequence_end, Some(5));
    assert_eq!(results[1].sequence_end, Some(5));
    assert_eq!(results[2].sequence_end, Some(3));

    let _ = fs::remove_file(db_path);
}

#[tokio::test]
async fn summary_state_write_uses_epoch_compare_and_swap() {
    let db_path = temp_db_path("summary-state-cas");
    let config = local_memory_config(&db_path);
    let backend = LibsqlMemory::from_config(&config)
        .await
        .expect("local memory should initialize")
        .expect("memory should be enabled");

    backend
        .store(MemoryStoreRequest {
            session_id: "session-summary-cas".to_owned(),
            sequence: 1,
            payload: json!({"role":"user","content":"seed"}),
        })
        .await
        .expect("seed message should store");

    let first = MemoryRetrieval::write_summary_state(
        &backend,
        MemorySummaryWriteRequest {
            session_id: "session-summary-cas".to_owned(),
            expected_epoch: 0,
            next_epoch: 1,
            upper_sequence: 1,
            summary: "initial summary".to_owned(),
            metadata: Some(json!({"kind":"rolling"})),
        },
    )
    .await
    .expect("first summary write should apply");
    assert!(first.applied);
    assert_eq!(first.current_epoch, 1);

    let stale = MemoryRetrieval::write_summary_state(
        &backend,
        MemorySummaryWriteRequest {
            session_id: "session-summary-cas".to_owned(),
            expected_epoch: 0,
            next_epoch: 2,
            upper_sequence: 1,
            summary: "stale summary".to_owned(),
            metadata: None,
        },
    )
    .await
    .expect("stale summary write should return compare-and-swap miss");
    assert!(!stale.applied);
    assert_eq!(stale.current_epoch, 1);

    backend
        .store(MemoryStoreRequest {
            session_id: "session-summary-cas".to_owned(),
            sequence: 2,
            payload: json!({"role":"assistant","content":"follow up"}),
        })
        .await
        .expect("second event should store");

    let second = MemoryRetrieval::write_summary_state(
        &backend,
        MemorySummaryWriteRequest {
            session_id: "session-summary-cas".to_owned(),
            expected_epoch: 1,
            next_epoch: 2,
            upper_sequence: 2,
            summary: "updated summary".to_owned(),
            metadata: None,
        },
    )
    .await
    .expect("second summary write should apply with current epoch");
    assert!(second.applied);
    assert_eq!(second.current_epoch, 2);

    let state = MemoryRetrieval::read_summary_state(
        &backend,
        MemorySummaryReadRequest {
            session_id: "session-summary-cas".to_owned(),
        },
    )
    .await
    .expect("summary read should succeed")
    .expect("summary state should exist");
    assert_eq!(state.epoch, 2);
    assert_eq!(state.upper_sequence, 2);
    assert_eq!(state.summary, "updated summary");
    assert_eq!(state.metadata, None);

    let _ = fs::remove_file(db_path);
}

#[tokio::test]
async fn store_preserves_existing_summary_state_in_session_state() {
    let db_path = temp_db_path("summary-state-preserve");
    let config = local_memory_config(&db_path);
    let backend = LibsqlMemory::from_config(&config)
        .await
        .expect("local memory should initialize")
        .expect("memory should be enabled");

    backend
        .store(MemoryStoreRequest {
            session_id: "session-summary-preserve".to_owned(),
            sequence: 1,
            payload: json!({"role":"user","content":"seed"}),
        })
        .await
        .expect("seed message should store");
    MemoryRetrieval::write_summary_state(
        &backend,
        MemorySummaryWriteRequest {
            session_id: "session-summary-preserve".to_owned(),
            expected_epoch: 0,
            next_epoch: 1,
            upper_sequence: 1,
            summary: "persisted summary".to_owned(),
            metadata: Some(json!({"source":"test"})),
        },
    )
    .await
    .expect("summary write should apply");

    backend
        .store(MemoryStoreRequest {
            session_id: "session-summary-preserve".to_owned(),
            sequence: 2,
            payload: json!({"role":"assistant","content":"follow up"}),
        })
        .await
        .expect("second event should update last_sequence");

    let summary_state = MemoryRetrieval::read_summary_state(
        &backend,
        MemorySummaryReadRequest {
            session_id: "session-summary-preserve".to_owned(),
        },
    )
    .await
    .expect("summary read should succeed")
    .expect("summary state should still exist");
    assert_eq!(summary_state.epoch, 1);
    assert_eq!(summary_state.upper_sequence, 1);
    assert_eq!(summary_state.summary, "persisted summary");

    let conn = backend.connect().expect("backend should connect");
    let mut rows = conn
        .query(
            "SELECT json_extract(state_json, '$.last_sequence')
             FROM session_state WHERE session_id = ?1",
            params!["session-summary-preserve"],
        )
        .await
        .expect("state query should run");
    let row = rows
        .next()
        .await
        .expect("state row should read")
        .expect("state row should exist");
    let last_sequence = row
        .get::<i64>(0)
        .expect("last_sequence should deserialize as integer");
    assert_eq!(last_sequence, 2);

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

async fn insert_test_session(conn: &libsql::Connection, session_id: &str) {
    conn.execute(
        "INSERT INTO sessions (session_id, agent_identity, created_at, updated_at)
         VALUES (?1, NULL, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)",
        params![session_id],
    )
    .await
    .expect("test session should insert");
}

async fn insert_chunk_with_embedding(
    conn: &libsql::Connection,
    chunk_id: &str,
    session_id: &str,
    sequence: i64,
    chunk_text: &str,
    metadata_json: &str,
    embedding: &[f32],
) {
    conn.execute(
        "INSERT INTO chunks (
            chunk_id, session_id, sequence_start, sequence_end, chunk_text, metadata_json, content_hash
         ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7
         )",
        params![
            chunk_id,
            session_id,
            sequence,
            sequence,
            chunk_text,
            metadata_json,
            format!("{chunk_id}-hash")
        ],
    )
    .await
    .expect("test chunk should insert");

    let embedding_blob = encode_embedding_blob(embedding);
    conn.execute(
        "INSERT INTO chunks_vec (chunk_id, embedding_blob, embedding_model)
         VALUES (?1, ?2, ?3)",
        params![chunk_id, embedding_blob.as_slice(), "test-embedding-model"],
    )
    .await
    .expect("test chunk vector should insert");
}

fn encode_embedding_blob(embedding: &[f32]) -> Vec<u8> {
    let mut blob = Vec::with_capacity(std::mem::size_of_val(embedding));
    for value in embedding {
        blob.extend_from_slice(&value.to_le_bytes());
    }
    blob
}

fn assert_approx_eq(left: f64, right: f64) {
    assert!(
        (left - right).abs() <= 1e-6,
        "expected {left} ~= {right} within 1e-6"
    );
}
