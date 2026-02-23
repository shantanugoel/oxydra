# Chapter 6: Memory and Retrieval

## Overview

The memory system provides durable conversation persistence, hybrid retrieval (vector similarity + full-text search), and rolling summarization — all backed by an embedded libSQL database. The system is local-first by default, with optional Turso remote connectivity for shared deployments.

## Architecture

```
┌──────────────────────────┐
│      AgentRuntime        │
│                          │
│  ┌─────────┐ ┌────────┐ │
│  │ Memory  │ │MemRet  │ │
│  │ (store, │ │(hybrid │ │
│  │ recall, │ │ query, │ │
│  │ forget) │ │summary,│ │
│  │         │ │ notes) │ │
│  └────┬────┘ └───┬────┘ │
│       │          │      │
└───────┼──────────┼──────┘
        │          │
   ┌────▼──────────▼────┐
   │    LibsqlMemory    │
   │                    │
   │  ┌──────────────┐  │
   │  │   libSQL     │  │
   │  │  (embedded)  │  │
   │  └──────┬───────┘  │
   │         │          │
   │  ┌──────▼───────┐  │
   │  │ SQL Tables:  │  │
   │  │  sessions    │  │
   │  │  events      │  │
   │  │  chunks      │  │
   │  │  chunks_vec  │  │
   │  │  chunks_fts  │  │
   │  └──────────────┘  │
   └────────────────────┘
```

## Memory Traits

Two traits defined in `types/src/memory.rs`:

### `Memory` — Basic Persistence

```rust
#[async_trait]
pub trait Memory: Send + Sync {
    async fn store(&self, session_id: &str, sequence: i64, payload: Value) -> Result<(), MemoryError>;
    async fn recall(&self, session_id: &str, limit: Option<usize>) -> Result<Vec<MemoryRecord>, MemoryError>;
    async fn forget(&self, session_id: &str) -> Result<(), MemoryError>;
}
```

### `MemoryRetrieval` — Advanced Retrieval

```rust
#[async_trait]
pub trait MemoryRetrieval: Send + Sync {
    async fn hybrid_query(&self, session_id: &str, query: &str, max_results: usize) -> Result<Vec<RetrievalResult>, MemoryError>;
    async fn read_summary_state(&self, session_id: &str) -> Result<Option<MemorySummaryState>, MemoryError>;
    async fn write_summary_state(&self, session_id: &str, state: &MemorySummaryState, expected_epoch: u64) -> Result<SummaryWriteOutcome, MemoryError>;
    async fn store_note(&self, session_id: &str, note_id: &str, content: &str) -> Result<(), MemoryError>;
    async fn delete_note(&self, session_id: &str, note_id: &str) -> Result<bool, MemoryError>;
}
```

## Database Schema

The schema is managed via versioned SQL migrations in `memory/migrations/`. The migration runner (`memory/src/schema.rs`) tracks applied versions in a `memory_migrations` table and applies pending migrations in transactions.

### Tables

#### `sessions`

```sql
CREATE TABLE sessions (
    session_id TEXT PRIMARY KEY,
    agent_identity TEXT,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);
```

Root table for session metadata. Created lazily on first message storage.

#### `conversation_events`

```sql
CREATE TABLE conversation_events (
    session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE CASCADE,
    sequence INTEGER NOT NULL,
    payload_json TEXT NOT NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (session_id, sequence)
);
```

Stores the raw message history. Each event is a JSON-serialized `Message` with a monotonically increasing sequence number. The composite primary key enforces ordering.

#### `session_state`

```sql
CREATE TABLE session_state (
    session_id TEXT PRIMARY KEY REFERENCES sessions(session_id) ON DELETE CASCADE,
    state_json TEXT NOT NULL DEFAULT '{}',
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);
```

Stores transient session data: `last_sequence` (high-water mark) and `summary_state` (rolling summary text, epoch counter, upper sequence boundary).

