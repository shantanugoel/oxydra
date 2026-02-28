use std::sync::Arc;

use libsql::{Connection, params};
use model2vec_rs::model::StaticModel;
use serde_json::json;
use types::{
    MemoryConfig, MemoryEmbeddingBackend, MemoryError, Message, MessageRole, Model2vecModel,
};

use crate::errors::{initialization_error, query_error};

const INDEX_CHUNK_MAX_CHARS: usize = 640;
const INDEX_CHUNK_OVERLAP_CHARS: usize = 96;
const DETERMINISTIC_EMBEDDING_DIMENSIONS: usize = 64;
const INDEX_VECTOR_DIMENSIONS: usize = 512;
const DETERMINISTIC_EMBEDDING_MODEL: &str = "deterministic-hash-v1";
const MODEL2VEC_POTION_8M_MODEL_ID: &str = "minishlab/potion-base-8M";
const MODEL2VEC_POTION_32M_MODEL_ID: &str = "minishlab/potion-base-32M";

#[derive(Debug, Clone)]
pub(crate) struct PreparedIndexDocument {
    source_uri: String,
    content_hash: String,
    metadata_json: String,
    embedding_model: String,
    chunks: Vec<PreparedChunk>,
}

#[derive(Debug, Clone)]
struct PreparedChunk {
    chunk_id: String,
    content_hash: String,
    chunk_text: String,
    metadata_json: String,
    note_id: Option<String>,
    sequence_start: i64,
    sequence_end: i64,
    embedding_json: String,
}

#[derive(Debug)]
struct IndexablePayload {
    normalized_text: String,
    role: &'static str,
    source: &'static str,
    tool_call_id: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct EmbeddingAdapter {
    backend: EmbeddingBackend,
}

#[derive(Debug, Clone)]
enum EmbeddingBackend {
    Deterministic,
    Model2vec(Model2vecBackend),
}

#[derive(Debug, Clone)]
struct Model2vecBackend {
    model_id: String,
    model: Arc<StaticModel>,
}

#[derive(Debug, Clone)]
struct EmbeddingBatch {
    model: String,
    vectors: Vec<Vec<f32>>,
}

impl EmbeddingAdapter {
    pub(crate) fn from_memory_config(config: &MemoryConfig) -> Result<Self, MemoryError> {
        match config.embedding_backend {
            MemoryEmbeddingBackend::Deterministic => Ok(Self::deterministic()),
            MemoryEmbeddingBackend::Model2vec => Self::model2vec(config.model2vec_model),
        }
    }

    pub(crate) fn deterministic() -> Self {
        Self {
            backend: EmbeddingBackend::Deterministic,
        }
    }

    fn model2vec(model: Model2vecModel) -> Result<Self, MemoryError> {
        let model_id = match model {
            Model2vecModel::Potion8m => MODEL2VEC_POTION_8M_MODEL_ID,
            Model2vecModel::Potion32m => MODEL2VEC_POTION_32M_MODEL_ID,
        };
        let loaded = StaticModel::from_pretrained(model_id, None, None, None).map_err(|error| {
            initialization_error(format!(
                "model2vec initialization failed for `{model_id}`: {error}"
            ))
        })?;
        Ok(Self {
            backend: EmbeddingBackend::Model2vec(Model2vecBackend {
                model_id: model_id.to_owned(),
                model: Arc::new(loaded),
            }),
        })
    }

    fn embed_batch(&self, texts: &[String]) -> Result<EmbeddingBatch, MemoryError> {
        if texts.is_empty() {
            let model = match &self.backend {
                EmbeddingBackend::Deterministic => DETERMINISTIC_EMBEDDING_MODEL.to_owned(),
                EmbeddingBackend::Model2vec(backend) => backend.model_id.clone(),
            };
            return Ok(EmbeddingBatch {
                model,
                vectors: Vec::new(),
            });
        }
        match &self.backend {
            EmbeddingBackend::Deterministic => {
                let vectors = texts
                    .iter()
                    .map(|text| deterministic_embedding(text))
                    .map(normalize_embedding_dimensions)
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(EmbeddingBatch {
                    model: DETERMINISTIC_EMBEDDING_MODEL.to_owned(),
                    vectors,
                })
            }
            EmbeddingBackend::Model2vec(backend) => {
                let vectors = backend
                    .model
                    .encode(texts)
                    .into_iter()
                    .map(normalize_embedding_dimensions)
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(EmbeddingBatch {
                    model: backend.model_id.clone(),
                    vectors,
                })
            }
        }
    }

