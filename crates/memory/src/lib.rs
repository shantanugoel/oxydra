mod connection;
mod errors;
mod indexing;
mod schema;

use std::collections::HashMap;

use async_trait::async_trait;
use libsql::{Builder, Connection, Database, params};
use serde_json::{Map, Value, json};
use types::{
    Memory, MemoryChunkUpsertRequest, MemoryChunkUpsertResponse, MemoryConfig, MemoryError,
    MemoryForgetRequest, MemoryHybridQueryRequest, MemoryHybridQueryResult, MemoryRecallRequest,
    MemoryRecord, MemoryRetrieval, MemoryStoreRequest, MemorySummaryReadRequest,
    MemorySummaryState, MemorySummaryWriteRequest, MemorySummaryWriteResult, RetrievalConfig,
};

use connection::{ConnectionStrategy, ensure_local_parent_directory};
use errors::{connection_error, initialization_error, query_error};
use indexing::{
    EmbeddingAdapter, decode_embedding_blob, index_prepared_document, prepare_index_document,
    prepare_index_document_with_extra_metadata,
};
use schema::{
    enable_foreign_keys, ensure_migration_bookkeeping, rollback_quietly, run_pending_migrations,
    verify_required_schema,
};

const RETRIEVAL_WEIGHT_SUM_EPSILON: f64 = 1e-6;
const RETRIEVAL_CANDIDATE_MULTIPLIER: usize = 4;

pub struct LibsqlMemory {
    db: Database,
    embedding_adapter: EmbeddingAdapter,
}

#[derive(Debug, Clone)]
struct HybridCandidateRow {
    chunk_id: String,
    session_id: String,
    text: String,
    file_id: Option<i64>,
    sequence_start: Option<u64>,
    sequence_end: Option<u64>,
    metadata: Option<Value>,
    recency_sequence: u64,
}

#[derive(Debug, Clone)]
struct ScoredHybridCandidate {
    row: HybridCandidateRow,
    raw_score: f64,
}

#[derive(Debug, Clone)]
struct MergedHybridCandidate {
    row: HybridCandidateRow,
    score: f64,
    vector_score: f64,
    fts_score: f64,
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

