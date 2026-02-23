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

---

## Post-Implementation Review

Review of commit `88e28be` against this plan. Performed after the initial implementation landed.

### Overall Assessment

The implementation is **faithful to the plan** in its core design: 4 tools, per-user `memory:{user_id}` scoping, natural language notes with UUID-based `note_id`, correct safety tiers and timeouts, proper delegation to existing traits, shared context holders, conditional registration, and documentation updates. All memory and tools tests pass (24 + 31). The single failing test (`run_session_executes_readonly_tools_in_parallel`) is pre-existing and unrelated.

### Findings

#### F1. Race Condition on `MemoryToolContext` — HIGH

`MemoryToolContext` uses `Arc<Mutex<Option<String>>>` for `user_id` and `session_id`. These are set in `RuntimeGatewayTurnRunner::run_turn()` and read during tool execution. However, the `RuntimeGatewayTurnRunner` (and therefore the `MemoryToolContext`) is a **single instance shared across all users/sessions**. The gateway dispatches turns via `tokio::spawn`, meaning concurrent turns for different users run in parallel.

The race:
1. Turn for User A starts: `run_turn()` sets `user_id = "alice"`.
2. Turn for User B starts before Alice's memory tool executes: `run_turn()` overwrites `user_id = "bob"`.
3. Alice's `memory_save` reads `user_id = "bob"` → saves into Bob's namespace.

This is a **cross-user data leak** — a memory tool could operate on the wrong user's namespace.

**Resolution plan:**

Option A (minimal, recommended): Pass `user_id` and `session_id` through the `Tool::execute()` interface as part of an execution context. This requires adding an optional context parameter to the `Tool` trait (or a separate `ToolExecutionContext` that wraps per-invocation state). This is the cleanest fix but requires a trait change.

Option B (no trait change): Replace the single shared `MemoryToolContext` with a per-turn context map keyed by some turn identifier. The gateway would generate a turn ID, store the context in a `DashMap<TurnId, (String, String)>`, and pass the turn ID through the existing `args` JSON (as an injected field). Memory tools would extract the turn ID from args and look up the context. This is hacky but avoids a trait change.

Option C (intermediate): Use `tokio::task_local!` to store the user_id/session_id. The gateway sets the task-local before spawning the turn future. Memory tools read from the task-local during execution. This avoids a trait change and is thread-safe, but task-locals don't propagate across `tokio::spawn` boundaries without explicit wrapping.

**Recommended approach:** Option A. The `Tool` trait should accept an execution context. This is a natural evolution — tools like memory tools are inherently context-dependent, and future tools (e.g., session management, user preferences) will need the same context. The change is:
```rust
// New struct in types
pub struct ToolExecutionContext {
    pub user_id: Option<String>,
    pub session_id: Option<String>,
}

// Tool trait change
async fn execute(&self, args: &str, context: &ToolExecutionContext) -> Result<String, ToolError>;
```