#### `files`

```sql
CREATE TABLE files (
    file_id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE CASCADE,
    source_uri TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    metadata_json TEXT DEFAULT '{}',
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);
```

Tracks indexed documents and their content hashes to prevent redundant processing.

#### `chunks`

```sql
CREATE TABLE chunks (
    chunk_id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE CASCADE,
    chunk_text TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    sequence_start INTEGER NOT NULL,
    sequence_end INTEGER NOT NULL,
    metadata_json TEXT DEFAULT '{}'
);
```

Text segments extracted from messages for indexing. Each chunk tracks its position in the conversation via `sequence_start` and `sequence_end`.

#### `chunks_vec`

```sql
CREATE TABLE chunks_vec (
    chunk_id TEXT PRIMARY KEY REFERENCES chunks(chunk_id) ON DELETE CASCADE,
    embedding_blob BLOB NOT NULL,
    embedding_model TEXT NOT NULL
);
```

Stores binary float vectors (little-endian f32 arrays) for cosine similarity search.

#### `chunks_fts` (FTS5 Virtual Table)

```sql
CREATE VIRTUAL TABLE chunks_fts USING fts5(
    chunk_text,
    content='chunks',
    content_rowid='rowid'
);
```

Full-text search index using SQLite's FTS5 engine. Automatically synchronized via triggers on the `chunks` table.

## Message Storage Pipeline

When a message is stored:

1. **Session upsert** — creates session if first message, updates `updated_at` otherwise
2. **Monotonicity check** — verifies sequence > max existing sequence
3. **Event insertion** — stores the JSON-serialized message
4. **State update** — updates `last_sequence` in `session_state`
5. **Text extraction** — `extract_indexable_payload` pulls text from message content and tool call arguments
6. **Chunking** — text is normalized and split into chunks (640 chars max, 96-char overlap)
7. **Embedding** — each chunk is embedded via the configured backend
8. **Deduplication** — chunks with matching `content_hash` update `sequence_end` instead of inserting
9. **Index storage** — new chunks are inserted into `chunks`, `chunks_vec`, and `chunks_fts` (via triggers)

## Note Storage Pipeline

The `store_note` and `delete_note` methods on `MemoryRetrieval` provide a direct note management API, distinct from the conversation message pipeline. These are used by the memory tools (see Chapter 4) to give the LLM explicit control over persisted knowledge.

### Storing a Note (`store_note`)

When a note is stored:

1. **Session upsert** — creates the session (using the `memory:{user_id}` namespace) if it doesn't exist
2. **Sequence allocation** — determines the next monotonic sequence number for the session via `next_event_sequence`
3. **Event insertion** — stores a `conversation_event` with the note content as the JSON payload
4. **Indexing with metadata** — the note content is chunked and embedded using the same pipeline as conversation messages, but with an additional `note_id` field merged into each chunk's metadata via `prepare_index_document_with_extra_metadata`

The `note_id` (format: `note-{uuid}`) is generated by the calling tool and stored in chunk metadata, enabling subsequent lookups by note identifier.

### Deleting a Note (`delete_note`)

When a note is deleted:

1. **Chunk discovery** — `find_chunk_ids_by_note_id` queries the `chunks` table for all chunks whose `metadata_json` contains the target `note_id`
2. **Not-found check** — returns `false` (no chunks found) or proceeds with deletion
3. **Cascade deletion** — deletes matched chunks from the `chunks` table; `ON DELETE CASCADE` foreign keys automatically clean up `chunks_vec`, and FTS5 triggers synchronize `chunks_fts`
4. **Event cleanup** — deletes the corresponding `conversation_event` by matching the `note_id` in `payload_json`

### Update Semantics

Note updates are implemented as delete-then-store: the memory tool deletes all chunks and events for the old `note_id`, then stores new content under the same identifier. This preserves the note's identity while fully replacing its searchable content.

