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
    async fn hybrid_query(&self, request: MemoryHybridQueryRequest) -> Result<Vec<MemoryHybridQueryResult>, MemoryError>;
    async fn read_summary_state(&self, session_id: &str) -> Result<Option<MemorySummaryState>, MemoryError>;
    async fn write_summary_state(&self, session_id: &str, state: &MemorySummaryState, expected_epoch: u64) -> Result<SummaryWriteOutcome, MemoryError>;
    async fn store_note(&self, request: MemoryNoteStoreRequest) -> Result<(), MemoryError>;
    async fn delete_note(&self, session_id: &str, note_id: &str) -> Result<bool, MemoryError>;
    async fn read_scratchpad(&self, request: MemoryScratchpadReadRequest) -> Result<Option<MemoryScratchpadState>, MemoryError>;
    async fn write_scratchpad(&self, request: MemoryScratchpadWriteRequest) -> Result<MemoryScratchpadWriteResult, MemoryError>;
    async fn clear_scratchpad(&self, request: MemoryScratchpadClearRequest) -> Result<bool, MemoryError>;
}
```

Key request types:

- **`MemoryHybridQueryRequest`** — carries `session_id`, `query`, optional `tags` (for DB-level filtering), optional `query_embedding` (pre-computed), `top_k`, `vector_weight`, and `fts_weight`.
- **`MemoryNoteStoreRequest`** — carries `session_id`, `note_id`, `content`, optional `source`, and optional `tags`. Timestamps are backend-owned.

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

#### `schedules`

```sql
CREATE TABLE schedules (
    schedule_id          TEXT PRIMARY KEY,
    user_id              TEXT NOT NULL,
    name                 TEXT,
    goal                 TEXT NOT NULL,
    cadence_json         TEXT NOT NULL,
    notification_policy  TEXT NOT NULL DEFAULT 'always',
    status               TEXT NOT NULL DEFAULT 'active',
    created_at           TEXT NOT NULL,
    updated_at           TEXT NOT NULL,
    next_run_at          TEXT,
    last_run_at          TEXT,
    last_run_status      TEXT,
    consecutive_failures INTEGER NOT NULL DEFAULT 0,
    channel_id           TEXT,
    channel_context_id   TEXT
);
```

Stores durable schedule definitions. The `cadence_json` field holds a JSON-serialized `ScheduleCadence` (cron, once, or interval). `channel_id` and `channel_context_id` capture the originating channel at creation time for notification routing. Indexed on `user_id`, `(next_run_at WHERE status='active')`, `(user_id, status)`, and `(user_id, name)`.

#### `schedule_runs`

```sql
CREATE TABLE schedule_runs (
    run_id         TEXT PRIMARY KEY,
    schedule_id    TEXT NOT NULL,
    started_at     TEXT NOT NULL,
    finished_at    TEXT NOT NULL,
    status         TEXT NOT NULL,
    output_summary TEXT,
    output         TEXT,
    turn_count     INTEGER NOT NULL DEFAULT 0,
    cost           REAL NOT NULL DEFAULT 0.0,
    notified       INTEGER NOT NULL DEFAULT 0,
    FOREIGN KEY (schedule_id) REFERENCES schedules(schedule_id) ON DELETE CASCADE
);
```

Audit trail for scheduled executions. Each run records timing, outcome status, truncated output summary (first 500 characters), full output in the `output` column, and whether the user was notified. Indexed on `(schedule_id, started_at DESC)` for efficient history queries. Cascading delete ensures run history is removed when the parent schedule is deleted.

## SchedulerStore

**File:** `memory/src/scheduler_store.rs`

The `SchedulerStore` trait provides the persistence layer for the scheduler system:

```rust
#[async_trait]
pub trait SchedulerStore: Send + Sync {
    async fn create_schedule(&self, def: &ScheduleDefinition) -> Result<(), SchedulerError>;
    async fn get_schedule(&self, schedule_id: &str) -> Result<ScheduleDefinition, SchedulerError>;
    async fn search_schedules(&self, user_id: &str, filters: &ScheduleSearchFilters) -> Result<ScheduleSearchResult, SchedulerError>;
    async fn count_schedules(&self, user_id: &str) -> Result<usize, SchedulerError>;
    async fn delete_schedule(&self, schedule_id: &str) -> Result<bool, SchedulerError>;
    async fn update_schedule(&self, schedule_id: &str, patch: &SchedulePatch) -> Result<ScheduleDefinition, SchedulerError>;
    async fn due_schedules(&self, now: &str, limit: usize) -> Result<Vec<ScheduleDefinition>, SchedulerError>;
    async fn record_run_and_reschedule(&self, schedule_id: &str, run: &ScheduleRunRecord, next_run_at: Option<String>, new_status: Option<ScheduleStatus>) -> Result<(), SchedulerError>;
    async fn prune_run_history(&self, schedule_id: &str, keep: usize) -> Result<(), SchedulerError>;
    async fn get_run_history(&self, schedule_id: &str, limit: usize) -> Result<Vec<ScheduleRunRecord>, SchedulerError>;
    async fn get_run_by_id(&self, run_id: &str) -> Result<ScheduleRunRecord, SchedulerError>;
}
```

The `LibsqlSchedulerStore` implementation uses a dedicated libSQL `Connection` obtained from the memory backend via `LibsqlMemory::connect_for_scheduler()`. The `due_schedules` query uses the indexed `next_run_at` column for efficient polling. The `search_schedules` method builds dynamic WHERE clauses from filters and runs two queries (count + fetch) for pagination support.

### Cadence Evaluation

**File:** `memory/src/cadence.rs`

The cadence module provides next-run computation and validation:

- `next_run_for_cadence(cadence, after)` — computes the next fire time after a given timestamp. Uses the `cron` crate for cron expressions (with timezone support via `chrono-tz`), direct timestamp comparison for one-shot, and `chrono::Duration` for intervals.
- `validate_cadence(cadence, min_interval_secs)` — validates a cadence at creation/edit time. Checks that one-shot times are in the future, cron expressions are valid with sufficient intervals, and interval durations meet the minimum.
- `parse_cadence(type, value, timezone)` — constructs a `ScheduleCadence` from tool arguments.
- `format_in_timezone(utc_timestamp, cadence)` — formats a UTC timestamp in the schedule's timezone for human-friendly display.

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
4. **Tag normalization** — if tags are provided, they are trimmed, lowercased, deduplicated, and capped
5. **Timestamp assignment** — backend-owned `created_at` and `updated_at` timestamps are set automatically (not caller-supplied)
6. **Indexing with metadata** — the note content is chunked and embedded using the same pipeline as conversation messages, with `note_id`, `source`, `tags`, `created_at`, `updated_at`, and `schema_version` merged into each chunk's metadata via `prepare_index_document_with_extra_metadata`

The `note_id` (format: `note-{uuid}`) is generated by the calling tool and stored in chunk metadata, enabling subsequent lookups by note identifier. The structured `MemoryNoteStoreRequest` carries all note fields including optional tags and source.

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

### Vector Search (Native libSQL)

1. Generate an embedding for the query text using the configured embedding backend
2. Use `vector_top_k('chunks_vec_idx', query_vector, k)` to perform fast ANN retrieval via the native libSQL vector index
3. Return top-N candidates by cosine similarity

The native vector index (`libsql_vector_idx`) is created during schema migration and maintained automatically by libSQL. This eliminates the need for in-memory cosine similarity computation over all stored embeddings.

### Full-Text Search (FTS5)

```sql
SELECT ..., -bm25(chunks_fts) AS score
FROM chunks_fts
INNER JOIN chunks c ON c.rowid = chunks_fts.rowid
WHERE chunks_fts MATCH ?1 AND c.session_id = ?2
ORDER BY score DESC LIMIT ?3;
```

### Tag Filtering

When tags are provided in the query request, both vector and FTS results are filtered at the database level. Each requested tag must be present in the chunk's `metadata_json.tags` array. This uses `json_each()` for efficient tag matching.

### Weighted Merge

1. Both result sets are normalized to `[0, 1]` via min-max scaling
2. Final score = `(vector_weight * vector_score) + (fts_weight * fts_score)`
3. Default weights: 0.7 vector, 0.3 FTS
4. Ties broken by recency (`sequence_end` — higher is more recent)

## Embedding Pipeline

**File:** `memory/src/indexing.rs`

Two backends are supported, configured via `memory.embedding_backend` in `AgentConfig`:

### Model2vec (Semantic) — Default

- **Activation:** `embedding_backend = "model2vec"` (default)
- **Library:** [model2vec-rs](https://github.com/MinishLab/model2vec-rs) — Rust-native static word embedding
- **Models:** Configurable via `model2vec_model`:
  - `potion_32m` (default) — `minishlab/potion-base-32M`, higher semantic quality (~32 MB)
  - `potion_8m` — `minishlab/potion-base-8M`, lighter footprint (~8 MB)
- **Native dimensions:** 256, automatically padded to 512 to match the vector index
- **Quality:** True semantic similarity — paraphrases and related concepts cluster naturally
- **Trade-off:** Requires model download on first use; moderate startup time

### Deterministic (Hash-Based Fallback)

- **Activation:** `embedding_backend = "deterministic"`
- **Algorithm:** Blake3 hash of text → 32-byte seed → 64-dimensional normalized vector → padded to 512
- **Quality:** No semantic similarity — only exact or near-exact text matches produce similar vectors. Same text always produces the same vector.
- **Trade-off:** Zero external model dependency, instant startup, fully deterministic output
- **Use cases:** Minimal builds, deterministic test behavior, constrained environments where model loading is undesirable

The backend choice is transparent to the retrieval layer — both produce 512-dimensional vectors compatible with the same cosine similarity computation and native libSQL vector index.

### Vector Index and Retrieval

All embeddings are stored in `chunks_vec` using libSQL's native `vector32()` function. Retrieval uses libSQL's native vector index (`libsql_vector_idx`) with `vector_top_k` for fast approximate nearest neighbor (ANN) search. There is no Rust-side fallback retrieval path — if native vector capability is unavailable, initialization fails fast with an explicit error.

## Note Metadata

Each stored note carries structured metadata in its chunk records:

| Field | Source | Purpose |
|-------|--------|---------|
| `note_id` | Generated (`note-{uuid}`) | Unique note identity for update/delete |
| `source` | Tool name (e.g., `memory_save`) | Provenance tracking |
| `tags` | User-supplied, normalized | Categorization and DB-level filtering |
| `created_at` | Backend-owned timestamp | Creation time (set once) |
| `updated_at` | Backend-owned timestamp | Last modification time |
| `schema_version` | Constant (`1`) | Future migration support |

**Tag normalization:** Tags supplied by the agent are trimmed, lowercased, deduplicated, and capped at 16 entries (each max 64 characters). This normalization happens in the memory backend before persistence.

**Tag filtering:** When `memory_search` includes tags, filtering is performed at the database level in the retrieval query path, improving precision and performance compared to broad post-filtering.

## Working-Memory Scratchpad

**File:** `memory/src/lib.rs`, `tools/src/scratchpad_tools.rs`

The scratchpad provides session-scoped working memory for tracking active sub-tasks during long autonomous runs:

- **Storage:** Stored in `session_state` as a JSON field, keyed by session ID
- **Scope:** Session-scoped only — data does not persist across sessions unless explicitly saved to durable memory
- **Limits:** Maximum 32 items, each up to 240 characters
- **Write semantics:** Each write replaces the entire scratchpad (not append-only)
- **Tools:** `scratchpad_read`, `scratchpad_write`, `scratchpad_clear`
- **Purpose:** Prevents drift during long multi-step execution by giving the agent an explicit place to track current goals, completed steps, and pending work

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
