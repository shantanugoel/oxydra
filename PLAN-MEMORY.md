# Plan: Exposing Memory as an LLM-Callable Tool

## Problem Statement

The memory subsystem (libSQL-backed persistence, hybrid retrieval, rolling summarization) currently operates entirely behind the scenes — the runtime auto-stores messages, auto-retrieves relevant context, and auto-summarizes. The LLM itself **has no explicit tool** it can call to search its memory, save a note for later, update a previously saved fact, or delete something it was told to forget. This means:

1. The LLM cannot proactively decide to remember something important (e.g., user preferences, key decisions).
2. The LLM cannot search across its memory on its own initiative when it suspects relevant prior context exists.
3. The LLM cannot update or correct previously saved information.
4. The LLM cannot forget specific things when asked.
5. There is no user-facing way to ask "what do you remember about X?" and have the LLM deliberately query its memory store.

## Design Goals

- **Minimal surface area**: 4 tools that cover the essential CRUD + search operations.
- **User-scoped, not session-scoped**: Memory belongs to the **user**, not to a single conversation session. Notes saved on one channel (e.g., TUI) must be searchable from another channel (e.g., WhatsApp) for the same user. Never cross user boundaries.
- **Natural language notes**: No key-value structure. Notes are free-form text. The semantic + keyword search pipeline is designed for natural language and works best with it.
- **Note-level identifiers**: Each `memory_save` produces a note with a stable ID that can be used for `memory_update` and `memory_delete`.
- **Safe**: `memory_search` is `ReadOnly`; `memory_save`, `memory_update`, `memory_delete` are `SideEffecting`.
- **Composable with existing infra**: The tools delegate to the existing `Memory` and `MemoryRetrieval` trait methods — no new storage engine.
- **LLM-friendly schemas**: Clear descriptions that guide the LLM to use the tools correctly and proactively.

## Scoping Model: Per-User Memory Namespace

### Why not session-scoped?

Currently everything in the memory system is scoped by `session_id`. But for user memory (preferences, facts, decisions), the right boundary is the **user**, not the session. When multiple channels exist (Phase 14+), a user talking on WhatsApp and TUI should share the same memory space.

### Implementation: dedicated per-user memory session

Rather than changing the `Memory`/`MemoryRetrieval` traits or the database schema, we use a **well-known session ID convention** for each user's memory namespace:

```
memory:{user_id}
```

