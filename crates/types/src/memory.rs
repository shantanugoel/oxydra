use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::MemoryError;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryStoreRequest {
    pub session_id: String,
    pub sequence: u64,
    pub payload: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryRecallRequest {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryForgetRequest {
    pub session_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryRecord {
    pub session_id: String,
    pub sequence: u64,
    pub payload: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryChunkUpsertRequest {
    pub session_id: String,
    pub chunks: Vec<MemoryChunkDocument>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryChunkDocument {
    pub chunk_id: String,
    pub content_hash: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence_start: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence_end: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<Vec<f32>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding_model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryChunkUpsertResponse {
    pub upserted_chunks: usize,
    pub skipped_chunks: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryHybridQueryRequest {
    pub session_id: String,
    pub query: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_embedding: Option<Vec<f32>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vector_weight: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fts_weight: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryHybridQueryResult {
    pub chunk_id: String,
    pub session_id: String,
    pub text: String,
    pub score: f64,
    pub vector_score: f64,
    pub fts_score: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence_start: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence_end: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemorySummaryReadRequest {
    pub session_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemorySummaryState {
    pub session_id: String,
    pub epoch: u64,
    pub upper_sequence: u64,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemorySummaryWriteRequest {
    pub session_id: String,
    pub expected_epoch: u64,
    pub next_epoch: u64,
    pub upper_sequence: u64,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemorySummaryWriteResult {
    pub applied: bool,
    pub current_epoch: u64,
}

#[async_trait]
pub trait Memory: Send + Sync {
    async fn store(&self, request: MemoryStoreRequest) -> Result<MemoryRecord, MemoryError>;

    async fn recall(&self, request: MemoryRecallRequest) -> Result<Vec<MemoryRecord>, MemoryError>;

    async fn forget(&self, request: MemoryForgetRequest) -> Result<(), MemoryError>;
}

#[async_trait]
pub trait MemoryRetrieval: Memory {
    async fn upsert_chunks(
        &self,
        request: MemoryChunkUpsertRequest,
    ) -> Result<MemoryChunkUpsertResponse, MemoryError>;

    async fn hybrid_query(
        &self,
        request: MemoryHybridQueryRequest,
    ) -> Result<Vec<MemoryHybridQueryResult>, MemoryError>;

    async fn read_summary_state(
        &self,
        request: MemorySummaryReadRequest,
    ) -> Result<Option<MemorySummaryState>, MemoryError>;

    async fn write_summary_state(
        &self,
        request: MemorySummaryWriteRequest,
    ) -> Result<MemorySummaryWriteResult, MemoryError>;

    /// Store a note in the given session with the specified `note_id`.
    ///
    /// The content is stored as a synthetic conversation event, chunked,
    /// embedded, and indexed for hybrid search. The `note_id` is propagated
    /// into each chunk's `metadata_json` so it can be queried by
    /// [`Self::delete_note`].
    async fn store_note(
        &self,
        session_id: &str,
        note_id: &str,
        content: &str,
    ) -> Result<(), MemoryError>;

    /// Delete all chunks and the conversation event associated with `note_id`
    /// within the given session.
    ///
    /// Returns `true` if the note was found and deleted, `false` if nothing
    /// matched.
    async fn delete_note(&self, session_id: &str, note_id: &str) -> Result<bool, MemoryError>;
}