All existing tools ignore the context (they don't need it). Memory tools read `user_id`/`session_id` from it. The `MemoryToolContext` shared holder is removed entirely.

#### F2. Missing Test for `include_conversation=true` — MEDIUM

The plan (Step 10) explicitly calls for a test: "`memory_search` with `include_conversation=true` merges results from both sessions." No such test exists. All search tests use the default (`include_conversation=false`). This leaves the merge/dedup/re-sort logic in `MemorySearchTool::execute()` untested.

**Resolution:** Add a test that:
1. Stores a note in the user memory namespace.
2. Stores a conversation event in a separate session (the current conversation session ID).
3. Calls `memory_search` with `include_conversation=true`.
4. Verifies results contain entries from both sources with correct `source` labels.
5. Verifies deduplication works if the same chunk appears in both sessions.

#### F3. Missing Runtime Integration Tests — MEDIUM

The plan (Step 10) calls for integration tests in the `runtime` crate verifying:
- Memory tools appear in tool schemas sent to the provider.
- Memory tools work within the existing execution pipeline (safety tier gating, timeout, output truncation, scrubbing).
- Context holders are correctly set before tool execution.

The only change to `crates/runtime/src/tests.rs` is adding `store_note`/`delete_note` stubs to `RecordingMemory`. No actual integration tests exist. These are important for ensuring memory tools behave correctly when invoked by the LLM through the full turn loop.

**Resolution:** Add at least one integration test that wires up memory tools in a `MockProvider`-driven turn and verifies the tool executes and returns results through the standard pipeline.

#### F4. Documentation Discrepancy: `max_results` vs `top_k` — LOW

`docs/guidebook/04-tool-system.md` (line 145) documents the `memory_search` parameter as `max_results` (default 10). The actual implementation uses `top_k` (default 5, max 20). The `include_conversation` parameter is also not mentioned in the table.

**Resolution:** Fix the table row to:
```
| `memory_search` | `query` (required), `top_k` (optional, default 5, max 20), `include_conversation` (optional, default false) | JSON array of `{text, source, score, note_id}` results |
```

#### F5. `include_conversation` Silently Swallows Errors — LOW

When `include_conversation=true`, failures in `resolve_session_id` or `hybrid_query` for the conversation session are silently ignored via chained `if let Ok(...)`:

```rust
if request.include_conversation.unwrap_or(false)
    && let Ok(conversation_session) = resolve_session_id(...)
    && let Ok(conversation_results) = self.memory.hybrid_query(...).await
```

This is arguably correct (graceful degradation — return memory results even if the conversation search fails), but the user gets no indication that part of the search was skipped. At minimum, a `tracing::warn!` when the conversation query fails would aid debugging.

**Resolution:** Add `tracing::warn!` on either failure case. This doesn't change behavior but provides observability.

#### F6. No `tracing` Instrumentation on Memory Tools — LOW

The memory tool implementations have zero `tracing` calls despite the rest of the codebase using tracing extensively. Save, delete, update, and search operations should log at `info` or `debug` level for operational visibility.

**Resolution:** Add `tracing::info!` for save/update/delete operations (including `note_id` and `user_id`), and `tracing::debug!` for search (including query and result count).

#### F7. `saturating_add(1)` in `next_event_sequence` Masks Overflow — VERY LOW

If the sequence ever reaches `u64::MAX`, `saturating_add(1)` returns `u64::MAX` again, causing a duplicate sequence and a constraint violation at the database level. This is practically impossible (would require 18 quintillion notes) but `checked_add` with an explicit error message would be more correct.

**Resolution:** Replace `saturating_add(1)` with `checked_add(1).ok_or_else(|| query_error("sequence overflow"))`.

#### F8. `with_memory()` vs `with_memory_retrieval()` Coexistence — VERY LOW

`oxydra-vm.rs` was changed from `with_memory()` to `with_memory_retrieval()`. The old `with_memory()` still exists and sets `memory_retrieval = None`. If anyone calls `with_memory()` in the future, the runtime's internal retrieval pipeline would lose its `MemoryRetrieval` reference even though memory tools have their own `Arc<dyn MemoryRetrieval>` from bootstrap. The two methods serve overlapping purposes and their coexistence is a footgun.

**Resolution:** Consider deprecating `with_memory()` or making it call `with_memory_retrieval()` internally when the `Arc<dyn Memory>` also implements `MemoryRetrieval` (which it always does since `MemoryRetrieval: Memory`).

#### F9. Empty `note_id` for Conversation Results — INFORMATIONAL

When `include_conversation=true`, conversation-sourced results return `"note_id": ""`. The LLM could attempt to pass this empty string to `memory_update` or `memory_delete`, which would fail with "not found." The behavior is correct (conversation messages are not deletable via memory tools), but the LLM tool descriptions don't explicitly say conversation results cannot be updated/deleted. The LLM might attempt it and get confused.

**Resolution:** Consider adding a sentence to the `memory_search` description: "Results with an empty `note_id` (from conversation search) cannot be updated or deleted via memory tools."

#### F10. Large Notes Produce Multiple Search Results — INFORMATIONAL

If a note is large enough to be split into multiple chunks, `memory_search` may return multiple results from the same note as separate entries. This is correct behavior and the `note_id` in each result allows the LLM to identify they belong to the same note. However, for a better UX, a future enhancement could group results by `note_id` and return consolidated entries.

#### F11. Unrelated Change in Commit — COSMETIC

The commit includes a `saturating_add(1)` change in `crates/tui/src/ui_model.rs` (scroll_down). This is unrelated to memory tools. Not a bug — just commit hygiene.

### Recommended Action Plan (Priority Order)

| Priority | Finding | Action | Effort |
|----------|---------|--------|--------|
| **HIGH** | F1: Race condition on MemoryToolContext | Add `ToolExecutionContext` to `Tool::execute()` trait; remove shared mutable holders | Medium — trait change touches all tool implementations |
| **MEDIUM** | F2: Missing `include_conversation` test | Add integration test for merged search results | Small |
| **MEDIUM** | F3: Missing runtime integration tests | Add turn-loop test with memory tools | Small-Medium |
| **LOW** | F4: Doc `max_results` vs `top_k` | Fix parameter name/default in `04-tool-system.md` | Trivial |
| **LOW** | F5: Silent error swallowing | Add `tracing::warn!` on conversation search failure | Trivial |
| **LOW** | F6: No tracing instrumentation | Add tracing to all four tool execute methods | Small |
| **LOW** | F7: `saturating_add` overflow | Replace with `checked_add` + error | Trivial |
| **LOW** | F8: `with_memory` footgun | Deprecate or unify | Trivial |
| **LOW** | F9: Empty `note_id` UX | Add clarification to tool description | Trivial |

---

## Memory Database Access in Isolated Environments

### Problem Statement

The memory database (`memory.db`) is currently configured with a **relative path** default of `.oxydra/memory.db` (set in `default_memory_db_path()` in `crates/types/src/config.rs`). When `oxydra-vm` runs, this path resolves relative to the current working directory.

In the current **Process tier** (`--insecure` mode), everything runs on the host machine and the CWD is the workspace root, so `oxydra-vm` directly accesses `.oxydra/memory.db` from the filesystem. This works today. However, this design has significant implications for Container and MicroVM tiers.

### Current Behavior

```
Process tier (--insecure):
  Host CWD = workspace root
  oxydra-vm opens .oxydra/memory.db → resolves to <workspace>/.oxydra/memory.db
  Direct filesystem access. Works.
```

### Why Per-User DB, Not a Shared Multi-User DB

Two options exist for local storage:

| Approach | Description |
|----------|-------------|
| **Shared DB** | Single `memory.db` outside any user workspace (e.g., `<workspace_root>/.oxydra/memory.db`). All users' data lives in one database, isolated only by the `memory:{user_id}` session convention. |
| **Per-user DB** | Each user gets their own `memory.db` inside their workspace (e.g., `<workspace_root>/<user_id>/.oxydra/memory.db`). Physical file-level isolation. |

**Decision: Per-user DB.** Rationale:

1. **Security — defense in depth.** A shared DB means a single SQL injection, a bug in `json_extract` filtering, or a logic error in session ID construction could leak User A's memory to User B. A per-user DB makes this physically impossible — each user's `oxydra-vm` process can only open its own file.

2. **Alignment with the workspace model.** The runner already provisions per-user workspaces (`<workspace_root>/<user_id>/`). Each user's `oxydra-vm` + `shell-vm` pair runs in isolation scoped to that workspace. The memory DB is a per-user artifact and belongs inside the user's workspace, not in a cross-user location.

3. **Mount isolation in Container/MicroVM tiers.** Each guest pair only sees its own user's workspace. A shared DB would need to be mounted into every user's guest, creating a cross-user shared file — the exact thing the sandbox model is designed to prevent.

4. **Simpler backup and lifecycle.** Deleting a user's workspace cleanly removes all their data, including memory. No need to selectively purge rows from a shared database.

5. **No concurrency complications.** SQLite handles concurrent readers well but concurrent writers from multiple processes can cause contention. Per-user DBs eliminate cross-user write contention entirely.

6. **Feasibility.** The memory subsystem already accepts a `db_path` in `MemoryConfig`. No architectural changes needed — each `oxydra-vm` instance already opens its own DB connection. The only change is where the path resolves to.

The `memory:{user_id}` session prefix inside each per-user DB is still useful — it separates note storage from conversation sessions within the same user's database. But it no longer serves as the cross-user isolation boundary.

### Problem in Container/MicroVM Tiers

In Container and MicroVM tiers, `oxydra-vm` runs inside an isolated guest. The workspace directories (`shared/`, `tmp/`, `vault/`) are bind-mounted from the host into the guest (see Chapter 8). But `.oxydra/memory.db` is **not** inside any of those well-known workspace subdirectories — it sits at the workspace root level.

Several issues arise:

#### Issue 1: Memory DB Location vs. Workspace Layout

The runner's workspace layout is:
```
<workspace_root>/<user_id>/
  ├── shared/    (persistent user data — tool-accessible)
  ├── tmp/       (ephemeral, IPC markers)
  ├── vault/     (credentials, restricted access)
  ├── logs/      (log files)
  └── ipc/       (gateway endpoint markers, daemon sockets)
```

The memory DB at `.oxydra/memory.db` (relative to CWD) would resolve to `<workspace_root>/<user_id>/.oxydra/memory.db` on the host. Currently, the runner doesn't provision this directory or mount it into guests.

#### Issue 2: Security — LLM Tools Can Access the Memory DB File

If the memory DB lives inside `shared/` (which is mounted into both `oxydra-vm` and `shell-vm` and is tool-accessible via WASM mount policies), any tool with filesystem access could read or corrupt the raw SQLite database. The LLM could `file_read shared/.oxydra/memory.db` and see raw data, or `shell_exec "sqlite3 shared/.oxydra/memory.db 'SELECT * FROM conversation_events'"` to dump everything.

The LLM-callable memory tools provide a safe, scoped API for memory access. Direct file-level access to the database bypasses all of that.

**Critical insight:** The DB must **not** live inside `shared/`, `tmp/`, or `vault/` — all of these are tool-accessible. It needs its own directory that is:
- Mounted only into the `oxydra-vm` container (not `shell-vm`)
- Not included in any WASM tool mount policy
- Not accessible via the security policy's file access checks

#### Issue 3: Multi-Channel Shared State

Per-user memory must survive across sessions and channels. In Container/MicroVM tiers where guests may be recreated, the DB must be on a persistent host-side path that survives container restarts. The current relative-path default works only because Process tier guests are long-lived host processes.

#### Issue 4: Remote Memory Mode

`MemoryConfig` already supports `remote_url` + `auth_token` for connecting to a remote libSQL instance (e.g., Turso). In this mode, the DB path is irrelevant — the guest connects over the network. This avoids all mount/access issues but introduces network latency and a dependency on an external service. This is the cleanest option for production but not required — local mode should also work correctly.

### Resolution Plan (Finalized)

#### Phase A: Per-User DB in a Dedicated `.oxydra/` Workspace Directory

**Goal:** Each user's memory DB lives at `<workspace_root>/<user_id>/.oxydra/memory.db`, inside a directory that is provisioned by the runner and mounted exclusively for `oxydra-vm`.

**Changes:**

1. **Add `.oxydra/` to the workspace layout.** Update `provision_user_workspace()` in `crates/runner/src/lib.rs` to create a `.oxydra/` directory alongside `shared/`, `tmp/`, `vault/`, `logs/`, `ipc/`. Add an `internal` (or `oxydra_internal`) field to `UserWorkspace` pointing to this directory.

   ```
   <workspace_root>/<user_id>/
     ├── .oxydra/   (internal — memory DB, config cache, model catalog)
     ├── shared/    (persistent user data — tool-accessible)
     ├── tmp/       (ephemeral, IPC markers)
     ├── vault/     (credentials, restricted access)
     ├── logs/      (log files)
     └── ipc/       (gateway endpoint markers, daemon sockets)
   ```

2. **Change the default `db_path`** in `default_memory_db_path()` from `.oxydra/memory.db` to a sentinel value (e.g., empty string or `"auto"`). When the path is `"auto"` or empty:
   - In `oxydra-vm`, resolve to `<workspace_root>/.oxydra/memory.db` using the `--workspace-root` argument already available.
   - This keeps `agent.toml` configuration optional — the default just works. Users can still override with an explicit absolute path or a path relative to the workspace root.

3. **Resolve `db_path` relative to the user's workspace root.** Update the memory backend initialization in `bootstrap_vm_runtime_with_paths()` or the `oxydra-vm` binary to resolve relative `db_path` values against the workspace root from CLI args. This way, `agent.toml` can say `db_path = ".oxydra/memory.db"` and it resolves to `<workspace_root>/<user_id>/.oxydra/memory.db`.

4. **Migration path**: If an existing `.oxydra/memory.db` exists at the old CWD-relative location and no DB exists at the new workspace-relative location, move it during first startup. Log a `tracing::warn!` about the migration.

#### Phase B: Role-Differentiated Container Mounts

**Goal:** Mount `.oxydra/` into `oxydra-vm` only — not into `shell-vm`. This gives hardware-level isolation: the shell daemon and all tools executed via it physically cannot see the memory DB.

**Changes:**

1. **Differentiate bind mounts by guest role** in `launch_docker_container_async()`. Currently, both `OxydraVm` and `ShellVm` receive identical bind mounts (`workspace.root`, `mounts.shared`, `mounts.tmp`, `mounts.vault`). Change this so:

   - **`OxydraVm`** gets: `workspace.root` (which includes `.oxydra/`), `shared`, `tmp`, `vault`
   - **`ShellVm`** gets: `shared`, `tmp`, `ipc` (for the daemon socket) — **no** `workspace.root`, **no** `.oxydra/`, **no** `vault`

   The `role` field is already passed into `DockerContainerLaunchParams` and available in the mount-building code. This is a targeted change to the `binds` construction.

2. **Update `EffectiveMountPaths`** (or add a role-aware wrapper) to express the `.oxydra/` path as a separate mount, so the runner can include it only for `OxydraVm`.

#### Phase C: Security Policy Denials for Process Tier

**Goal:** In Process tier (where there's no container boundary), prevent LLM tools from accessing the memory DB via the existing security policy.

**Changes:**

1. **Add `.oxydra/` to the security policy's path denial list.** Update `SecurityPolicy::check_file_access()` to reject any canonical path that falls within the `.oxydra/` directory under the workspace root. This blocks `file_read`, `file_write`, `file_edit`, `file_delete`, and `file_search` from touching the database or any other internal files.

2. **WASM mount exclusion.** When WASM sandboxing lands (currently simulated), ensure the `.oxydra/` directory is never included in any tool's preopened directory list. The mount policies already restrict tools to `shared/`, `tmp/`, and `vault/` — the `.oxydra/` directory is naturally excluded as long as it's not nested inside any of those.

3. **Shell command awareness.** The existing shell allowlist/blocklist should be reviewed to ensure commands like `sqlite3 .oxydra/memory.db` don't bypass file policy. Since shell commands execute inside `shell-vm` (which won't have the `.oxydra/` mount per Phase B), this is handled at the container level for Container/MicroVM tiers. For Process tier, the security policy path check covers `file_*` tools; shell commands need a separate path guard or rely on the Landlock/Seatbelt best-effort restrictions already in place.

#### Phase D: Recommend Remote Memory for Production (Advisory)

**Goal:** Document that remote libSQL is the recommended mode for production Container/MicroVM deployments.

**Changes:**

1. **Auto-detect and warn** if local memory mode is used with Container/MicroVM tiers. The bootstrap sequence should log: `"Local memory DB in Container/MicroVM tier — ensure the DB path is on a persistent volume."` This is advisory, not blocking.

2. **Document the trade-offs** in the guidebook:
   - **Local mode**: zero external dependencies, lowest latency, per-user file isolation, but requires careful mount management.
   - **Remote mode**: slight network latency, requires a running sqld/Turso instance, but cleanly separates storage from compute, survives guest recreation trivially, and avoids all mount concerns.

#### Phase E: Encrypt the Memory DB at Rest (Future Enhancement)

For additional defense in depth (not required for current needs):

1. **SQLCipher or libSQL encryption** for the local database file. Even if a tool manages to read the raw file, the contents are encrypted.

2. **Key management**: The encryption key would be delivered via the bootstrap envelope (alongside `auth_token`) and never written to the guest filesystem.

### Finalized Workspace Layout

```
<workspace_root>/<user_id>/
  ├── .oxydra/                    ← NEW: internal, oxydra-vm only
  │   ├── memory.db               ← per-user memory database
  │   └── model_catalog.json      ← (existing, also moves here)
  ├── shared/                     ← tool-accessible (file_read/write, shell)
  ├── tmp/                        ← ephemeral, IPC markers
  ├── vault/                      ← credentials, restricted
  ├── logs/                       ← log files
  └── ipc/                        ← gateway endpoint, daemon sockets
```

Mount visibility by guest role:

| Directory | `oxydra-vm` | `shell-vm` |
|-----------|:-----------:|:----------:|
| `.oxydra/` | ✅ read-write | ❌ not mounted |
| `shared/` | ✅ read-write | ✅ read-write |
| `tmp/` | ✅ read-write | ✅ read-write |
| `vault/` | ✅ read-only | ❌ not mounted |
| `ipc/` | ✅ read-write | ✅ read-write |
| `logs/` | ✅ read-write | ✅ read-write |

Tool visibility (WASM mount policies):

| Directory | `file_read` | `file_write` | `shell_exec` |
|-----------|:-----------:|:------------:|:------------:|
| `.oxydra/` | ❌ denied | ❌ denied | ❌ denied (not mounted in shell-vm) |
| `shared/` | ✅ | ✅ | ✅ |
| `tmp/` | ✅ | ✅ | ✅ |
| `vault/` | ✅ read-only | ❌ | ❌ |

### Summary

| Tier | Current State | Required Action |
|------|--------------|-----------------|
| **Process** | Works (direct filesystem access) | Move DB to `<workspace>/.oxydra/` (Phase A) + security policy denials (Phase C) |
| **Container** | Broken (DB path not provisioned or mounted) | Per-user `.oxydra/` dir (Phase A) + role-differentiated mounts (Phase B) + security policy (Phase C) |
| **MicroVM** | Broken (same as Container) | Same as Container (Phase A + B + C) |
| **Remote mode** | Works (network access, no file) | No changes needed; recommend as default for production (Phase D) |

### Implementation Order

```
Phase A (per-user DB placement)
    │
    ├──► Phase B (role-differentiated mounts) ── requires Phase A for the new path
    │
    └──► Phase C (security policy denials) ── can parallel with B
              │
              ▼
         Phase D (advisory docs + warnings) ── independent
              │
              ▼
         Phase E (encryption) ── future, independent
```

Phase A is the prerequisite. Phases B and C can be done in parallel after A. Phase D is advisory documentation. Phase E is a future enhancement.