## Hybrid Retrieval

The `hybrid_query` method combines two search signals:

### Vector Search

1. Generate an embedding for the query text
2. Fetch all chunk embeddings for the session from `chunks_vec`
3. Compute cosine similarity in-memory between query embedding and each stored embedding
4. Return top-N by similarity

### Full-Text Search (FTS5)

```sql
SELECT ..., -bm25(chunks_fts) AS score
FROM chunks_fts
INNER JOIN chunks c ON c.rowid = chunks_fts.rowid
WHERE chunks_fts MATCH ?1 AND c.session_id = ?2
ORDER BY score DESC LIMIT ?3;
```

### Weighted Merge

1. Both result sets are normalized to `[0, 1]` via min-max scaling
2. Final score = `(vector_weight * vector_score) + (fts_weight * fts_score)`
3. Default weights: 0.7 vector, 0.3 FTS
4. Ties broken by recency (`sequence_end` — higher is more recent)

## Embedding Pipeline

**File:** `memory/src/indexing.rs`

Two backends are supported:

### FastEmbed (Semantic)

- **Activation:** Feature flag `fastembed` + env `OXYDRA_MEMORY_EMBEDDING_BACKEND=fastembed`
- **Model:** BGE-small-en-v1.5 (384 dimensions)
- **Quality:** True semantic similarity (e.g., "cat" and "kitten" produce similar vectors)
- **Trade-off:** Requires ONNX runtime, larger binary, slower startup

### Blake3 Fallback (Deterministic)

- **Activation:** Default when fastembed is not available
- **Algorithm:** Blake3 hash of text → 32-byte seed → 64-dimensional normalized vector
- **Quality:** No semantic similarity — only exact or near-exact text matches produce similar vectors
- **Trade-off:** Zero dependencies, instant startup, deterministic output

The choice is transparent to the rest of the system — both backends produce embeddings that work with the same cosine similarity computation.

## Rolling Summarization

When the conversation history exceeds the token budget, the runtime triggers rolling summarization.

### Trigger Conditions

Both must be true:
- Unsummarized history length ≥ `min_turns` (default: 4 messages)
- Token utilization > `trigger_ratio` (default: 0.80)

### Process

1. **Target calculation** — determine how many messages to condense to reach `target_ratio` (default: 0.50)
2. **Condensation** — oldest unsummarized messages are formatted into a bulleted "Recent condensed turns" list
3. **Append to summary** — new condensation is appended to the existing rolling summary text
4. **Store** — summary state is written to `session_state` with updated `upper_sequence` and `epoch`
5. **Inject** — the full summary is injected as a system message: `"Rolling session summary: ..."`

### Race Safety (Optimistic Concurrency)

Summarization uses epoch-based compare-and-swap:

```rust
fn write_summary_state(session_id, state, expected_epoch) -> SummaryWriteOutcome {
    let current = read_current_epoch();
    if current != expected_epoch {
        return SummaryWriteOutcome::Applied(false); // stale, discard
    }
    // write new state with epoch = expected_epoch + 1
    SummaryWriteOutcome::Applied(true)
}
```

This prevents a slower summarization task from overwriting a newer one's results. If the epoch has advanced since the read, the stale summary is silently discarded.

## Session Operations

### List Sessions

Queries the `sessions` table ordered by `updated_at DESC`, providing a "recently active" view.

### Delete Session (Forget)

```sql
DELETE FROM sessions WHERE session_id = ?1;
```

All dependent data is cleaned up via `ON DELETE CASCADE`:
- `conversation_events` → deleted
- `session_state` → deleted
- `files` → deleted
- `chunks` → deleted (triggers clean up `chunks_vec` and `chunks_fts`)

### Restore Session

Rebuilds the `Context` by fetching all `conversation_events` for the session in sequence order, deserializing each `payload_json` back to a `Message`, then applying token budgeting to select which messages fit in the provider's context window.