    pub(crate) fn embed_query(&self, text: &str) -> Result<Vec<f32>, MemoryError> {
        let normalized = normalize_text_for_indexing(text);
        if normalized.is_empty() {
            return Ok(vec![0.0; INDEX_VECTOR_DIMENSIONS]);
        }
        let batch = self.embed_batch(&[normalized])?;
        batch
            .vectors
            .into_iter()
            .next()
            .ok_or_else(|| query_error("query embedding batch unexpectedly empty".to_owned()))
    }
}

impl Default for EmbeddingAdapter {
    fn default() -> Self {
        Self::deterministic()
    }
}

pub(crate) fn prepare_index_document(
    embedding_adapter: &EmbeddingAdapter,
    session_id: &str,
    sequence: i64,
    payload: &serde_json::Value,
) -> Result<Option<PreparedIndexDocument>, MemoryError> {
    prepare_index_document_with_extra_metadata(
        embedding_adapter,
        session_id,
        sequence,
        payload,
        None,
    )
}

pub(crate) fn prepare_index_document_with_extra_metadata(
    embedding_adapter: &EmbeddingAdapter,
    session_id: &str,
    sequence: i64,
    payload: &serde_json::Value,
    extra_metadata: Option<&serde_json::Value>,
) -> Result<Option<PreparedIndexDocument>, MemoryError> {
    let Some(indexable_payload) = extract_indexable_payload(payload) else {
        return Ok(None);
    };

    let chunk_texts = split_text_into_chunks(&indexable_payload.normalized_text);
    if chunk_texts.is_empty() {
        return Ok(None);
    }

    let embedding_batch = embedding_adapter.embed_batch(&chunk_texts)?;
    if embedding_batch.vectors.len() != chunk_texts.len() {
        return Err(query_error(format!(
            "embedding count mismatch: expected {}, got {}",
            chunk_texts.len(),
            embedding_batch.vectors.len()
        )));
    }

    let chunk_count = chunk_texts.len();
    let file_metadata_json = serde_json::to_string(&json!({
        "source": indexable_payload.source,
        "role": indexable_payload.role,
        "sequence": sequence,
    }))?;
    let source_uri = format!("conversation://{session_id}/{}", indexable_payload.source);
    let mut chunks = Vec::with_capacity(chunk_count);
    for (chunk_index, (chunk_text, embedding)) in chunk_texts
        .into_iter()
        .zip(embedding_batch.vectors.into_iter())
        .enumerate()
    {
        let content_hash = stable_hash_hex(chunk_text.as_bytes());
        let mut chunk_metadata = json!({
            "source": indexable_payload.source,
            "role": indexable_payload.role,
            "sequence_start": sequence,
            "sequence_end": sequence,
            "chunk_index": chunk_index,
            "chunk_count": chunk_count,
            "tool_call_id": indexable_payload.tool_call_id.clone(),
        });
        if let Some(extra) = extra_metadata
            && let (Some(target), Some(source)) =
                (chunk_metadata.as_object_mut(), extra.as_object())
        {
            for (key, value) in source {
                target.insert(key.clone(), value.clone());
            }
        }
        let note_id = chunk_metadata
            .get("note_id")
            .and_then(|value| value.as_str())
            .map(str::to_owned);
        let chunk_metadata_json = serde_json::to_string(&chunk_metadata)?;
        chunks.push(PreparedChunk {
            chunk_id: format!(
                "chunk:{session_id}:{sequence}:{chunk_index}:{}",
                &content_hash[..16]
            ),
            content_hash,
            chunk_text,
            metadata_json: chunk_metadata_json,
            note_id,
            sequence_start: sequence,
            sequence_end: sequence,
            embedding_json: encode_embedding_json(&embedding)?,
        });
    }

    Ok(Some(PreparedIndexDocument {
        source_uri,
        content_hash: stable_hash_hex(indexable_payload.normalized_text.as_bytes()),
        metadata_json: file_metadata_json,
        embedding_model: embedding_batch.model,
        chunks,
    }))
}

fn extract_indexable_payload(payload: &serde_json::Value) -> Option<IndexablePayload> {
    let message: Message = serde_json::from_value(payload.clone()).ok()?;
    let mut components = Vec::new();
    if let Some(content) = message.content.as_deref().map(str::trim)
        && !content.is_empty()
    {
        components.push(content.to_owned());
    }
    for tool_call in &message.tool_calls {
        let arguments = format_tool_call_arguments(&tool_call.arguments);
        if arguments.is_empty() {
            components.push(format!("tool {} invocation", tool_call.name));
        } else {
            components.push(format!("tool {} arguments {}", tool_call.name, arguments));
        }
    }
    if components.is_empty() {
        return None;
    }

    let normalized_text = normalize_text_for_indexing(&components.join("\n"));
    if normalized_text.is_empty() {
        return None;
    }

    Some(IndexablePayload {
        normalized_text,
        role: message_role_name(&message.role),
        source: message_source_name(&message),
        tool_call_id: message.tool_call_id,
    })
}

fn format_tool_call_arguments(arguments: &serde_json::Value) -> String {
    match arguments {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(value) => value.clone(),
        _ => serde_json::to_string(arguments).unwrap_or_default(),
    }
}

fn message_role_name(role: &MessageRole) -> &'static str {
    match role {
        MessageRole::System => "system",
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::Tool => "tool",
    }
}

