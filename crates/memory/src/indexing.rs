#[cfg(feature = "fastembed")]
use fastembed::TextEmbedding;
use libsql::{Connection, params};
use serde_json::json;
use types::{MemoryError, Message, MessageRole};

use crate::errors::query_error;

const INDEX_CHUNK_MAX_CHARS: usize = 640;
const INDEX_CHUNK_OVERLAP_CHARS: usize = 96;
const DETERMINISTIC_EMBEDDING_DIMENSIONS: usize = 64;
const DETERMINISTIC_EMBEDDING_MODEL: &str = "deterministic-hash-v1";
#[cfg(feature = "fastembed")]
const FASTEMBED_EMBEDDING_MODEL: &str = "fastembed-default";

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
    sequence_start: i64,
    sequence_end: i64,
    embedding_blob: Vec<u8>,
}

#[derive(Debug)]
struct IndexablePayload {
    normalized_text: String,
    role: &'static str,
    source: &'static str,
    tool_call_id: Option<String>,
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct EmbeddingAdapter;

#[derive(Debug, Clone)]
struct EmbeddingBatch {
    model: String,
    vectors: Vec<Vec<f32>>,
}

impl EmbeddingAdapter {
    fn embed_batch(&self, texts: &[String]) -> Result<EmbeddingBatch, MemoryError> {
        if texts.is_empty() {
            return Ok(EmbeddingBatch {
                model: DETERMINISTIC_EMBEDDING_MODEL.to_owned(),
                vectors: Vec::new(),
            });
        }

        #[cfg(feature = "fastembed")]
        if std::env::var("OXYDRA_MEMORY_EMBEDDING_BACKEND")
            .ok()
            .as_deref()
            .is_some_and(|value| value.eq_ignore_ascii_case("fastembed"))
        {
            let vectors = embed_with_fastembed(texts)?;
            return Ok(EmbeddingBatch {
                model: FASTEMBED_EMBEDDING_MODEL.to_owned(),
                vectors,
            });
        }

        let vectors = texts
            .iter()
            .map(|text| deterministic_embedding(text))
            .collect();
        Ok(EmbeddingBatch {
            model: DETERMINISTIC_EMBEDDING_MODEL.to_owned(),
            vectors,
        })
    }

    pub(crate) fn embed_query(&self, text: &str) -> Result<Vec<f32>, MemoryError> {
        let normalized = normalize_text_for_indexing(text);
        if normalized.is_empty() {
            return Ok(vec![0.0; DETERMINISTIC_EMBEDDING_DIMENSIONS]);
        }
        let batch = self.embed_batch(&[normalized])?;
        batch
            .vectors
            .into_iter()
            .next()
            .ok_or_else(|| query_error("query embedding batch unexpectedly empty".to_owned()))
    }
}

#[cfg(feature = "fastembed")]
fn embed_with_fastembed(texts: &[String]) -> Result<Vec<Vec<f32>>, MemoryError> {
    let mut model = TextEmbedding::try_new(Default::default())
        .map_err(|error| query_error(format!("fastembed model initialization failed: {error}")))?;
    model
        .embed(texts, None)
        .map_err(|error| query_error(format!("fastembed embedding failed: {error}")))
}

pub(crate) fn prepare_index_document(
    embedding_adapter: &EmbeddingAdapter,
    session_id: &str,
    sequence: i64,
    payload: &serde_json::Value,
) -> Result<Option<PreparedIndexDocument>, MemoryError> {
    prepare_index_document_with_extra_metadata(embedding_adapter, session_id, sequence, payload, None)
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
        let chunk_metadata_json = serde_json::to_string(&chunk_metadata)?;
        chunks.push(PreparedChunk {
            chunk_id: format!(
                "chunk:{session_id}:{sequence}:{chunk_index}:{}",
                &content_hash[..16]
            ),
            content_hash,
            chunk_text,
            metadata_json: chunk_metadata_json,
            sequence_start: sequence,
            sequence_end: sequence,
            embedding_blob: encode_embedding_blob(&embedding),
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

fn encode_embedding_blob(embedding: &[f32]) -> Vec<u8> {
    let mut blob = Vec::with_capacity(std::mem::size_of_val(embedding));
    for value in embedding {
        blob.extend_from_slice(&value.to_le_bytes());
    }
    blob
}

pub(crate) fn decode_embedding_blob(blob: &[u8]) -> Result<Vec<f32>, MemoryError> {
    const FLOAT_WIDTH: usize = std::mem::size_of::<f32>();
    if !blob.len().is_multiple_of(FLOAT_WIDTH) {
        return Err(query_error(format!(
            "embedding blob length {} is not aligned to f32 width",
            blob.len()
        )));
    }
    let mut embedding = Vec::with_capacity(blob.len() / FLOAT_WIDTH);
    for chunk in blob.chunks_exact(FLOAT_WIDTH) {
        embedding.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    Ok(embedding)
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
        if let Some(existing_chunk_id) =
            find_existing_chunk_id(conn, session_id, chunk.content_hash.as_str()).await?
        {
            touch_existing_chunk(conn, existing_chunk_id.as_str(), sequence).await?;
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
             VALUES (?1, ?2, ?3, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)
             ON CONFLICT(chunk_id) DO UPDATE SET
                 embedding_blob = excluded.embedding_blob,
                 embedding_model = excluded.embedding_model,
                 updated_at = CURRENT_TIMESTAMP",
            params![
                chunk.chunk_id.as_str(),
                chunk.embedding_blob.as_slice(),
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
) -> Result<Option<String>, MemoryError> {
    let mut rows = conn
        .query(
            "SELECT chunk_id FROM chunks
             WHERE session_id = ?1 AND content_hash = ?2
             ORDER BY updated_at DESC, chunk_id ASC
             LIMIT 1",
            params![session_id, content_hash],
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
) -> Result<(), MemoryError> {
    conn.execute(
        "UPDATE chunks
         SET sequence_end = CASE
                 WHEN sequence_end IS NULL OR sequence_end < ?2 THEN ?2
                 ELSE sequence_end
             END,
             updated_at = CURRENT_TIMESTAMP
         WHERE chunk_id = ?1",
        params![chunk_id, sequence],
    )
    .await
    .map_err(|error| query_error(error.to_string()))?;
    Ok(())
}