- All `memory_save`, `memory_update`, and `memory_delete` operations target `memory:{user_id}`.
- `memory_search` queries `memory:{user_id}` (the user's saved notes). It can optionally also search the current conversation session for things said earlier in the chat.
- This session persists across all conversation sessions and channels for that user.
- No schema changes required — the existing `sessions` table handles it.

### Future: cross-conversation-session search

When Phase 14 (External Channels + Identity Mapping) lands with proper `user_id ↔ session_id` mapping, `memory_search` can be extended to span all conversation sessions for a user. For now, it searches the user's memory namespace + optionally the current conversation session.

## Why Not More Tools?

### Session management tools (`list_sessions`, `read_session`, `delete_session`) — deferred

These were considered and rejected for now because:
- Session IDs are currently opaque strings with no title/summary metadata — a bare list isn't useful.
- Reading raw conversation history is brute-force when `memory_search` gives better, relevance-ranked results.
- The current session's recent history is already in the LLM's context window (auto-loaded by the runtime).
- Delete requires list to discover sessions first.

These belong in **Phase 18 (Session Lifecycle Controls)** where proper session metadata, naming, and management are planned.

### `memory_list` / `memory_read` for notes — not needed

`memory_search` with a broad query effectively lists/reads relevant notes. The semantic search engine *is* the index. Adding separate list/read tools would duplicate what search already does, but worse (no ranking, potentially dumping everything).

### Key-value structure — not needed

Embedding models are trained on natural language. `"User's name is Shantanu"` produces a richer, more useful embedding than `key: "user_name", value: "Shantanu"`. The sentence captures semantic relationships that a KV pair doesn't. Natural language notes also avoid fragile key naming (is it `user_name`, `name`, `username`?) and let approximate matching just work.

The note ID handles update/delete — it serves the same "stable handle" purpose that a key would, without constraining the content format.

## Proposed Tool Set

### 1. `memory_search` — Semantic + keyword search over user memory

**Purpose**: Let the LLM explicitly search its saved notes and (optionally) current conversation for relevant information.

| Property | Value |
|----------|-------|
| Safety tier | `ReadOnly` |
| Timeout | 15s |

**Parameters**:
| Name | Type | Required | Description |
|------|------|----------|-------------|
| `query` | string | yes | Natural language search query |
| `top_k` | integer | no | Max results to return (default: 5, max: 20) |
| `include_conversation` | boolean | no | Also search the current conversation session (default: false) |

**Returns**: JSON array of matching notes/chunks with `note_id`, `text`, `score`, and `source` (indicating whether the result came from user memory or the conversation).

**Implementation**: Delegates to `MemoryRetrieval::hybrid_query()` against the `memory:{user_id}` session. If `include_conversation` is true, also queries the current conversation session and merges results. Uses configured vector/FTS weights from `RuntimeLimits::retrieval`.

### 2. `memory_save` — Save a note to user memory

**Purpose**: Let the LLM proactively store important information (user preferences, project decisions, key facts) that should persist across all conversations.

| Property | Value |
|----------|-------|
| Safety tier | `SideEffecting` |
| Timeout | 10s |

**Parameters**:
| Name | Type | Required | Description |
|------|------|----------|-------------|
| `content` | string | yes | Natural language text to save (fact, preference, decision, etc.) |

**Returns**: JSON object with `note_id` (stable identifier for future update/delete) and confirmation message.

**Implementation**:
1. Generate a note ID (UUID).
2. Store via `Memory::store()` against the `memory:{user_id}` session with a synthetic message payload that includes the note ID in metadata.
3. The existing indexing pipeline automatically extracts text, chunks it, embeds it, and makes it searchable via `hybrid_query`.
4. The note ID is stored in the chunk metadata so it can be resolved from search results and used for update/delete.

### 3. `memory_update` — Update an existing note

**Purpose**: Let the LLM modify a previously saved note (e.g., "call me SG instead of Shantanu").

| Property | Value |
|----------|-------|
| Safety tier | `SideEffecting` |
| Timeout | 10s |

**Parameters**:
| Name | Type | Required | Description |
|------|------|----------|-------------|
| `note_id` | string | yes | ID of the note to update (from `memory_search` or `memory_save` results) |
| `content` | string | yes | New content to replace the existing note |

**Returns**: Confirmation message with the note ID.

**Typical LLM workflow**:
1. `memory_search("user's name")` → finds note `note-abc123` with "User's name is Shantanu"
2. `memory_update("note-abc123", "User prefers to be called SG")`

**Implementation**:
1. Find all chunks belonging to `note_id` in the `memory:{user_id}` session (via metadata query).
2. Delete the old chunks (and their embeddings/FTS entries).
3. Store the new content via the same path as `memory_save`, reusing the same note ID.
4. This is effectively delete-then-save with the same note ID, keeping the identifier stable.

### 4. `memory_delete` — Delete a note from user memory

**Purpose**: Let the LLM forget specific information when asked (e.g., "forget that I like chocolates").

| Property | Value |
|----------|-------|
| Safety tier | `SideEffecting` |
| Timeout | 10s |

**Parameters**:
| Name | Type | Required | Description |
|------|------|----------|-------------|
| `note_id` | string | yes | ID of the note to delete (from `memory_search` results) |

**Returns**: Confirmation message.

**Typical LLM workflow**:
1. `memory_search("chocolates")` → finds note `note-xyz789` with "User likes chocolates"
2. `memory_delete("note-xyz789")`

**Implementation**:
1. Find all chunks belonging to `note_id` in the `memory:{user_id}` session.
2. Delete the chunks from `chunks`, `chunks_vec`, and `chunks_fts` (FTS cleanup happens via existing triggers).
3. Delete the corresponding conversation event.
4. Return confirmation or error if note not found.

---

## Implementation Plan

### Step 1: Add note-level storage support to the memory layer

**Crates**: `types`, `memory`

The existing storage is chunk-level. Notes are a logical grouping of one or more chunks. We need:

1. **Add a `note_id` field to chunk metadata**: When `memory_save` stores content, the resulting chunks carry the note ID in their `metadata_json`. This is how `memory_update`/`memory_delete` find all chunks belonging to a note.

2. **Add a query-by-note-ID helper on `LibsqlMemory`**: A method to find all chunk IDs for a given `note_id` within a session. This is a simple SQL query on `metadata_json` (SQLite JSON functions: `json_extract(metadata_json, '$.note_id') = ?`).

3. **Add a delete-chunks-by-IDs method**: Delete specific chunks and their embeddings from `chunks` + `chunks_vec`. The FTS5 triggers already handle `chunks_fts` cleanup.

4. **Add a delete-event-by-sequence method**: Remove the synthetic conversation event created by `memory_save`, so deleted notes don't reappear in recalls.

No new tables or migrations required — we use existing tables with metadata conventions.

### Step 2: Create memory tool structs in the `tools` crate

**Crates**: `tools`

**Changes**:
1. Create a new module `crates/tools/src/memory_tools.rs` containing the four tool structs:
   - `MemorySearchTool`
   - `MemorySaveTool`
   - `MemoryUpdateTool`
   - `MemoryDeleteTool`

2. Each tool struct holds:
   - `Arc<dyn MemoryRetrieval>` (since `MemoryRetrieval: Memory`, this covers both traits)
   - `user_id: Arc<Mutex<Option<String>>>` — shared holder for current user ID
   - `current_session_id: Arc<Mutex<Option<String>>>` — shared holder for current conversation session ID (needed by `memory_search` when `include_conversation` is true)
   - Configured retrieval settings from `RetrievalConfig`

3. Each struct implements the `Tool` trait with appropriate `schema()`, `execute()`, `timeout()`, and `safety_tier()`.

4. Export tool name constants:
   ```rust
   pub const MEMORY_SEARCH_TOOL_NAME: &str = "memory_search";
   pub const MEMORY_SAVE_TOOL_NAME: &str = "memory_save";
   pub const MEMORY_UPDATE_TOOL_NAME: &str = "memory_update";
   pub const MEMORY_DELETE_TOOL_NAME: &str = "memory_delete";
   ```

5. Helper function to compute the user memory session ID:
   ```rust
   fn user_memory_session_id(user_id: &str) -> String {
       format!("memory:{user_id}")
   }
   ```

### Step 3: Implement `memory_save` storage path

**Crates**: `tools`, `memory`

**Changes**:
1. Generate a note ID: `format!("note-{}", Uuid::new_v4())`.
2. Store via `Memory::store()` against the `memory:{user_id}` session with:
   ```rust
   Message {
       role: MessageRole::User,  // Use User role so the indexing pipeline extracts it
       content: Some(content),
       tool_calls: Vec::new(),
       tool_call_id: Some(note_id.clone()),  // Carry note_id as tool_call_id for tracing
   }
   ```
3. The payload stored also includes the note ID in a wrapper:
   ```rust
   json!({
       "role": "user",
       "content": content,
       "tool_call_id": note_id,
       "metadata": { "note_id": note_id, "source": "memory_save" }
   })
   ```
   The indexing pipeline's `prepare_index_document` will pick up the content, chunk it, embed it, and store the chunks. We need to ensure the `note_id` flows into the chunk's `metadata_json` so it can be queried later.

4. **Modify `prepare_index_document`** (or add a wrapper) to accept optional extra metadata fields that get merged into each chunk's `metadata_json`. This way the `note_id` propagates from the store call to every chunk.

5. Return the note ID to the LLM.

### Step 4: Implement `memory_update` and `memory_delete`

**Crates**: `tools`, `memory`

**`memory_delete` implementation**:
1. Compute `memory:{user_id}` session ID.
2. Query chunks where `json_extract(metadata_json, '$.note_id') = ?` for the given session.
3. Delete those chunks from `chunks` (cascade handles `chunks_vec`; triggers handle `chunks_fts`).
4. Find and delete the corresponding conversation event (by matching `note_id` in the payload).
5. Return confirmation or "note not found" error.

**`memory_update` implementation**:
1. Execute `memory_delete` for the note ID.
2. Execute `memory_save` with the new content, reusing the same note ID (don't generate a new UUID).
3. Return confirmation with the same note ID.

### Step 5: Implement `memory_search`

**Crates**: `tools`

**Implementation**:
1. Compute `memory:{user_id}` session ID.
2. Call `MemoryRetrieval::hybrid_query()` against the user memory session with the query text, configured weights, and `top_k`.
3. If `include_conversation` is true and a current conversation session ID is available, also query that session and merge results (re-rank by score, deduplicate by `chunk_id`).
4. Format results as JSON array with `note_id` (extracted from chunk metadata), `text`, `score`, and `source` ("user_memory" or "conversation").
5. Return the formatted results.

### Step 6: Plumb `user_id` and `session_id` into the tool layer

**Crates**: `runtime`, `tools`, `gateway`

The memory tools need two pieces of runtime context:
- `user_id` — to compute the `memory:{user_id}` session
- Current conversation `session_id` — for `include_conversation` in search

**Approach**: Shared `Arc<Mutex<Option<String>>>` holders (one for user_id, one for session_id).

1. The `AgentRuntime` or `RuntimeGatewayTurnRunner` creates these holders at construction time.
2. They are passed to the memory tools during registration.
3. Before each turn, the gateway turn runner sets both values:
   - `user_id` from `UserSessionState.user_id`
   - `session_id` from `UserSessionState.runtime_session_id`
4. Memory tools read these values during `execute()`.

**Why `user_id` is already available**: The gateway has `UserSessionState.user_id` from the Hello handshake. It's plumbed through `RuntimeGatewayTurnRunner::run_turn()` which receives `runtime_session_id`. We need to also pass `user_id` through this path.

### Step 7: Wire memory tools into the ToolRegistry bootstrap

**Crates**: `tools`

**Changes**:
1. Add a `register_memory_tools()` function that takes:
   - `Arc<dyn MemoryRetrieval>`
   - `Arc<Mutex<Option<String>>>` for user_id
   - `Arc<Mutex<Option<String>>>` for session_id
   - `RetrievalConfig`
2. The function creates all four tools and registers them in the `ToolRegistry`.
3. If `MemoryRetrieval` is not available, memory tools are not registered (they require the retrieval layer for search).

### Step 8: Update bootstrap and gateway wiring

**Crates**: `tools`, `runner`, `gateway`

**Changes**:
1. Extend `bootstrap_runtime_tools()` to accept optional `Arc<dyn MemoryRetrieval>` and the context holders.
2. In `crates/runner/src/bin/oxydra-vm.rs`:
   - Build the memory backend as `Arc<dyn MemoryRetrieval>` (not just `Arc<dyn Memory>`) when retrieval is available.
   - Create the shared user_id and session_id holders.
   - Pass them to `bootstrap_runtime_tools()`.
   - Pass the same holders to `RuntimeGatewayTurnRunner`.
3. In `RuntimeGatewayTurnRunner::run_turn()`:
   - Set `user_id` in the holder from the session state.
   - Set `session_id` in the holder from `runtime_session_id`.
4. Ensure the user memory session (`memory:{user_id}`) is lazily created on first `memory_save` — no upfront provisioning needed (the `store()` method already upserts sessions).

### Step 9: Add tool descriptions that guide LLM behavior

**Crates**: `tools`

Tool descriptions are critical for the LLM to use tools correctly and proactively. Each tool's `schema()` description:

- **`memory_search`**: "Search your persistent memory for relevant information. Use this when you need to recall user preferences, past decisions, stored facts, or anything previously saved. Results are ranked by relevance using semantic similarity and keyword matching. By default searches only your saved notes; set include_conversation to true to also search the current conversation history."

- **`memory_save`**: "Save an important piece of information to your persistent memory as a natural language note. Use this proactively whenever the user shares preferences, makes decisions, states facts, or tells you something worth remembering across conversations. Saved notes persist across all sessions and channels. Returns a note_id for future reference."

- **`memory_update`**: "Update a previously saved note with new content. Use this when the user corrects or changes previously stored information (e.g., a new preferred name, an updated preference). First use memory_search to find the note and its note_id, then call this with the note_id and the new content."

- **`memory_delete`**: "Delete a previously saved note from memory. Use this when the user asks you to forget specific information. First use memory_search to find the note and its note_id, then call this with the note_id to remove it."

### Step 10: Tests

**Crates**: `tools`, `memory`, `runtime`

**Test categories**:

1. **Note lifecycle unit tests** (in `memory`):
   - Store a note via the memory_save path, verify chunks carry `note_id` in metadata
   - Query chunks by `note_id`, verify all chunks are found
   - Delete chunks by `note_id`, verify cleanup of chunks + embeddings + FTS
   - Update a note: verify old chunks removed, new chunks created with same note_id

2. **Memory tool unit tests** (in `tools`):
   - Schema validation for all four tools (parameter names, types, required fields)
   - `memory_save` returns a valid note_id
   - `memory_search` returns results with note_id, text, score
   - `memory_search` with `include_conversation=true` merges results from both sessions
   - `memory_update` with valid note_id succeeds
   - `memory_update` with nonexistent note_id returns error
   - `memory_delete` with valid note_id succeeds
   - `memory_delete` with nonexistent note_id returns error
   - All tools return clear errors when memory is not configured

3. **User scoping tests**:
   - Notes saved by user A are not visible to user B
   - `memory:{user_a}` and `memory:{user_b}` are fully isolated
   - Search never crosses user boundaries

4. **Integration tests** (in `runtime`):
   - Full save → search → update → search → delete → search lifecycle
   - Memory tools appear in tool schemas sent to the provider
   - Memory tools work within the existing execution pipeline (safety tier gating, timeout, output truncation, scrubbing)
   - Context holders correctly set before tool execution

5. **Roundtrip tests**:
   - Save "User's name is Shantanu" → search "name" → finds it
   - Update to "User prefers SG" → search "name" → finds updated version, old version gone
   - Delete → search "name" → no results

### Step 11: Update documentation

**Files**: `docs/guidebook/04-tool-system.md`, `docs/guidebook/06-memory-and-retrieval.md`, `docs/guidebook/15-progressive-build-plan.md`

1. Add memory tools to the tool catalog in Chapter 4, with descriptions and safety tiers.
2. Add a "Memory Tools" section to Chapter 6 explaining:
   - The user-scoped memory namespace (`memory:{user_id}`)
   - The note abstraction (natural language, note-level IDs)
   - How the tools complement automatic context retrieval
   - The LLM's typical workflow (search → update/delete)
3. Update Phase 15 to reflect this work as an enhancement to Phase 9, noting the relationship with Phase 14 (cross-channel user identity) and Phase 18 (session lifecycle controls).

---

## Dependency Graph

```
Step 1 (note-level storage)
    │
    ├──► Step 3 (memory_save path)
    │        │
    │        ▼
    │    Step 4 (memory_update + memory_delete)
    │
    ├──► Step 5 (memory_search)
    │
    ▼
Step 2 (tool structs) ◄── Step 9 (tool descriptions)
    │
    ▼
Step 7 (registry wiring) ◄── Step 6 (user_id + session_id plumbing)
    │
    ▼
Step 8 (bootstrap + gateway wiring)
    │
    ▼
Step 10 (tests)
    │
    ▼
Step 11 (docs)
```

Steps 1 and 6 can be done in parallel (no dependencies between them).
Steps 3, 4, 5 depend on Step 1.
Step 2 depends on Steps 3, 4, 5 (needs the storage path defined) but the struct shells can be created early.
Step 7 depends on Steps 2 and 6.
Step 8 depends on Step 7.
Step 9 can be done any time (just descriptions).
Step 10 depends on Steps 1–8.

## Key Design Decisions (Resolved)

| Decision | Resolution | Rationale |
|----------|-----------|-----------|
| How many tools? | 4 (`search`, `save`, `update`, `delete`) | Minimal CRUD + search; session management deferred to Phase 18 |
| Scoping? | Per-user (`memory:{user_id}`) | Memory belongs to the user, not the session; survives across channels |
| Structured (KV) or unstructured? | Unstructured natural language | Embeddings work best on natural language; KV adds fragile key naming |
| Note identifiers? | UUID-based `note_id` in chunk metadata | Stable handle for update/delete without requiring keys |
| Where to store? | `Memory::store()` with synthetic messages | Uses existing indexing pipeline; avoids blocked `upsert_chunks` gap |
| Session tools? | Deferred to Phase 18 | Opaque session IDs without metadata aren't useful yet |

## Future Extensions

1. **Cross-conversation search** (Phase 14): When user-session mapping exists, `memory_search` can span all conversation sessions for a user, not just the notes namespace.
2. **Session management tools** (Phase 18): `memory_list_sessions`, `memory_read_session`, `memory_delete_session` with proper session metadata (titles, summaries, timestamps).
3. **Bulk note operations**: `memory_export` / `memory_import` for backup/migration.
4. **Category filtering**: Add optional `category` parameter to `memory_save` and `memory_search` for organization without imposing structure on the content itself. Stored in chunk metadata, used as an optional filter in search queries.