fn message_source_name(message: &Message) -> &'static str {
    match message.role {
        MessageRole::Tool => "tool_output",
        MessageRole::Assistant if !message.tool_calls.is_empty() => "assistant_tool_call",
        MessageRole::Assistant => "assistant_message",
        MessageRole::User => "user_message",
        MessageRole::System => "system_message",
    }
}

fn normalize_text_for_indexing(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn split_text_into_chunks(text: &str) -> Vec<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    let chars: Vec<char> = trimmed.chars().collect();
    let mut chunks = Vec::new();
    let mut start = 0_usize;
    while start < chars.len() {
        let end = (start + INDEX_CHUNK_MAX_CHARS).min(chars.len());
        let chunk: String = chars[start..end].iter().collect();
        let normalized_chunk = normalize_text_for_indexing(&chunk);
        if !normalized_chunk.is_empty() {
            chunks.push(normalized_chunk);
        }
        if end == chars.len() {
            break;
        }
        start = end.saturating_sub(INDEX_CHUNK_OVERLAP_CHARS);
        if start >= end {
            break;
        }
    }
    chunks
}

fn deterministic_embedding(text: &str) -> Vec<f32> {
    if text.trim().is_empty() {
        return vec![0.0; DETERMINISTIC_EMBEDDING_DIMENSIONS];
    }

    let mut embedding = Vec::with_capacity(DETERMINISTIC_EMBEDDING_DIMENSIONS);
    let mut seed = blake3::hash(text.as_bytes()).as_bytes().to_vec();
    while embedding.len() < DETERMINISTIC_EMBEDDING_DIMENSIONS {
        for byte in &seed {
            if embedding.len() == DETERMINISTIC_EMBEDDING_DIMENSIONS {
                break;
            }
            embedding.push((*byte as f32 / 127.5) - 1.0);
        }
        seed = blake3::hash(&seed).as_bytes().to_vec();
    }

    let norm = embedding
        .iter()
        .map(|value| value * value)
        .sum::<f32>()
        .sqrt();
    if norm > f32::EPSILON {
        for value in &mut embedding {
            *value /= norm;
        }
    }
    embedding
}

fn normalize_embedding_dimensions(mut embedding: Vec<f32>) -> Result<Vec<f32>, MemoryError> {
    match embedding.len().cmp(&INDEX_VECTOR_DIMENSIONS) {
        std::cmp::Ordering::Less => {
            embedding.resize(INDEX_VECTOR_DIMENSIONS, 0.0);
            Ok(embedding)
        }
        std::cmp::Ordering::Equal => Ok(embedding),
        std::cmp::Ordering::Greater => Err(query_error(format!(
            "embedding dimension {} exceeds indexed dimension {}",
            embedding.len(),
            INDEX_VECTOR_DIMENSIONS
        ))),
    }
}

pub(crate) fn normalize_embedding_for_index(embedding: &[f32]) -> Result<Vec<f32>, MemoryError> {
    normalize_embedding_dimensions(embedding.to_vec())
}

pub(crate) fn encode_embedding_json(embedding: &[f32]) -> Result<String, MemoryError> {
    serde_json::to_string(embedding).map_err(Into::into)
}

fn stable_hash_hex(input: &[u8]) -> String {
    blake3::hash(input).to_hex().to_string()
}

