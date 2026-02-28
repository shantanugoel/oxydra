use std::{
    env, fs,
    time::{SystemTime, UNIX_EPOCH},
};

use libsql::{Builder, params};
use serde_json::json;
use types::{
    Memory, MemoryConfig, MemoryEmbeddingBackend, MemoryError, MemoryForgetRequest,
    MemoryHybridQueryRequest, MemoryNoteStoreRequest, MemoryRecallRequest, MemoryRetrieval,
    MemoryScratchpadClearRequest, MemoryScratchpadReadRequest, MemoryScratchpadWriteRequest,
    MemoryStoreRequest, MemorySummaryReadRequest, MemorySummaryWriteRequest, Message, MessageRole,
    Model2vecModel,
};

use crate::{
    LibsqlMemory,
    connection::ConnectionStrategy,
    schema::{
        CHUNKS_VECTOR_INDEX_NAME, MIGRATIONS, REQUIRED_INDEXES, REQUIRED_TABLES, REQUIRED_TRIGGERS,
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
        remote_url: Some("libsql://example-org.turso.io".to_owned()),
        auth_token: None,
        embedding_backend: MemoryEmbeddingBackend::Model2vec,
        model2vec_model: Model2vecModel::Potion32m,
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

    let backend = LibsqlMemory::new_local(db_path.to_string_lossy())
        .await
        .expect("local memory should initialize");
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
    let backend = local_memory_backend(&db_path).await;
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

    let reopened = LibsqlMemory::new_local(&db_path)
        .await
        .expect("reopen should succeed");
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
    let backend = local_memory_backend(&db_path).await;
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
    let backend = local_memory_backend(&db_path).await;

    let payload = serde_json::to_value(Message {
        role: MessageRole::Assistant,
        content: Some(
            "hybrid indexing pipeline should normalize and chunk this message body".to_owned(),
        ),
        tool_calls: vec![],
        tool_call_id: None,
        attachments: Vec::new(),
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
    let backend = local_memory_backend(&db_path).await;

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
    let backend = local_memory_backend(&db_path).await;

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

    let backend = LibsqlMemory::new_local(&db_path)
        .await
        .expect("runtime migration pass should succeed");
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

    let backend = LibsqlMemory::new_local(&db_path)
        .await
        .expect("hybrid-schema migration pass should succeed");

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
    let backend = local_memory_backend(&db_path).await;

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
async fn vector_top_k_retrieval_smoke_uses_native_index() {
    let db_path = temp_db_path("vector-top-k-smoke");
    let backend = local_memory_backend(&db_path).await;
    let conn = backend.connect().expect("backend should connect");
    insert_test_session(&conn, "session-vector-smoke").await;
    insert_chunk_with_embedding(
        &conn,
        "chunk-a",
        "session-vector-smoke",
        1,
        "anchor",
        r#"{"tag":"a"}"#,
        &[1.0, 0.0],
    )
    .await;
    insert_chunk_with_embedding(
        &conn,
        "chunk-b",
        "session-vector-smoke",
        2,
        "other",
        r#"{"tag":"b"}"#,
        &[0.0, 1.0],
    )
    .await;

    let mut query_embedding_values = vec![1.0_f32, 0.0_f32];
    query_embedding_values.resize(512, 0.0);
    let query_embedding =
        serde_json::to_string(&query_embedding_values).expect("query embedding should serialize");
    let sql = format!(
        "SELECT c.chunk_id, vector_distance_cos(cv.embedding_blob, vector32(?1)) AS distance
         FROM vector_top_k('{CHUNKS_VECTOR_INDEX_NAME}', vector32(?1), 2) AS v
         INNER JOIN chunks_vec cv ON cv.rowid = v.id
         INNER JOIN chunks c ON c.chunk_id = cv.chunk_id
         WHERE c.session_id = ?2
         ORDER BY distance ASC, c.chunk_id ASC"
    );
    let mut rows = conn
        .query(
            sql.as_str(),
            params![query_embedding.as_str(), "session-vector-smoke"],
        )
        .await
        .expect("vector_top_k query should succeed");
    let first_row = rows
        .next()
        .await
        .expect("first row should be readable")
        .expect("first row should exist");
    let first_chunk_id = first_row
        .get::<String>(0)
        .expect("chunk id should be readable");
    assert_eq!(first_chunk_id, "chunk-a");

    drop(conn);
    let _ = fs::remove_file(db_path);
}

#[tokio::test]
async fn hybrid_query_fails_fast_when_native_vector_index_is_missing() {
    let db_path = temp_db_path("hybrid-query-missing-vector-index");
    let backend = local_memory_backend(&db_path).await;
    let conn = backend.connect().expect("backend should connect");
    insert_test_session(&conn, "session-vector-missing-index").await;
    insert_chunk_with_embedding(
        &conn,
        "chunk-a",
        "session-vector-missing-index",
        1,
        "anchor",
        r#"{"tag":"a"}"#,
        &[1.0, 0.0],
    )
    .await;

    let drop_index_sql = format!("DROP INDEX {CHUNKS_VECTOR_INDEX_NAME}");
    conn.execute(drop_index_sql.as_str(), params![])
        .await
        .expect("vector index should drop");
    drop(conn);

    let error = MemoryRetrieval::hybrid_query(
        &backend,
        MemoryHybridQueryRequest {
            session_id: "session-vector-missing-index".to_owned(),
            query: "anchor".to_owned(),
            tags: None,
            query_embedding: Some(vec![1.0, 0.0]),
            top_k: Some(1),
            vector_weight: Some(1.0),
            fts_weight: Some(0.0),
        },
    )
    .await
    .expect_err("hybrid query should fail without native vector index");
    match error {
        MemoryError::Query { message } => {
            assert!(
                message.contains("native libsql vector query failed"),
                "error should mention native vector query failure: {message}"
            );
        }
        other => panic!("expected MemoryError::Query, got {other:?}"),
    }

    let _ = fs::remove_file(db_path);
}

#[tokio::test]
async fn hybrid_query_merges_vector_and_fts_with_weighted_rank() {
    let db_path = temp_db_path("hybrid-query-merge");
    let backend = local_memory_backend(&db_path).await;
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
            tags: None,
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
    let backend = local_memory_backend(&db_path).await;
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
            tags: None,
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
    let backend = local_memory_backend(&db_path).await;

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
    let backend = local_memory_backend(&db_path).await;

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

/// Create a local `LibsqlMemory` instance for testing.
///
/// Memory crate tests use `new_local` directly — the `db_path` is no longer
/// part of `MemoryConfig` (it's a runner-level convention).
async fn local_memory_backend(db_path: &str) -> LibsqlMemory {
    LibsqlMemory::new_local(db_path)
        .await
        .expect("local memory backend should initialize")
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

    const TEST_VECTOR_DIMENSIONS: usize = 512;
    let mut normalized_embedding = embedding.to_vec();
    assert!(
        normalized_embedding.len() <= TEST_VECTOR_DIMENSIONS,
        "test embedding dimensions must not exceed {TEST_VECTOR_DIMENSIONS}"
    );
    normalized_embedding.resize(TEST_VECTOR_DIMENSIONS, 0.0);
    let embedding_json =
        serde_json::to_string(&normalized_embedding).expect("embedding json should serialize");
    conn.execute(
        "INSERT INTO chunks_vec (chunk_id, embedding_blob, embedding_model)
         VALUES (?1, vector32(?2), ?3)",
        params![chunk_id, embedding_json.as_str(), "test-embedding-model"],
    )
    .await
    .expect("test chunk vector should insert");
}

fn assert_approx_eq(left: f64, right: f64) {
    assert!(
        (left - right).abs() <= 1e-6,
        "expected {left} ~= {right} within 1e-6"
    );
}

fn note_store_request(session_id: &str, note_id: &str, content: &str) -> MemoryNoteStoreRequest {
    MemoryNoteStoreRequest {
        session_id: session_id.to_owned(),
        note_id: note_id.to_owned(),
        content: content.to_owned(),
        source: Some("memory_save".to_owned()),
        tags: None,
    }
}

// ---------------------------------------------------------------------------
// Note lifecycle tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn store_note_creates_chunks_with_note_id_in_metadata() {
    let db_path = temp_db_path("store-note-metadata");
    let backend = local_memory_backend(&db_path).await;
    let mut request = note_store_request("memory:alice", "note-abc123", "User likes chocolate");
    request.tags = Some(vec![
        " Preferences ".to_owned(),
        "PREFERENCES".to_owned(),
        "theme".to_owned(),
    ]);

    MemoryRetrieval::store_note(&backend, request)
        .await
        .expect("store_note should succeed");

    let conn = backend.connect().expect("backend should connect");
    let mut rows = conn
        .query(
            "SELECT chunk_id, metadata_json FROM chunks WHERE session_id = ?1",
            params!["memory:alice"],
        )
        .await
        .expect("chunk query should run");
    let mut found = false;
    while let Some(row) = rows.next().await.expect("chunk row should read") {
        found = true;
        let metadata_json = row
            .get::<String>(1)
            .expect("metadata_json should be readable");
        let metadata: serde_json::Value =
            serde_json::from_str(&metadata_json).expect("metadata should parse");
        assert_eq!(
            metadata.get("note_id").and_then(|v| v.as_str()),
            Some("note-abc123"),
            "chunk metadata should carry note_id"
        );
        assert_eq!(
            metadata.get("source").and_then(|v| v.as_str()),
            Some("memory_save"),
            "chunk metadata should carry source marker"
        );
        assert_eq!(
            metadata.get("schema_version").and_then(|v| v.as_u64()),
            Some(1),
            "chunk metadata should carry schema_version"
        );
        let tags = metadata
            .get("tags")
            .and_then(|value| value.as_array())
            .expect("chunk metadata should carry tags array");
        let normalized_tags: Vec<&str> = tags.iter().filter_map(|value| value.as_str()).collect();
        assert_eq!(normalized_tags, vec!["preferences", "theme"]);
        let created_at = metadata
            .get("created_at")
            .and_then(|v| v.as_str())
            .expect("chunk metadata should carry created_at");
        let updated_at = metadata
            .get("updated_at")
            .and_then(|v| v.as_str())
            .expect("chunk metadata should carry updated_at");
        chrono::DateTime::parse_from_rfc3339(created_at)
            .expect("created_at should be RFC3339 timestamp");
        chrono::DateTime::parse_from_rfc3339(updated_at)
            .expect("updated_at should be RFC3339 timestamp");
    }
    assert!(found, "at least one chunk should have been created");

    drop(conn);
    let _ = fs::remove_file(db_path);
}

#[tokio::test]
async fn store_note_creates_searchable_conversation_event() {
    let db_path = temp_db_path("store-note-event");
    let backend = local_memory_backend(&db_path).await;

    MemoryRetrieval::store_note(
        &backend,
        note_store_request("memory:alice", "note-ev1", "User prefers dark mode"),
    )
    .await
    .expect("store_note should succeed");

    let conn = backend.connect().expect("backend should connect");
    let mut rows = conn
        .query(
            "SELECT payload_json FROM conversation_events WHERE session_id = ?1",
            params!["memory:alice"],
        )
        .await
        .expect("event query should run");
    let row = rows.next().await.expect("row read").expect("row exists");
    let payload_json = row.get::<String>(0).expect("payload readable");
    let payload: serde_json::Value =
        serde_json::from_str(&payload_json).expect("payload should parse");
    assert_eq!(
        payload.get("tool_call_id").and_then(|v| v.as_str()),
        Some("note-ev1")
    );
    assert_eq!(
        payload.get("content").and_then(|v| v.as_str()),
        Some("User prefers dark mode")
    );

    drop(conn);
    let _ = fs::remove_file(db_path);
}

#[tokio::test]
async fn delete_note_removes_chunks_and_event() {
    let db_path = temp_db_path("delete-note");
    let backend = local_memory_backend(&db_path).await;

    MemoryRetrieval::store_note(
        &backend,
        note_store_request("memory:alice", "note-del1", "User likes chocolates"),
    )
    .await
    .expect("store_note should succeed");

    // Verify chunks exist before deletion
    let conn = backend.connect().expect("backend should connect");
    let mut count_rows = conn
        .query(
            "SELECT COUNT(*) FROM chunks WHERE session_id = ?1",
            params!["memory:alice"],
        )
        .await
        .expect("count query");
    let count = count_rows
        .next()
        .await
        .expect("row read")
        .expect("row exists")
        .get::<i64>(0)
        .expect("count readable");
    assert!(count > 0, "chunks should exist before deletion");
    drop(count_rows);

    let found = MemoryRetrieval::delete_note(&backend, "memory:alice", "note-del1")
        .await
        .expect("delete_note should succeed");
    assert!(found, "delete should report note was found");

    // Verify chunks are gone
    let mut count_rows = conn
        .query(
            "SELECT COUNT(*) FROM chunks WHERE session_id = ?1",
            params!["memory:alice"],
        )
        .await
        .expect("count query");
    let count = count_rows
        .next()
        .await
        .expect("row read")
        .expect("row exists")
        .get::<i64>(0)
        .expect("count readable");
    assert_eq!(count, 0, "chunks should be deleted");
    drop(count_rows);

    // Verify event is gone
    let mut event_rows = conn
        .query(
            "SELECT COUNT(*) FROM conversation_events WHERE session_id = ?1",
            params!["memory:alice"],
        )
        .await
        .expect("event count query");
    let event_count = event_rows
        .next()
        .await
        .expect("row read")
        .expect("row exists")
        .get::<i64>(0)
        .expect("count readable");
    assert_eq!(event_count, 0, "conversation event should be deleted");

    drop(conn);
    let _ = fs::remove_file(db_path);
}

#[tokio::test]
async fn delete_note_returns_false_for_nonexistent_note() {
    let db_path = temp_db_path("delete-note-missing");
    let backend = local_memory_backend(&db_path).await;

    let found = MemoryRetrieval::delete_note(&backend, "memory:alice", "note-nonexistent")
        .await
        .expect("delete_note should not fail for missing note");
    assert!(!found, "should return false for non-existent note");

    let _ = fs::remove_file(db_path);
}

#[tokio::test]
async fn note_save_search_update_delete_roundtrip() {
    let db_path = temp_db_path("note-roundtrip");
    let backend = local_memory_backend(&db_path).await;

    // Save
    MemoryRetrieval::store_note(
        &backend,
        note_store_request("memory:bob", "note-rt1", "User's name is Shantanu"),
    )
    .await
    .expect("store_note should succeed");

    // Search - should find the note
    let results = MemoryRetrieval::hybrid_query(
        &backend,
        MemoryHybridQueryRequest {
            session_id: "memory:bob".to_owned(),
            query: "name".to_owned(),
            tags: None,
            query_embedding: None,
            top_k: Some(5),
            vector_weight: Some(0.7),
            fts_weight: Some(0.3),
        },
    )
    .await
    .expect("search should succeed");
    assert!(!results.is_empty(), "search should find the saved note");
    let found_note = results.iter().any(|r| r.text.contains("Shantanu"));
    assert!(found_note, "search should find 'Shantanu'");

    // Update the note
    MemoryRetrieval::delete_note(&backend, "memory:bob", "note-rt1")
        .await
        .expect("delete old note should succeed");
    MemoryRetrieval::store_note(
        &backend,
        note_store_request("memory:bob", "note-rt1", "User prefers to be called SG"),
    )
    .await
    .expect("store updated note should succeed");

    // Search again - should find updated version
    let results = MemoryRetrieval::hybrid_query(
        &backend,
        MemoryHybridQueryRequest {
            session_id: "memory:bob".to_owned(),
            query: "name".to_owned(),
            tags: None,
            query_embedding: None,
            top_k: Some(5),
            vector_weight: Some(0.7),
            fts_weight: Some(0.3),
        },
    )
    .await
    .expect("search after update should succeed");
    let found_updated = results.iter().any(|r| r.text.contains("SG"));
    assert!(found_updated, "search should find updated note with 'SG'");
    let old_still_present = results.iter().any(|r| r.text.contains("Shantanu"));
    assert!(
        !old_still_present,
        "old note content should not appear in search results"
    );

    // Delete
    let found = MemoryRetrieval::delete_note(&backend, "memory:bob", "note-rt1")
        .await
        .expect("delete should succeed");
    assert!(found, "delete should report note was found");

    // Search again - should find nothing
    let results = MemoryRetrieval::hybrid_query(
        &backend,
        MemoryHybridQueryRequest {
            session_id: "memory:bob".to_owned(),
            query: "name".to_owned(),
            tags: None,
            query_embedding: None,
            top_k: Some(5),
            vector_weight: Some(0.7),
            fts_weight: Some(0.3),
        },
    )
    .await
    .expect("search after delete should succeed");
    assert!(
        results.is_empty(),
        "search should return no results after deletion"
    );

    let _ = fs::remove_file(db_path);
}

#[tokio::test]
async fn note_user_scoping_isolates_across_users() {
    let db_path = temp_db_path("note-user-isolation");
    let backend = local_memory_backend(&db_path).await;

    // User A saves a note
    MemoryRetrieval::store_note(
        &backend,
        note_store_request("memory:user-a", "note-ua1", "User A prefers red"),
    )
    .await
    .expect("user A store should succeed");

    // User B saves a note
    MemoryRetrieval::store_note(
        &backend,
        note_store_request("memory:user-b", "note-ub1", "User B prefers blue"),
    )
    .await
    .expect("user B store should succeed");

    // User A's search should only find their own note
    let results_a = MemoryRetrieval::hybrid_query(
        &backend,
        MemoryHybridQueryRequest {
            session_id: "memory:user-a".to_owned(),
            query: "prefers".to_owned(),
            tags: None,
            query_embedding: None,
            top_k: Some(10),
            vector_weight: Some(0.5),
            fts_weight: Some(0.5),
        },
    )
    .await
    .expect("user A search should succeed");
    assert!(!results_a.is_empty(), "user A should have results");
    for result in &results_a {
        assert!(
            !result.text.contains("blue"),
            "user A should not see user B's note"
        );
    }

    // User B's search should only find their own note
    let results_b = MemoryRetrieval::hybrid_query(
        &backend,
        MemoryHybridQueryRequest {
            session_id: "memory:user-b".to_owned(),
            query: "prefers".to_owned(),
            tags: None,
            query_embedding: None,
            top_k: Some(10),
            vector_weight: Some(0.5),
            fts_weight: Some(0.5),
        },
    )
    .await
    .expect("user B search should succeed");
    assert!(!results_b.is_empty(), "user B should have results");
    for result in &results_b {
        assert!(
            !result.text.contains("red"),
            "user B should not see user A's note"
        );
    }

    let _ = fs::remove_file(db_path);
}

#[tokio::test]
async fn store_note_multiple_notes_in_same_session_coexist() {
    let db_path = temp_db_path("note-multiple-coexist");
    let backend = local_memory_backend(&db_path).await;

    MemoryRetrieval::store_note(
        &backend,
        note_store_request("memory:carol", "note-m1", "Favorite color is green"),
    )
    .await
    .expect("first note should store");
    MemoryRetrieval::store_note(
        &backend,
        note_store_request("memory:carol", "note-m2", "Preferred language is Rust"),
    )
    .await
    .expect("second note should store");

    // Delete only one note
    let found = MemoryRetrieval::delete_note(&backend, "memory:carol", "note-m1")
        .await
        .expect("delete should succeed");
    assert!(found);

    // The other note should still be searchable
    let results = MemoryRetrieval::hybrid_query(
        &backend,
        MemoryHybridQueryRequest {
            session_id: "memory:carol".to_owned(),
            query: "Rust".to_owned(),
            tags: None,
            query_embedding: None,
            top_k: Some(5),
            vector_weight: Some(0.5),
            fts_weight: Some(0.5),
        },
    )
    .await
    .expect("search should succeed");
    assert!(
        !results.is_empty(),
        "second note should survive first note's deletion"
    );
    let has_rust = results.iter().any(|r| r.text.contains("Rust"));
    assert!(has_rust, "second note should be found");

    let _ = fs::remove_file(db_path);
}

#[tokio::test]
async fn same_content_notes_stay_independent_on_update_delete_and_search() {
    let db_path = temp_db_path("note-same-content-independence");
    let backend = local_memory_backend(&db_path).await;
    let session_id = "memory:dana";

    MemoryRetrieval::store_note(
        &backend,
        note_store_request(session_id, "note-a", "User likes jazz music"),
    )
    .await
    .expect("first note should store");
    MemoryRetrieval::store_note(
        &backend,
        note_store_request(session_id, "note-b", "User likes jazz music"),
    )
    .await
    .expect("second note should store");

    // Simulate update semantics used by memory tools: delete then store same note_id.
    let removed = MemoryRetrieval::delete_note(&backend, session_id, "note-a")
        .await
        .expect("note-a delete before update should succeed");
    assert!(removed, "note-a should exist before update");
    MemoryRetrieval::store_note(
        &backend,
        note_store_request(session_id, "note-a", "User likes rock music"),
    )
    .await
    .expect("updated note-a should store");

    let jazz_results = MemoryRetrieval::hybrid_query(
        &backend,
        MemoryHybridQueryRequest {
            session_id: session_id.to_owned(),
            query: "jazz music".to_owned(),
            tags: None,
            query_embedding: None,
            top_k: Some(1),
            vector_weight: Some(0.0),
            fts_weight: Some(1.0),
        },
    )
    .await
    .expect("jazz search should succeed");
    let jazz_note_ids: Vec<&str> = jazz_results
        .iter()
        .filter_map(|result| result.metadata.as_ref())
        .filter_map(|metadata| metadata.get("note_id"))
        .filter_map(|value| value.as_str())
        .collect();
    assert_eq!(
        jazz_note_ids.first().copied(),
        Some("note-b"),
        "jazz search should rank note-b first after note-a update"
    );
    assert!(
        jazz_note_ids.contains(&"note-b"),
        "jazz search should still return note-b"
    );

    let removed_again = MemoryRetrieval::delete_note(&backend, session_id, "note-a")
        .await
        .expect("note-a delete should succeed");
    assert!(removed_again, "updated note-a should be deletable");

    let conn = backend.connect().expect("backend should connect");
    let mut rows = conn
        .query(
            "SELECT
                 COALESCE(SUM(CASE WHEN json_extract(metadata_json, '$.note_id') = 'note-a' THEN 1 ELSE 0 END), 0) AS note_a_chunks,
                 COALESCE(SUM(CASE WHEN json_extract(metadata_json, '$.note_id') = 'note-b' THEN 1 ELSE 0 END), 0) AS note_b_chunks
             FROM chunks
             WHERE session_id = ?1",
            params![session_id],
        )
        .await
        .expect("chunk counts query should run");
    let row = rows.next().await.expect("row read").expect("row exists");
    let note_a_chunks = row.get::<i64>(0).expect("note-a count should be readable");
    let note_b_chunks = row.get::<i64>(1).expect("note-b count should be readable");
    assert_eq!(note_a_chunks, 0, "note-a chunks should be fully removed");
    assert!(note_b_chunks > 0, "note-b chunks should remain present");

    let jazz_results_after_delete = MemoryRetrieval::hybrid_query(
        &backend,
        MemoryHybridQueryRequest {
            session_id: session_id.to_owned(),
            query: "jazz".to_owned(),
            tags: None,
            query_embedding: None,
            top_k: Some(10),
            vector_weight: Some(0.0),
            fts_weight: Some(1.0),
        },
    )
    .await
    .expect("jazz search after delete should succeed");
    let jazz_note_ids_after_delete: Vec<&str> = jazz_results_after_delete
        .iter()
        .filter_map(|result| result.metadata.as_ref())
        .filter_map(|metadata| metadata.get("note_id"))
        .filter_map(|value| value.as_str())
        .collect();
    assert!(
        jazz_note_ids_after_delete.contains(&"note-b"),
        "note-b should remain searchable after deleting note-a"
    );

    let _ = fs::remove_file(db_path);
}

#[tokio::test]
async fn hybrid_query_tag_filter_requires_all_requested_tags() {
    let db_path = temp_db_path("hybrid-query-tag-filter");
    let backend = local_memory_backend(&db_path).await;

    let mut first = note_store_request("memory:tags", "note-tags-a", "deploy checklist");
    first.tags = Some(vec!["project-a".to_owned(), "workflow".to_owned()]);
    MemoryRetrieval::store_note(&backend, first)
        .await
        .expect("first tagged note should store");

    let mut second = note_store_request("memory:tags", "note-tags-b", "deploy checklist");
    second.tags = Some(vec!["project-b".to_owned()]);
    MemoryRetrieval::store_note(&backend, second)
        .await
        .expect("second tagged note should store");

    let filtered = MemoryRetrieval::hybrid_query(
        &backend,
        MemoryHybridQueryRequest {
            session_id: "memory:tags".to_owned(),
            query: "deploy checklist".to_owned(),
            tags: Some(vec!["project-a".to_owned(), "workflow".to_owned()]),
            query_embedding: None,
            top_k: Some(10),
            vector_weight: Some(0.0),
            fts_weight: Some(1.0),
        },
    )
    .await
    .expect("tag filtered search should succeed");
    assert_eq!(
        filtered.len(),
        1,
        "only fully matching tags should be returned"
    );
    let note_id = filtered[0]
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get("note_id"))
        .and_then(|value| value.as_str());
    assert_eq!(note_id, Some("note-tags-a"));

    let no_match = MemoryRetrieval::hybrid_query(
        &backend,
        MemoryHybridQueryRequest {
            session_id: "memory:tags".to_owned(),
            query: "deploy checklist".to_owned(),
            tags: Some(vec!["workflow".to_owned(), "project-b".to_owned()]),
            query_embedding: None,
            top_k: Some(10),
            vector_weight: Some(0.0),
            fts_weight: Some(1.0),
        },
    )
    .await
    .expect("non-matching tag search should succeed");
    assert!(
        no_match.is_empty(),
        "results should be empty when not all requested tags are present"
    );

    let _ = fs::remove_file(db_path);
}

#[tokio::test]
async fn scratchpad_state_roundtrip_is_session_scoped() {
    let db_path = temp_db_path("scratchpad-roundtrip");
    let backend = local_memory_backend(&db_path).await;
    let session_id = "runtime:session-1";

    let write_result = MemoryRetrieval::write_scratchpad(
        &backend,
        MemoryScratchpadWriteRequest {
            session_id: session_id.to_owned(),
            items: vec![
                "Inspect failing test".to_owned(),
                "Apply minimal patch".to_owned(),
            ],
        },
    )
    .await
    .expect("scratchpad write should succeed");
    assert_eq!(write_result.item_count, 2);

    let scratchpad = MemoryRetrieval::read_scratchpad(
        &backend,
        MemoryScratchpadReadRequest {
            session_id: session_id.to_owned(),
        },
    )
    .await
    .expect("scratchpad read should succeed")
    .expect("scratchpad should exist");
    assert_eq!(
        scratchpad.items,
        vec![
            "Inspect failing test".to_owned(),
            "Apply minimal patch".to_owned()
        ]
    );
    chrono::DateTime::parse_from_rfc3339(&scratchpad.updated_at)
        .expect("scratchpad updated_at should be RFC3339");

    let other_session = MemoryRetrieval::read_scratchpad(
        &backend,
        MemoryScratchpadReadRequest {
            session_id: "runtime:session-2".to_owned(),
        },
    )
    .await
    .expect("scratchpad read for other session should succeed");
    assert!(
        other_session.is_none(),
        "scratchpad must be isolated per session"
    );

    let cleared = MemoryRetrieval::clear_scratchpad(
        &backend,
        MemoryScratchpadClearRequest {
            session_id: session_id.to_owned(),
        },
    )
    .await
    .expect("scratchpad clear should succeed");
    assert!(cleared, "clear should report scratchpad existed");

    let after_clear = MemoryRetrieval::read_scratchpad(
        &backend,
        MemoryScratchpadReadRequest {
            session_id: session_id.to_owned(),
        },
    )
    .await
    .expect("scratchpad read after clear should succeed");
    assert!(
        after_clear.is_none(),
        "scratchpad should be removed after clear"
    );

    let _ = fs::remove_file(db_path);
}

#[tokio::test]
async fn scratchpad_write_enforces_limits() {
    let db_path = temp_db_path("scratchpad-limits");
    let backend = local_memory_backend(&db_path).await;

    let too_many_items = MemoryRetrieval::write_scratchpad(
        &backend,
        MemoryScratchpadWriteRequest {
            session_id: "runtime:limits".to_owned(),
            items: (0..33).map(|index| format!("item-{index}")).collect(),
        },
    )
    .await
    .expect_err("write should fail when item count exceeds limit");
    assert!(matches!(
        too_many_items,
        MemoryError::Query { message } if message.contains("exceeds max")
    ));

    let oversized_payload = MemoryRetrieval::write_scratchpad(
        &backend,
        MemoryScratchpadWriteRequest {
            session_id: "runtime:limits".to_owned(),
            items: vec!["é".repeat(240); 32],
        },
    )
    .await
    .expect_err("write should fail when payload exceeds byte limit");
    assert!(matches!(
        oversized_payload,
        MemoryError::Query { message } if message.contains("payload size")
    ));

    let _ = fs::remove_file(db_path);
}

// ── Scheduler store tests ──────────────────────────────────────────────

#[tokio::test]
async fn scheduler_store_create_get_roundtrip() {
    use crate::scheduler_store::{LibsqlSchedulerStore, SchedulerStore};
    use types::{NotificationPolicy, ScheduleCadence, ScheduleDefinition, ScheduleStatus};

    let db_path = temp_db_path("sched-create-get");
    let backend = local_memory_backend(&db_path).await;
    let conn = backend
        .connect_for_scheduler()
        .await
        .expect("scheduler conn");
    let store = LibsqlSchedulerStore::new(conn);

    let now = chrono::Utc::now().to_rfc3339();
    let def = ScheduleDefinition {
        schedule_id: "sched-001".to_owned(),
        user_id: "alice".to_owned(),
        name: Some("Daily check".to_owned()),
        goal: "Check the weather".to_owned(),
        cadence: ScheduleCadence::Cron {
            expression: "0 8 * * *".to_owned(),
            timezone: "Asia/Kolkata".to_owned(),
        },
        notification_policy: NotificationPolicy::Always,
        status: ScheduleStatus::Active,
        created_at: now.clone(),
        updated_at: now.clone(),
        next_run_at: Some(now.clone()),
        last_run_at: None,
        last_run_status: None,
        consecutive_failures: 0,
        channel_id: None,
        channel_context_id: None,
    };
    store
        .create_schedule(&def)
        .await
        .expect("create should succeed");

    let fetched = store
        .get_schedule("sched-001")
        .await
        .expect("get should succeed");
    assert_eq!(fetched.schedule_id, "sched-001");
    assert_eq!(fetched.user_id, "alice");
    assert_eq!(fetched.name.as_deref(), Some("Daily check"));
    assert_eq!(fetched.goal, "Check the weather");
    assert_eq!(fetched.status, ScheduleStatus::Active);
    assert_eq!(fetched.notification_policy, NotificationPolicy::Always);

    let _ = fs::remove_file(db_path);
}

#[tokio::test]
async fn scheduler_store_count_and_limit() {
    use crate::scheduler_store::{LibsqlSchedulerStore, SchedulerStore};
    use types::{NotificationPolicy, ScheduleCadence, ScheduleDefinition, ScheduleStatus};

    let db_path = temp_db_path("sched-count");
    let backend = local_memory_backend(&db_path).await;
    let conn = backend
        .connect_for_scheduler()
        .await
        .expect("scheduler conn");
    let store = LibsqlSchedulerStore::new(conn);

    let now = chrono::Utc::now().to_rfc3339();
    for i in 0..3 {
        let def = ScheduleDefinition {
            schedule_id: format!("sched-{i}"),
            user_id: "bob".to_owned(),
            name: None,
            goal: format!("Task {i}"),
            cadence: ScheduleCadence::Interval { every_secs: 3600 },
            notification_policy: NotificationPolicy::Never,
            status: ScheduleStatus::Active,
            created_at: now.clone(),
            updated_at: now.clone(),
            next_run_at: Some(now.clone()),
            last_run_at: None,
            last_run_status: None,
            consecutive_failures: 0,
            channel_id: None,
            channel_context_id: None,
        };
        store.create_schedule(&def).await.expect("create");
    }

    let count = store.count_schedules("bob").await.expect("count");
    assert_eq!(count, 3);

    let count_other = store.count_schedules("charlie").await.expect("count other");
    assert_eq!(count_other, 0);

    let _ = fs::remove_file(db_path);
}

#[tokio::test]
async fn scheduler_store_search_with_filters() {
    use crate::scheduler_store::{LibsqlSchedulerStore, SchedulerStore};
    use types::{
        NotificationPolicy, ScheduleCadence, ScheduleDefinition, ScheduleSearchFilters,
        ScheduleStatus,
    };

    let db_path = temp_db_path("sched-search");
    let backend = local_memory_backend(&db_path).await;
    let conn = backend
        .connect_for_scheduler()
        .await
        .expect("scheduler conn");
    let store = LibsqlSchedulerStore::new(conn);

    let now = chrono::Utc::now().to_rfc3339();
    let schedules = vec![
        ScheduleDefinition {
            schedule_id: "s1".to_owned(),
            user_id: "alice".to_owned(),
            name: Some("Weather check".to_owned()),
            goal: "Check weather".to_owned(),
            cadence: ScheduleCadence::Cron {
                expression: "0 8 * * *".to_owned(),
                timezone: "Asia/Kolkata".to_owned(),
            },
            notification_policy: NotificationPolicy::Always,
            status: ScheduleStatus::Active,
            created_at: now.clone(),
            updated_at: now.clone(),
            next_run_at: Some(now.clone()),
            last_run_at: None,
            last_run_status: None,
            consecutive_failures: 0,
            channel_id: None,
            channel_context_id: None,
        },
        ScheduleDefinition {
            schedule_id: "s2".to_owned(),
            user_id: "alice".to_owned(),
            name: Some("Backup".to_owned()),
            goal: "Backup files".to_owned(),
            cadence: ScheduleCadence::Interval { every_secs: 86400 },
            notification_policy: NotificationPolicy::Never,
            status: ScheduleStatus::Paused,
            created_at: now.clone(),
            updated_at: now.clone(),
            next_run_at: None,
            last_run_at: None,
            last_run_status: None,
            consecutive_failures: 0,
            channel_id: None,
            channel_context_id: None,
        },
    ];
    for s in &schedules {
        store.create_schedule(s).await.expect("create");
    }

    // Search all
    let all = store
        .search_schedules(
            "alice",
            &ScheduleSearchFilters {
                limit: 20,
                ..Default::default()
            },
        )
        .await
        .expect("search all");
    assert_eq!(all.total_count, 2);

    // Filter by status
    let active = store
        .search_schedules(
            "alice",
            &ScheduleSearchFilters {
                status: Some(ScheduleStatus::Active),
                limit: 20,
                ..Default::default()
            },
        )
        .await
        .expect("search active");
    assert_eq!(active.total_count, 1);
    assert_eq!(active.schedules[0].schedule_id, "s1");

    // Filter by name
    let weather = store
        .search_schedules(
            "alice",
            &ScheduleSearchFilters {
                name_contains: Some("Weather".to_owned()),
                limit: 20,
                ..Default::default()
            },
        )
        .await
        .expect("search name");
    assert_eq!(weather.total_count, 1);

    let _ = fs::remove_file(db_path);
}

#[tokio::test]
async fn scheduler_store_delete_cascades_runs() {
    use crate::scheduler_store::{LibsqlSchedulerStore, SchedulerStore};
    use types::{
        NotificationPolicy, ScheduleCadence, ScheduleDefinition, ScheduleRunRecord,
        ScheduleRunStatus, ScheduleStatus,
    };

    let db_path = temp_db_path("sched-delete");
    let backend = local_memory_backend(&db_path).await;
    let conn = backend
        .connect_for_scheduler()
        .await
        .expect("scheduler conn");
    let store = LibsqlSchedulerStore::new(conn);

    let now = chrono::Utc::now().to_rfc3339();
    let def = ScheduleDefinition {
        schedule_id: "sched-del".to_owned(),
        user_id: "alice".to_owned(),
        name: None,
        goal: "Task".to_owned(),
        cadence: ScheduleCadence::Interval { every_secs: 3600 },
        notification_policy: NotificationPolicy::Always,
        status: ScheduleStatus::Active,
        created_at: now.clone(),
        updated_at: now.clone(),
        next_run_at: Some(now.clone()),
        last_run_at: None,
        last_run_status: None,
        consecutive_failures: 0,
        channel_id: None,
        channel_context_id: None,
    };
    store.create_schedule(&def).await.expect("create");

    let run = ScheduleRunRecord {
        run_id: "run-1".to_owned(),
        schedule_id: "sched-del".to_owned(),
        started_at: now.clone(),
        finished_at: now.clone(),
        status: ScheduleRunStatus::Success,
        output_summary: Some("Done".to_owned()),
        turn_count: 1,
        cost: 0.01,
        notified: true,
        output: Some("Done".to_owned()),
    };
    store
        .record_run_and_reschedule("sched-del", &run, Some(now.clone()), None)
        .await
        .expect("record run");

    let history = store
        .get_run_history("sched-del", 10)
        .await
        .expect("history");
    assert_eq!(history.len(), 1);

    let deleted = store.delete_schedule("sched-del").await.expect("delete");
    assert!(deleted);

    // Verify schedule is gone
    let result = store.get_schedule("sched-del").await;
    assert!(result.is_err());

    // Verify runs are also gone (cascading delete)
    let history = store
        .get_run_history("sched-del", 10)
        .await
        .expect("history after delete");
    assert!(history.is_empty());

    let _ = fs::remove_file(db_path);
}

#[tokio::test]
async fn scheduler_store_update_pause_resume() {
    use crate::scheduler_store::{LibsqlSchedulerStore, SchedulerStore};
    use types::{
        NotificationPolicy, ScheduleCadence, ScheduleDefinition, SchedulePatch, ScheduleStatus,
    };

    let db_path = temp_db_path("sched-pause");
    let backend = local_memory_backend(&db_path).await;
    let conn = backend
        .connect_for_scheduler()
        .await
        .expect("scheduler conn");
    let store = LibsqlSchedulerStore::new(conn);

    let now = chrono::Utc::now().to_rfc3339();
    let def = ScheduleDefinition {
        schedule_id: "sched-pr".to_owned(),
        user_id: "alice".to_owned(),
        name: Some("Test".to_owned()),
        goal: "Do stuff".to_owned(),
        cadence: ScheduleCadence::Interval { every_secs: 3600 },
        notification_policy: NotificationPolicy::Always,
        status: ScheduleStatus::Active,
        created_at: now.clone(),
        updated_at: now.clone(),
        next_run_at: Some(now.clone()),
        last_run_at: None,
        last_run_status: None,
        consecutive_failures: 0,
        channel_id: None,
        channel_context_id: None,
    };
    store.create_schedule(&def).await.expect("create");

    // Pause
    let paused = store
        .update_schedule(
            "sched-pr",
            &SchedulePatch {
                status: Some(ScheduleStatus::Paused),
                next_run_at: Some(None),
                ..Default::default()
            },
        )
        .await
        .expect("pause");
    assert_eq!(paused.status, ScheduleStatus::Paused);
    assert!(paused.next_run_at.is_none());

    // Resume
    let resumed = store
        .update_schedule(
            "sched-pr",
            &SchedulePatch {
                status: Some(ScheduleStatus::Active),
                next_run_at: Some(Some(now.clone())),
                ..Default::default()
            },
        )
        .await
        .expect("resume");
    assert_eq!(resumed.status, ScheduleStatus::Active);
    assert!(resumed.next_run_at.is_some());

    let _ = fs::remove_file(db_path);
}

#[tokio::test]
async fn scheduler_store_due_schedules_filters_correctly() {
    use crate::scheduler_store::{LibsqlSchedulerStore, SchedulerStore};
    use types::{NotificationPolicy, ScheduleCadence, ScheduleDefinition, ScheduleStatus};

    let db_path = temp_db_path("sched-due");
    let backend = local_memory_backend(&db_path).await;
    let conn = backend
        .connect_for_scheduler()
        .await
        .expect("scheduler conn");
    let store = LibsqlSchedulerStore::new(conn);

    let past = "2020-01-01T00:00:00Z".to_owned();
    let future = "2099-01-01T00:00:00Z".to_owned();
    let now_str = chrono::Utc::now().to_rfc3339();

    // Due schedule (next_run_at in the past)
    store
        .create_schedule(&ScheduleDefinition {
            schedule_id: "due".to_owned(),
            user_id: "alice".to_owned(),
            name: None,
            goal: "Due task".to_owned(),
            cadence: ScheduleCadence::Interval { every_secs: 3600 },
            notification_policy: NotificationPolicy::Always,
            status: ScheduleStatus::Active,
            created_at: now_str.clone(),
            updated_at: now_str.clone(),
            next_run_at: Some(past.clone()),
            last_run_at: None,
            last_run_status: None,
            consecutive_failures: 0,
            channel_id: None,
            channel_context_id: None,
        })
        .await
        .expect("create due");

    // Not yet due (next_run_at in the future)
    store
        .create_schedule(&ScheduleDefinition {
            schedule_id: "future".to_owned(),
            user_id: "alice".to_owned(),
            name: None,
            goal: "Future task".to_owned(),
            cadence: ScheduleCadence::Interval { every_secs: 3600 },
            notification_policy: NotificationPolicy::Always,
            status: ScheduleStatus::Active,
            created_at: now_str.clone(),
            updated_at: now_str.clone(),
            next_run_at: Some(future.clone()),
            last_run_at: None,
            last_run_status: None,
            consecutive_failures: 0,
            channel_id: None,
            channel_context_id: None,
        })
        .await
        .expect("create future");

    // Paused (should not be picked up)
    store
        .create_schedule(&ScheduleDefinition {
            schedule_id: "paused".to_owned(),
            user_id: "alice".to_owned(),
            name: None,
            goal: "Paused task".to_owned(),
            cadence: ScheduleCadence::Interval { every_secs: 3600 },
            notification_policy: NotificationPolicy::Always,
            status: ScheduleStatus::Paused,
            created_at: now_str.clone(),
            updated_at: now_str.clone(),
            next_run_at: Some(past.clone()),
            last_run_at: None,
            last_run_status: None,
            consecutive_failures: 0,
            channel_id: None,
            channel_context_id: None,
        })
        .await
        .expect("create paused");

    let due = store
        .due_schedules(&now_str, 10)
        .await
        .expect("due_schedules");
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].schedule_id, "due");

    let _ = fs::remove_file(db_path);
}

#[tokio::test]
async fn scheduler_store_record_run_and_prune_history() {
    use crate::scheduler_store::{LibsqlSchedulerStore, SchedulerStore};
    use types::{
        NotificationPolicy, ScheduleCadence, ScheduleDefinition, ScheduleRunRecord,
        ScheduleRunStatus, ScheduleStatus,
    };

    let db_path = temp_db_path("sched-runs");
    let backend = local_memory_backend(&db_path).await;
    let conn = backend
        .connect_for_scheduler()
        .await
        .expect("scheduler conn");
    let store = LibsqlSchedulerStore::new(conn);

    let now = chrono::Utc::now().to_rfc3339();
    store
        .create_schedule(&ScheduleDefinition {
            schedule_id: "sched-runs".to_owned(),
            user_id: "alice".to_owned(),
            name: None,
            goal: "Task".to_owned(),
            cadence: ScheduleCadence::Interval { every_secs: 3600 },
            notification_policy: NotificationPolicy::Always,
            status: ScheduleStatus::Active,
            created_at: now.clone(),
            updated_at: now.clone(),
            next_run_at: Some(now.clone()),
            last_run_at: None,
            last_run_status: None,
            consecutive_failures: 0,
            channel_id: None,
            channel_context_id: None,
        })
        .await
        .expect("create");

    // Record 5 runs
    for i in 0..5 {
        let run = ScheduleRunRecord {
            run_id: format!("run-{i}"),
            schedule_id: "sched-runs".to_owned(),
            started_at: now.clone(),
            finished_at: now.clone(),
            status: ScheduleRunStatus::Success,
            output_summary: Some(format!("Output {i}")),
            turn_count: 1,
            cost: 0.01,
            notified: false,
            output: None,
        };
        store
            .record_run_and_reschedule("sched-runs", &run, Some(now.clone()), None)
            .await
            .expect("record run");
    }

    let history = store
        .get_run_history("sched-runs", 10)
        .await
        .expect("history");
    assert_eq!(history.len(), 5);

    // Prune to keep 2
    store
        .prune_run_history("sched-runs", 2)
        .await
        .expect("prune");

    let pruned = store
        .get_run_history("sched-runs", 10)
        .await
        .expect("history after prune");
    assert_eq!(pruned.len(), 2);

    let _ = fs::remove_file(db_path);
}

// ── Cadence evaluation tests ───────────────────────────────────────────

#[test]
fn cadence_interval_computes_next_run() {
    use crate::cadence::next_run_for_cadence;

    let cadence = types::ScheduleCadence::Interval { every_secs: 3600 };
    let now = chrono::Utc::now();
    let next = next_run_for_cadence(&cadence, now)
        .expect("should compute")
        .expect("should have next");
    let diff = (next - now).num_seconds();
    assert_eq!(diff, 3600);
}

#[test]
fn cadence_once_future_returns_some() {
    use crate::cadence::next_run_for_cadence;

    let future = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
    let cadence = types::ScheduleCadence::Once { at: future };
    let result = next_run_for_cadence(&cadence, chrono::Utc::now()).expect("should compute");
    assert!(result.is_some());
}

#[test]
fn cadence_once_past_returns_none() {
    use crate::cadence::next_run_for_cadence;

    let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
    let cadence = types::ScheduleCadence::Once { at: past };
    let result = next_run_for_cadence(&cadence, chrono::Utc::now()).expect("should compute");
    assert!(result.is_none());
}

#[test]
fn cadence_cron_computes_next_run_in_timezone() {
    use crate::cadence::next_run_for_cadence;

    let cadence = types::ScheduleCadence::Cron {
        expression: "0 8 * * *".to_owned(),
        timezone: "Asia/Kolkata".to_owned(),
    };
    let now = chrono::Utc::now();
    let next = next_run_for_cadence(&cadence, now)
        .expect("should compute")
        .expect("should have next");
    assert!(next > now, "next run should be in the future");
}

#[test]
fn validate_cadence_rejects_too_frequent_interval() {
    use crate::cadence::validate_cadence;

    let cadence = types::ScheduleCadence::Interval { every_secs: 30 };
    let result = validate_cadence(&cadence, 60);
    assert!(result.is_err());
}

#[test]
fn validate_cadence_accepts_valid_interval() {
    use crate::cadence::validate_cadence;

    let cadence = types::ScheduleCadence::Interval { every_secs: 120 };
    let result = validate_cadence(&cadence, 60);
    assert!(result.is_ok());
}

#[test]
fn validate_cadence_rejects_invalid_timezone() {
    use crate::cadence::validate_cadence;

    let cadence = types::ScheduleCadence::Cron {
        expression: "0 8 * * *".to_owned(),
        timezone: "Invalid/Zone".to_owned(),
    };
    let result = validate_cadence(&cadence, 60);
    assert!(result.is_err());
}

#[test]
fn parse_cadence_roundtrips_all_types() {
    use crate::cadence::parse_cadence;

    let once = parse_cadence("once", "2099-01-01T00:00:00Z", "UTC").expect("once");
    assert!(matches!(once, types::ScheduleCadence::Once { .. }));

    let cron = parse_cadence("cron", "0 8 * * *", "Asia/Kolkata").expect("cron");
    assert!(matches!(cron, types::ScheduleCadence::Cron { .. }));

    let interval = parse_cadence("interval", "3600", "UTC").expect("interval");
    assert!(matches!(
        interval,
        types::ScheduleCadence::Interval { every_secs: 3600 }
    ));
}

#[test]
fn parse_cadence_rejects_invalid_type() {
    use crate::cadence::parse_cadence;

    let result = parse_cadence("weekly", "monday", "UTC");
    assert!(result.is_err());
}

#[test]
fn parse_cadence_rejects_invalid_interval() {
    use crate::cadence::parse_cadence;

    let result = parse_cadence("interval", "not-a-number", "UTC");
    assert!(result.is_err());
}

#[test]
fn format_in_timezone_returns_local_for_cron() {
    use crate::cadence::format_in_timezone;

    let cadence = types::ScheduleCadence::Cron {
        expression: "0 8 * * *".to_owned(),
        timezone: "America/New_York".to_owned(),
    };
    let utc_ts = "2026-03-01T13:00:00+00:00";
    let formatted = format_in_timezone(utc_ts, &cadence);
    assert!(formatted.is_some());
    let s = formatted.unwrap();
    assert!(s.contains("EST") || s.contains("EDT") || s.contains("America"));
}

#[test]
fn format_in_timezone_returns_none_for_interval() {
    use crate::cadence::format_in_timezone;

    let cadence = types::ScheduleCadence::Interval { every_secs: 3600 };
    let result = format_in_timezone("2026-03-01T13:00:00+00:00", &cadence);
    assert!(result.is_none());
}