    async fn execute_hybrid_query(
        &self,
        request: MemoryHybridQueryRequest,
    ) -> Result<Vec<MemoryHybridQueryResult>, MemoryError> {
        let query = request.query.trim();
        if query.is_empty() {
            return Err(query_error(
                "hybrid query must include a non-empty query string".to_owned(),
            ));
        }

        let defaults = RetrievalConfig::default();
        let top_k = request.top_k.unwrap_or(defaults.top_k);
        if top_k == 0 {
            return Err(query_error(
                "hybrid query `top_k` must be greater than zero".to_owned(),
            ));
        }
        let vector_weight = request.vector_weight.unwrap_or(defaults.vector_weight);
        let fts_weight = request.fts_weight.unwrap_or(defaults.fts_weight);
        validate_hybrid_weights(vector_weight, fts_weight)?;

        let query_embedding = match request.query_embedding {
            Some(embedding) => embedding,
            None => self.embedding_adapter.embed_query(query)?,
        };
        validate_query_embedding(&query_embedding)?;

        let candidate_limit = top_k
            .checked_mul(RETRIEVAL_CANDIDATE_MULTIPLIER)
            .ok_or_else(|| query_error("hybrid query top_k is too large".to_owned()))?;
        let (vector_candidates, fts_candidates) = futures::try_join!(
            self.vector_search(
                request.session_id.as_str(),
                query_embedding.as_slice(),
                candidate_limit
            ),
            self.fts_search(request.session_id.as_str(), query, candidate_limit)
        )?;

        let vector_scores = normalize_scores(vector_candidates.as_slice());
        let fts_scores = normalize_scores(fts_candidates.as_slice());
        let mut merged: HashMap<String, MergedHybridCandidate> = HashMap::new();

        for candidate in vector_candidates {
            let chunk_id = candidate.row.chunk_id.clone();
            let entry = merged
                .entry(chunk_id.clone())
                .or_insert_with(|| MergedHybridCandidate {
                    row: candidate.row,
                    score: 0.0,
                    vector_score: 0.0,
                    fts_score: 0.0,
                });
            entry.vector_score = vector_scores.get(&chunk_id).copied().unwrap_or_default();
        }
        for candidate in fts_candidates {
            let chunk_id = candidate.row.chunk_id.clone();
            let entry = merged
                .entry(chunk_id.clone())
                .or_insert_with(|| MergedHybridCandidate {
                    row: candidate.row,
                    score: 0.0,
                    vector_score: 0.0,
                    fts_score: 0.0,
                });
            entry.fts_score = fts_scores.get(&chunk_id).copied().unwrap_or_default();
        }

        let mut ranked = merged.into_values().collect::<Vec<_>>();
        for candidate in &mut ranked {
            candidate.score =
                (vector_weight * candidate.vector_score) + (fts_weight * candidate.fts_score);
        }
        ranked.sort_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| right.row.recency_sequence.cmp(&left.row.recency_sequence))
                .then_with(|| left.row.chunk_id.cmp(&right.row.chunk_id))
        });

        let mut results = Vec::with_capacity(top_k.min(ranked.len()));
        for candidate in ranked.into_iter().take(top_k) {
            results.push(MemoryHybridQueryResult {
                chunk_id: candidate.row.chunk_id,
                session_id: candidate.row.session_id,
                text: candidate.row.text,
                score: candidate.score,
                vector_score: candidate.vector_score,
                fts_score: candidate.fts_score,
                file_id: candidate.row.file_id,
                sequence_start: candidate.row.sequence_start,
                sequence_end: candidate.row.sequence_end,
                metadata: candidate.row.metadata,
            });
        }
        Ok(results)
    }

    async fn vector_search(
        &self,
        session_id: &str,
        query_embedding: &[f32],
        candidate_limit: usize,
    ) -> Result<Vec<ScoredHybridCandidate>, MemoryError> {
        let conn = self.connect()?;
        enable_foreign_keys(&conn).await?;

        let mut rows = conn
            .query(
                "SELECT c.chunk_id, c.session_id, c.chunk_text, c.file_id, c.sequence_start, c.sequence_end, c.metadata_json, cv.embedding_blob
                 FROM chunks c
                 INNER JOIN chunks_vec cv ON cv.chunk_id = c.chunk_id
                 WHERE c.session_id = ?1",
                params![session_id],
            )
            .await
            .map_err(|error| query_error(error.to_string()))?;

        let mut candidates = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|error| query_error(error.to_string()))?
        {
            let row_data = hybrid_candidate_from_row(&row)?;
            let embedding_blob = row
                .get::<Vec<u8>>(7)
                .map_err(|error| query_error(error.to_string()))?;
            let chunk_embedding = decode_embedding_blob(embedding_blob.as_slice())?;
            let score = cosine_similarity(query_embedding, chunk_embedding.as_slice())?;
            candidates.push(ScoredHybridCandidate {
                row: row_data,
                raw_score: score,
            });
        }

        candidates.sort_by(|left, right| {
            right
                .raw_score
                .total_cmp(&left.raw_score)
                .then_with(|| right.row.recency_sequence.cmp(&left.row.recency_sequence))
                .then_with(|| left.row.chunk_id.cmp(&right.row.chunk_id))
        });
        candidates.truncate(candidate_limit);
        Ok(candidates)
    }

    async fn fts_search(
        &self,
        session_id: &str,
        query: &str,
        candidate_limit: usize,
    ) -> Result<Vec<ScoredHybridCandidate>, MemoryError> {
        let conn = self.connect()?;
        enable_foreign_keys(&conn).await?;

        let candidate_limit = i64::try_from(candidate_limit)
            .map_err(|_| query_error("hybrid candidate limit exceeds sqlite range".to_owned()))?;
        let match_query = format_fts_match_query(query);
        let mut rows = conn
            .query(
                "SELECT c.chunk_id, c.session_id, c.chunk_text, c.file_id, c.sequence_start, c.sequence_end, c.metadata_json, -bm25(chunks_fts) AS score
                 FROM chunks_fts
                 INNER JOIN chunks c ON c.rowid = chunks_fts.rowid
                 WHERE chunks_fts MATCH ?1 AND c.session_id = ?2
                 ORDER BY score DESC, c.sequence_end DESC, c.sequence_start DESC, c.chunk_id ASC
                 LIMIT ?3",
                params![match_query, session_id, candidate_limit],
            )
            .await
            .map_err(|error| query_error(error.to_string()))?;

        let mut candidates = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|error| query_error(error.to_string()))?
        {
            let row_data = hybrid_candidate_from_row(&row)?;
            let raw_score = row
                .get::<f64>(7)
                .map_err(|error| query_error(error.to_string()))?;
            candidates.push(ScoredHybridCandidate {
                row: row_data,
                raw_score,
            });
        }
        Ok(candidates)
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

            let mut session_state = load_session_state_map(&conn, session_id.as_str())
                .await?
                .unwrap_or_default();
            session_state.insert("last_sequence".to_owned(), Value::from(sequence_u64));
            let session_state_json = serde_json::to_string(&Value::Object(session_state))?;

            conn.execute(
                "INSERT INTO session_state (session_id, state_json, updated_at)
                 VALUES (?1, ?2, CURRENT_TIMESTAMP)
                 ON CONFLICT(session_id) DO UPDATE SET
                      state_json = excluded.state_json,
                      updated_at = CURRENT_TIMESTAMP",
                params![session_id.as_str(), session_state_json],
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

#[async_trait]
impl MemoryRetrieval for LibsqlMemory {
    async fn upsert_chunks(
        &self,
        _request: MemoryChunkUpsertRequest,
    ) -> Result<MemoryChunkUpsertResponse, MemoryError> {
        Err(query_error(
            "chunk upsert API is not implemented yet for libsql memory".to_owned(),
        ))
    }

    async fn hybrid_query(
        &self,
        request: MemoryHybridQueryRequest,
    ) -> Result<Vec<MemoryHybridQueryResult>, MemoryError> {
        self.execute_hybrid_query(request).await
    }

    async fn read_summary_state(
        &self,
        request: MemorySummaryReadRequest,
    ) -> Result<Option<MemorySummaryState>, MemoryError> {
        let conn = self.connect()?;
        enable_foreign_keys(&conn).await?;

        let Some(session_state) =
            load_session_state_map(&conn, request.session_id.as_str()).await?
        else {
            return Ok(None);
        };
        read_summary_state_from_map(request.session_id.as_str(), &session_state)
    }

    async fn write_summary_state(
        &self,
        request: MemorySummaryWriteRequest,
    ) -> Result<MemorySummaryWriteResult, MemoryError> {
        let MemorySummaryWriteRequest {
            session_id,
            expected_epoch,
            next_epoch,
            upper_sequence,
            summary,
            metadata,
        } = request;
        if next_epoch <= expected_epoch {
            return Err(query_error(format!(
                "summary write next_epoch must be greater than expected_epoch; got expected_epoch={expected_epoch} next_epoch={next_epoch}"
            )));
        }

        let conn = self.connect()?;
        enable_foreign_keys(&conn).await?;

        conn.execute("BEGIN IMMEDIATE TRANSACTION", params![])
            .await
            .map_err(|error| query_error(error.to_string()))?;
        let transaction_result = async {
            conn.execute(
                "INSERT INTO sessions (session_id, agent_identity, created_at, updated_at)
                 VALUES (?1, NULL, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)
                 ON CONFLICT(session_id) DO UPDATE SET updated_at = CURRENT_TIMESTAMP",
                params![session_id.as_str()],
            )
            .await
            .map_err(|error| query_error(error.to_string()))?;

            let mut session_state = load_session_state_map(&conn, session_id.as_str())
                .await?
                .unwrap_or_default();
            let existing_summary = read_summary_state_from_map(session_id.as_str(), &session_state)?;
            let current_epoch = existing_summary.as_ref().map_or(0, |state| state.epoch);
            if current_epoch != expected_epoch {
                return Ok(MemorySummaryWriteResult {
                    applied: false,
                    current_epoch,
                });
            }
            if let Some(existing_summary) = existing_summary
                && upper_sequence < existing_summary.upper_sequence
            {
                return Err(query_error(format!(
                    "summary write upper_sequence must be monotonic; got {upper_sequence} but existing upper_sequence is {}",
                    existing_summary.upper_sequence
                )));
            }

            session_state.insert(
                "summary_state".to_owned(),
                json!({
                    "epoch": next_epoch,
                    "upper_sequence": upper_sequence,
                    "summary": summary,
                    "metadata": metadata,
                }),
            );
            let session_state_json = serde_json::to_string(&Value::Object(session_state))?;
            conn.execute(
                "INSERT INTO session_state (session_id, state_json, updated_at)
                 VALUES (?1, ?2, CURRENT_TIMESTAMP)
                 ON CONFLICT(session_id) DO UPDATE SET
                     state_json = excluded.state_json,
                     updated_at = CURRENT_TIMESTAMP",
                params![session_id.as_str(), session_state_json],
            )
            .await
            .map_err(|error| query_error(error.to_string()))?;

            Ok::<MemorySummaryWriteResult, MemoryError>(MemorySummaryWriteResult {
                applied: true,
                current_epoch: next_epoch,
            })
        }
        .await;
        let result = match transaction_result {
            Ok(result) => result,
            Err(error) => {
                rollback_quietly(&conn).await;
                return Err(error);
            }
        };
        conn.execute("COMMIT TRANSACTION", params![])
            .await
            .map_err(|error| query_error(error.to_string()))?;
        Ok(result)
    }

    async fn store_note(
        &self,
        session_id: &str,
        note_id: &str,
        content: &str,
    ) -> Result<(), MemoryError> {
        let conn = self.connect()?;
        enable_foreign_keys(&conn).await?;

        // Build a synthetic message payload that the indexing pipeline can extract.
        let payload = json!({
            "role": "user",
            "content": content,
            "tool_call_id": note_id,
        });
        let payload_json = serde_json::to_string(&payload)?;

        // Determine the next sequence for this session.
        let next_sequence = next_event_sequence(&conn, session_id).await?;
        let sequence = i64::try_from(next_sequence)
            .map_err(|_| query_error("note sequence exceeds sqlite integer range".to_owned()))?;

        let extra_metadata = json!({
            "note_id": note_id,
            "source": "memory_save",
        });
        let prepared_index = prepare_index_document_with_extra_metadata(
            &self.embedding_adapter,
            session_id,
            sequence,
            &payload,
            Some(&extra_metadata),
        )?;

        conn.execute("BEGIN IMMEDIATE TRANSACTION", params![])
            .await
            .map_err(|error| query_error(error.to_string()))?;
        let transaction_result = async {
            conn.execute(
                "INSERT INTO sessions (session_id, agent_identity, created_at, updated_at)
                 VALUES (?1, NULL, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)
                 ON CONFLICT(session_id) DO UPDATE SET updated_at = CURRENT_TIMESTAMP",
                params![session_id],
            )
            .await
            .map_err(|error| query_error(error.to_string()))?;

            conn.execute(
                "INSERT INTO conversation_events (session_id, sequence, payload_json, created_at)
                 VALUES (?1, ?2, ?3, CURRENT_TIMESTAMP)",
                params![session_id, sequence, payload_json],
            )
            .await
            .map_err(|error| query_error(error.to_string()))?;

            if let Some(index_document) = prepared_index.as_ref() {
                index_prepared_document(&conn, session_id, sequence, index_document).await?;
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

    async fn delete_note(&self, session_id: &str, note_id: &str) -> Result<bool, MemoryError> {
        let conn = self.connect()?;
        enable_foreign_keys(&conn).await?;

        conn.execute("BEGIN IMMEDIATE TRANSACTION", params![])
            .await
            .map_err(|error| query_error(error.to_string()))?;
        let transaction_result = async {
            // Find all chunk IDs whose metadata contains this note_id.
            let chunk_ids = find_chunk_ids_by_note_id(&conn, session_id, note_id).await?;

            // Delete matching chunks (CASCADE handles chunks_vec; triggers
            // handle chunks_fts).
            for chunk_id in &chunk_ids {
                conn.execute(
                    "DELETE FROM chunks WHERE chunk_id = ?1 AND session_id = ?2",
                    params![chunk_id.as_str(), session_id],
                )
                .await
                .map_err(|error| query_error(error.to_string()))?;
            }

            // Delete the conversation event whose payload carries this note_id
            // in its tool_call_id field.
            let deleted_events = conn
                .execute(
                    "DELETE FROM conversation_events
                     WHERE session_id = ?1
                       AND json_extract(payload_json, '$.tool_call_id') = ?2",
                    params![session_id, note_id],
                )
                .await
                .map_err(|error| query_error(error.to_string()))?;

            Ok::<bool, MemoryError>(!chunk_ids.is_empty() || deleted_events > 0)
        }
        .await;
        let found = match transaction_result {
            Ok(found) => found,
            Err(error) => {
                rollback_quietly(&conn).await;
                return Err(error);
            }
        };
        conn.execute("COMMIT TRANSACTION", params![])
            .await
            .map_err(|error| query_error(error.to_string()))?;
        Ok(found)
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

#[async_trait]
impl MemoryRetrieval for UnconfiguredMemory {
    async fn upsert_chunks(
        &self,
        _request: MemoryChunkUpsertRequest,
    ) -> Result<MemoryChunkUpsertResponse, MemoryError> {
        Err(Self::backend_unavailable())
    }

    async fn hybrid_query(
        &self,
        _request: MemoryHybridQueryRequest,
    ) -> Result<Vec<MemoryHybridQueryResult>, MemoryError> {
        Err(Self::backend_unavailable())
    }

    async fn read_summary_state(
        &self,
        _request: MemorySummaryReadRequest,
    ) -> Result<Option<MemorySummaryState>, MemoryError> {
        Err(Self::backend_unavailable())
    }

    async fn write_summary_state(
        &self,
        _request: MemorySummaryWriteRequest,
    ) -> Result<MemorySummaryWriteResult, MemoryError> {
        Err(Self::backend_unavailable())
    }

    async fn store_note(
        &self,
        _session_id: &str,
        _note_id: &str,
        _content: &str,
    ) -> Result<(), MemoryError> {
        Err(Self::backend_unavailable())
    }

    async fn delete_note(&self, _session_id: &str, _note_id: &str) -> Result<bool, MemoryError> {
        Err(Self::backend_unavailable())
    }
}

async fn load_session_state_map(
    conn: &Connection,
    session_id: &str,
) -> Result<Option<Map<String, Value>>, MemoryError> {
    let mut rows = conn
        .query(
            "SELECT state_json FROM session_state WHERE session_id = ?1 LIMIT 1",
            params![session_id],
        )
        .await
        .map_err(|error| query_error(error.to_string()))?;
    let Some(row) = rows
        .next()
        .await
        .map_err(|error| query_error(error.to_string()))?
    else {
        return Ok(None);
    };
    let state_json = row
        .get::<String>(0)
        .map_err(|error| query_error(error.to_string()))?;
    let state_value: Value = serde_json::from_str(state_json.as_str()).map_err(|error| {
        query_error(format!(
            "session_state for session `{session_id}` is not valid JSON: {error}"
        ))
    })?;
    let state_map = state_value.as_object().cloned().ok_or_else(|| {
        query_error(format!(
            "session_state for session `{session_id}` must be a JSON object"
        ))
    })?;
    Ok(Some(state_map))
}

fn read_summary_state_from_map(
    session_id: &str,
    state: &Map<String, Value>,
) -> Result<Option<MemorySummaryState>, MemoryError> {
    let Some(summary_value) = state.get("summary_state") else {
        return Ok(None);
    };
    if summary_value.is_null() {
        return Ok(None);
    }
    let summary_state = summary_value.as_object().ok_or_else(|| {
        query_error(format!(
            "summary_state for session `{session_id}` must be a JSON object"
        ))
    })?;
    let epoch = read_summary_u64_field(session_id, summary_state, "epoch")?;
    let upper_sequence = read_summary_u64_field(session_id, summary_state, "upper_sequence")?;
    let summary = read_summary_string_field(session_id, summary_state, "summary")?;
    let metadata = summary_state
        .get("metadata")
        .cloned()
        .filter(|value| !value.is_null());

    Ok(Some(MemorySummaryState {
        session_id: session_id.to_owned(),
        epoch,
        upper_sequence,
        summary,
        metadata,
    }))
}

fn read_summary_u64_field(
    session_id: &str,
    summary_state: &Map<String, Value>,
    field_name: &str,
) -> Result<u64, MemoryError> {
    let value = summary_state.get(field_name).ok_or_else(|| {
        query_error(format!(
            "summary_state for session `{session_id}` is missing `{field_name}`"
        ))
    })?;
    value.as_u64().ok_or_else(|| {
        query_error(format!(
            "summary_state field `{field_name}` for session `{session_id}` must be an unsigned integer"
        ))
    })
}

fn read_summary_string_field(
    session_id: &str,
    summary_state: &Map<String, Value>,
    field_name: &str,
) -> Result<String, MemoryError> {
    let value = summary_state.get(field_name).ok_or_else(|| {
        query_error(format!(
            "summary_state for session `{session_id}` is missing `{field_name}`"
        ))
    })?;
    value.as_str().map(str::to_owned).ok_or_else(|| {
        query_error(format!(
            "summary_state field `{field_name}` for session `{session_id}` must be a string"
        ))
    })
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

async fn next_event_sequence(
    conn: &Connection,
    session_id: &str,
) -> Result<u64, MemoryError> {
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
    if max_sequence < 0 {
        Ok(1)
    } else {
        let next = u64::try_from(max_sequence)
            .map_err(|_| query_error("stored sequence is negative".to_owned()))?;
        Ok(next.saturating_add(1))
    }
}

async fn find_chunk_ids_by_note_id(
    conn: &Connection,
    session_id: &str,
    note_id: &str,
) -> Result<Vec<String>, MemoryError> {
    let mut rows = conn
        .query(
            "SELECT chunk_id FROM chunks
             WHERE session_id = ?1
               AND json_extract(metadata_json, '$.note_id') = ?2",
            params![session_id, note_id],
        )
        .await
        .map_err(|error| query_error(error.to_string()))?;
    let mut chunk_ids = Vec::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|error| query_error(error.to_string()))?
    {
        let chunk_id = row
            .get::<String>(0)
            .map_err(|error| query_error(error.to_string()))?;
        chunk_ids.push(chunk_id);
    }
    Ok(chunk_ids)
}

fn validate_hybrid_weights(vector_weight: f64, fts_weight: f64) -> Result<(), MemoryError> {
    if !vector_weight.is_finite() || !(0.0..=1.0).contains(&vector_weight) {
        return Err(query_error(format!(
            "hybrid query vector_weight must be within [0.0, 1.0]; got {vector_weight}"
        )));
    }
    if !fts_weight.is_finite() || !(0.0..=1.0).contains(&fts_weight) {
        return Err(query_error(format!(
            "hybrid query fts_weight must be within [0.0, 1.0]; got {fts_weight}"
        )));
    }
    let sum = vector_weight + fts_weight;
    if (sum - 1.0).abs() > RETRIEVAL_WEIGHT_SUM_EPSILON {
        return Err(query_error(format!(
            "hybrid query weights must sum to 1.0; got vector_weight={vector_weight} fts_weight={fts_weight}"
        )));
    }
    Ok(())
}

fn validate_query_embedding(embedding: &[f32]) -> Result<(), MemoryError> {
    if embedding.is_empty() {
        return Err(query_error(
            "hybrid query embedding must contain at least one dimension".to_owned(),
        ));
    }
    if embedding.iter().any(|value| !value.is_finite()) {
        return Err(query_error(
            "hybrid query embedding must contain only finite f32 values".to_owned(),
        ));
    }
    Ok(())
}

fn normalize_scores(candidates: &[ScoredHybridCandidate]) -> HashMap<String, f64> {
    if candidates.is_empty() {
        return HashMap::new();
    }

    let (min_score, max_score) = candidates.iter().fold(
        (f64::INFINITY, f64::NEG_INFINITY),
        |(current_min, current_max), candidate| {
            (
                current_min.min(candidate.raw_score),
                current_max.max(candidate.raw_score),
            )
        },
    );
    let range = max_score - min_score;

    let mut normalized = HashMap::with_capacity(candidates.len());
    for candidate in candidates {
        let score = if range.abs() <= RETRIEVAL_WEIGHT_SUM_EPSILON {
            1.0
        } else {
            (candidate.raw_score - min_score) / range
        };
        normalized.insert(candidate.row.chunk_id.clone(), score);
    }
    normalized
}

fn hybrid_candidate_from_row(row: &libsql::Row) -> Result<HybridCandidateRow, MemoryError> {
    let chunk_id = row
        .get::<String>(0)
        .map_err(|error| query_error(error.to_string()))?;
    let session_id = row
        .get::<String>(1)
        .map_err(|error| query_error(error.to_string()))?;
    let text = row
        .get::<String>(2)
        .map_err(|error| query_error(error.to_string()))?;
    let file_id = row
        .get::<Option<i64>>(3)
        .map_err(|error| query_error(error.to_string()))?;
    let sequence_start = read_optional_u64(row, 4, "sequence_start")?;
    let sequence_end = read_optional_u64(row, 5, "sequence_end")?;
    let metadata_json = row
        .get::<String>(6)
        .map_err(|error| query_error(error.to_string()))?;
    let metadata: Value = serde_json::from_str(&metadata_json)?;
    let metadata = match metadata {
        Value::Null => None,
        other => Some(other),
    };
    let recency_sequence = sequence_end.or(sequence_start).unwrap_or(0);

    Ok(HybridCandidateRow {
        chunk_id,
        session_id,
        text,
        file_id,
        sequence_start,
        sequence_end,
        metadata,
        recency_sequence,
    })
}

fn read_optional_u64(
    row: &libsql::Row,
    index: i32,
    field_name: &str,
) -> Result<Option<u64>, MemoryError> {
    let value = row
        .get::<Option<i64>>(index)
        .map_err(|error| query_error(error.to_string()))?;
    value
        .map(|raw| {
            u64::try_from(raw).map_err(|_| {
                query_error(format!(
                    "stored {field_name} value {raw} is negative and cannot be converted to u64"
                ))
            })
        })
        .transpose()
}

fn cosine_similarity(query: &[f32], candidate: &[f32]) -> Result<f64, MemoryError> {
    if query.len() != candidate.len() {
        return Err(query_error(format!(
            "embedding dimension mismatch for hybrid vector search: query has {}, candidate has {}",
            query.len(),
            candidate.len()
        )));
    }
    let mut dot = 0.0_f64;
    let mut query_norm = 0.0_f64;
    let mut candidate_norm = 0.0_f64;
    for (left, right) in query.iter().zip(candidate.iter()) {
        let left = f64::from(*left);
        let right = f64::from(*right);
        dot += left * right;
        query_norm += left * left;
        candidate_norm += right * right;
    }
    if query_norm <= f64::EPSILON || candidate_norm <= f64::EPSILON {
        return Ok(0.0);
    }
    Ok(dot / (query_norm.sqrt() * candidate_norm.sqrt()))
}

fn format_fts_match_query(query: &str) -> String {
    let escaped = query.replace('"', "\"\"");
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests;