pub(crate) async fn index_prepared_document(
    conn: &Connection,
    session_id: &str,
    sequence: i64,
    document: &PreparedIndexDocument,
) -> Result<(), MemoryError> {
    let file_id = upsert_index_file(conn, session_id, document).await?;
    for chunk in &document.chunks {
        if let Some(existing_chunk_id) = find_existing_chunk_id(
            conn,
            session_id,
            chunk.content_hash.as_str(),
            chunk.note_id.as_deref(),
        )
        .await?
        {
            let replacement_metadata = chunk.note_id.as_ref().map(|_| chunk.metadata_json.as_str());
            touch_existing_chunk(
                conn,
                existing_chunk_id.as_str(),
                sequence,
                replacement_metadata,
            )
            .await?;
            continue;
        }

        conn.execute(
            "INSERT INTO chunks (
                chunk_id, session_id, file_id, sequence_start, sequence_end, chunk_text, metadata_json, content_hash
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8
             )",
            params![
                chunk.chunk_id.as_str(),
                session_id,
                file_id,
                chunk.sequence_start,
                chunk.sequence_end,
                chunk.chunk_text.as_str(),
                chunk.metadata_json.as_str(),
                chunk.content_hash.as_str(),
            ],
        )
        .await
        .map_err(|error| query_error(error.to_string()))?;

        conn.execute(
            "INSERT INTO chunks_vec (chunk_id, embedding_blob, embedding_model, created_at, updated_at)
             VALUES (?1, vector32(?2), ?3, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)
             ON CONFLICT(chunk_id) DO UPDATE SET
                 embedding_blob = vector32(?2),
                 embedding_model = excluded.embedding_model,
                 updated_at = CURRENT_TIMESTAMP",
            params![
                chunk.chunk_id.as_str(),
                chunk.embedding_json.as_str(),
                document.embedding_model.as_str(),
            ],
        )
        .await
        .map_err(|error| query_error(error.to_string()))?;
    }
    Ok(())
}

async fn upsert_index_file(
    conn: &Connection,
    session_id: &str,
    document: &PreparedIndexDocument,
) -> Result<i64, MemoryError> {
    conn.execute(
        "INSERT INTO files (session_id, source_uri, content_hash, metadata_json, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)
         ON CONFLICT(session_id, source_uri, content_hash) DO UPDATE SET
             metadata_json = excluded.metadata_json,
             updated_at = CURRENT_TIMESTAMP",
        params![
            session_id,
            document.source_uri.as_str(),
            document.content_hash.as_str(),
            document.metadata_json.as_str(),
        ],
    )
    .await
    .map_err(|error| query_error(error.to_string()))?;

    let mut rows = conn
        .query(
            "SELECT file_id FROM files
             WHERE session_id = ?1 AND source_uri = ?2 AND content_hash = ?3
             LIMIT 1",
            params![
                session_id,
                document.source_uri.as_str(),
                document.content_hash.as_str(),
            ],
        )
        .await
        .map_err(|error| query_error(error.to_string()))?;
    let row = rows
        .next()
        .await
        .map_err(|error| query_error(error.to_string()))?
        .ok_or_else(|| query_error("failed to resolve file_id for indexed content".to_owned()))?;
    row.get::<i64>(0)
        .map_err(|error| query_error(error.to_string()))
}

async fn find_existing_chunk_id(
    conn: &Connection,
    session_id: &str,
    content_hash: &str,
    note_id: Option<&str>,
) -> Result<Option<String>, MemoryError> {
    let mut rows = conn
        .query(
            "SELECT chunk_id FROM chunks
              WHERE session_id = ?1 AND content_hash = ?2
               AND (
                   (?3 IS NULL AND json_extract(metadata_json, '$.note_id') IS NULL)
                   OR json_extract(metadata_json, '$.note_id') = ?3
               )
              ORDER BY updated_at DESC, chunk_id ASC
              LIMIT 1",
            params![session_id, content_hash, note_id],
        )
        .await
        .map_err(|error| query_error(error.to_string()))?;
    let row = rows
        .next()
        .await
        .map_err(|error| query_error(error.to_string()))?;
    let Some(row) = row else {
        return Ok(None);
    };
    row.get::<String>(0)
        .map(Some)
        .map_err(|error| query_error(error.to_string()))
}

async fn touch_existing_chunk(
    conn: &Connection,
    chunk_id: &str,
    sequence: i64,
    replacement_metadata_json: Option<&str>,
) -> Result<(), MemoryError> {
    conn.execute(
        "UPDATE chunks
         SET sequence_end = CASE
                 WHEN sequence_end IS NULL OR sequence_end < ?2 THEN ?2
                 ELSE sequence_end
             END,
             metadata_json = COALESCE(?3, metadata_json),
             updated_at = CURRENT_TIMESTAMP
         WHERE chunk_id = ?1",
        params![chunk_id, sequence, replacement_metadata_json],
    )
    .await
    .map_err(|error| query_error(error.to_string()))?;
    Ok(())
}
