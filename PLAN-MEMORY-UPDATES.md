# Memory System Enhancement — Implementation Plan

## Goal

Make the LLM deeply aware of and proactive about using persistent memory to:
- Recall past conversations, preferences, and learned knowledge
- Store user preferences, facts, and important discoveries
- Build reusable procedures ("skills") from trial-and-error
- Track corrective learnings and pitfalls
- Maintain and update stored knowledge as understanding evolves

## Design Decisions (Agreed)

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Auto-retrieval from persistent memory | **No** — tool-based with strong prompting | Blake3 embeddings lack semantic quality; LLM crafts better targeted queries; tool calls can filter by tags; future-proof for auto-injection when semantic embeddings are default |
| Note taxonomy | **Tags** (free-form, LLM-chosen) | More flexible than fixed categories; LLM can create domain-specific tags; multiple tags per note; guided by system prompt conventions |
| Staleness tracking | **Timestamps only** (`created_at`) | Simple, useful signal for the LLM to judge note freshness; skip confidence scoring for now |
| Embedding backend | **Config option**, default `fastembed` | Semantic search is critical for tool-based retrieval; config option allows fallback to deterministic |
| System prompt | **Expand significantly**, conditional on available tools | Detailed behavioral steering for when to search, when to save, how to write notes, memory maintenance |
| `memory_list` tool | **No** — search with broad query is sufficient | Keeps tool surface lean |
| Implementation order | **All together** — one coherent change | All pieces are interdependent |

## Scope of Changes

| Area | Crates Affected | Nature |
|------|-----------------|--------|
| A. Embedding backend config | `types`, `memory`, `runner` | Config + code |
| B. Tags on notes | `tools` (+ minor `memory` store_note metadata) | Code + schema |
| C. Staleness tracking | `tools`, `memory` | Code + metadata |
| D. System prompt expansion | `runner` | Prompt engineering |
| E. Tool description enhancement | `tools` | Prompt engineering |

---

## Step-by-Step Implementation

### Step 1: Add `embedding_backend` to `MemoryConfig` (types crate)

**File:** `crates/types/src/config.rs`

**Changes:**
1. Add `embedding_backend` field to `MemoryConfig`:
   ```rust
   #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
   pub struct MemoryConfig {
       #[serde(default)]
       pub enabled: bool,
       #[serde(default, skip_serializing_if = "Option::is_none")]
       pub remote_url: Option<String>,
       #[serde(default, skip_serializing_if = "Option::is_none")]
       pub auth_token: Option<String>,
       #[serde(default)]
       pub embedding_backend: EmbeddingBackend,
       #[serde(default)]
       pub retrieval: RetrievalConfig,
   }
   ```

2. Add `EmbeddingBackend` enum:
   ```rust
   #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
   #[serde(rename_all = "lowercase")]
   pub enum EmbeddingBackend {
       Fastembed,
       Deterministic,
   }

   impl Default for EmbeddingBackend {
       fn default() -> Self {
           Self::Fastembed
       }
   }
   ```

3. Re-export `EmbeddingBackend` from `types/src/lib.rs`.

**Verification:**
- Existing config deserialization still works (field has a default)
- Existing tests in `types/tests/config_contracts.rs` still pass
- `cargo test -p types`

---

### Step 2: Enable `fastembed` feature by default (memory + runner crates)

**File:** `crates/memory/Cargo.toml`

**Changes:**
1. Make `fastembed` a default feature:
   ```toml
   [features]
   default = ["fastembed"]
   fastembed = ["dep:fastembed"]
   ```

**File:** `crates/runner/Cargo.toml`

**Changes:**
1. Enable `fastembed` feature on the memory dependency:
   ```toml
   memory = { path = "../memory", features = ["fastembed"] }
   ```
   (This is redundant with the default but makes the intent explicit; ensures fastembed is available even if someone changes the default.)

**Verification:**
- `cargo build -p memory` succeeds with fastembed
- `cargo build -p memory --no-default-features` still compiles (Blake3 fallback)
- `cargo build -p runner` succeeds

---

### Step 3: Refactor `EmbeddingAdapter` to use config instead of env var (memory crate)

**File:** `crates/memory/src/indexing.rs`

**Changes:**
1. Modify `EmbeddingAdapter` to hold the configured backend choice:
   ```rust
   #[derive(Debug, Clone)]
   pub(crate) struct EmbeddingAdapter {
       backend: EmbeddingBackendChoice,
   }

   #[derive(Debug, Clone, Copy, PartialEq, Eq)]
   pub(crate) enum EmbeddingBackendChoice {
       Fastembed,
       Deterministic,
   }
   ```

2. Update `EmbeddingAdapter::new(choice)` constructor.

3. In `embed_batch`, use the stored `backend` field instead of reading the env var:
   ```rust
   fn embed_batch(&self, texts: &[String]) -> Result<EmbeddingBatch, MemoryError> {
       if texts.is_empty() {
           return Ok(EmbeddingBatch { model: DETERMINISTIC_EMBEDDING_MODEL.to_owned(), vectors: Vec::new() });
       }

       #[cfg(feature = "fastembed")]
       if self.backend == EmbeddingBackendChoice::Fastembed {
           let vectors = embed_with_fastembed(texts)?;
           return Ok(EmbeddingBatch { model: FASTEMBED_EMBEDDING_MODEL.to_owned(), vectors });
       }

       #[cfg(not(feature = "fastembed"))]
       if self.backend == EmbeddingBackendChoice::Fastembed {
           tracing::warn!(
               "fastembed embedding backend requested but feature not compiled in; falling back to deterministic"
           );
       }

       let vectors = texts.iter().map(|text| deterministic_embedding(text)).collect();
       Ok(EmbeddingBatch { model: DETERMINISTIC_EMBEDDING_MODEL.to_owned(), vectors })
   }
   ```

4. `embed_query` unchanged except it delegates to `embed_batch` which now uses the stored backend.

**File:** `crates/memory/src/lib.rs`

**Changes:**
1. `LibsqlMemory` now stores `EmbeddingAdapter` constructed from config:
   ```rust
   impl LibsqlMemory {
       pub async fn from_config(config: &MemoryConfig) -> Result<Option<Self>, MemoryError> {
           // ... existing connection strategy logic ...
           // Pass embedding_backend from config
       }
   }
   ```

2. The `open()` method and constructors (`new_local`, `new_remote`) need to accept the embedding backend choice. Add a parameter or use a builder pattern.

3. Map `types::EmbeddingBackend` to `indexing::EmbeddingBackendChoice` during construction.

**File:** `crates/runner/src/bootstrap.rs`

**Changes:**
1. Remove any env var handling for `OXYDRA_MEMORY_EMBEDDING_BACKEND` (if it exists in bootstrap).
2. Pass `config.memory.embedding_backend` through to `LibsqlMemory` construction.

**Verification:**
- `cargo test -p memory` — all existing memory tests pass
- `cargo test -p runner` — bootstrap tests pass
- Build with and without fastembed feature

---

### Step 4: Add tags support to `store_note` (memory crate)

**File:** `crates/memory/src/lib.rs` (`store_note` method)

**Changes:**
1. The `store_note` method already accepts `content` and stores `note_id` in metadata via `prepare_index_document_with_extra_metadata`. Tags will be stored in the same `extra_metadata`:
   ```rust
   let extra_metadata = json!({
       "note_id": note_id,
       "source": "memory_save",
       "tags": tags,              // NEW: Vec<String>
       "created_at": now_iso8601, // NEW: timestamp
   });
   ```

2. The `store_note` signature needs to accept tags and created_at. Two options:
   - **Option A:** Add parameters: `store_note(&self, session_id, note_id, content, tags, created_at)` — breaks the trait
   - **Option B:** Use a struct: `StoreNoteRequest { session_id, note_id, content, tags, created_at }` — cleaner but bigger change
   - **Option C:** Pass tags and created_at through the existing `extra_metadata` mechanism from the tool layer — no trait change needed

   **Recommended: Option C.** The tool layer already constructs the `content` string. We can embed tags and timestamps into the content or metadata from the tool side, and pass them through the existing `extra_metadata` parameter path. But wait — `store_note` doesn't expose extra_metadata to callers. We need to either:
   - Modify `MemoryRetrieval::store_note` trait to accept optional metadata
   - Or handle it entirely in the tool layer by embedding tags in the note content

   **Decision: Modify the trait** to accept optional tags and timestamp. This is cleaner:

**File:** `crates/types/src/memory.rs` (`MemoryRetrieval` trait)

```rust
async fn store_note(
    &self,
    session_id: &str,
    note_id: &str,
    content: &str,
    tags: Option<&[String]>,
    created_at: Option<&str>,
) -> Result<(), MemoryError>;
```

Update all implementations (`LibsqlMemory`, `UnconfiguredMemory`).

In `LibsqlMemory::store_note`, merge `tags` and `created_at` into the `extra_metadata` JSON that's already passed to `prepare_index_document_with_extra_metadata`.

**Verification:**
- `cargo test -p types` — contract tests pass
- `cargo test -p memory` — note storage tests pass
- `cargo build` — all crates compile (tool crate updated in Step 5)

---

### Step 5: Add tags and timestamps to memory tools (tools crate)

**File:** `crates/tools/src/memory_tools.rs`

#### 5a. Update `MemorySaveTool`

**Schema changes:**
```json
{
  "type": "object",
  "required": ["content"],
  "properties": {
    "content": {
      "type": "string",
      "description": "Natural language text to save. Make it self-contained — it will be retrieved out of context in future conversations. Include WHEN/WHERE it applies.",
      "minLength": 1
    },
    "tags": {
      "type": "array",
      "items": { "type": "string" },
      "description": "Optional tags for categorization and retrieval filtering. Recommended tags: 'preference' (user preferences), 'procedure' (working methods), 'correction' (pitfalls/failures), 'convention' (project conventions), 'fact' (durable facts). Add domain-specific tags as appropriate (e.g., 'rust', 'aws')."
    }
  }
}
```

**Description update:**
```
Save an important piece of information to your persistent memory as a natural language note.

Save proactively when:
- The user shares preferences, personal details, or how they like things done.
- You discover a working approach after trial-and-error — save what works AND what failed.
- You find a non-obvious pitfall or something that doesn't work as expected.
- You discover project or codebase conventions.
- You learn durable facts about the user's setup, environment, or domain.

Use tags to categorize notes for better retrieval. Make notes self-contained and specific —
they will be retrieved out of context in future conversations.
Do NOT save secrets, one-off outputs, or transient details. Returns a note_id for future reference.
```

**Arg struct change:**
```rust
#[derive(Debug, Deserialize)]
struct MemorySaveArgs {
    content: String,
    tags: Option<Vec<String>>,
}
```

**Execute change:**
- Generate ISO 8601 timestamp for `created_at`
- Pass `tags` and `created_at` to `store_note()`

#### 5b. Update `MemoryUpdateTool`

**Schema changes:**
- Add optional `tags: string[]` parameter (same description as save)

**Arg struct change:**
```rust
#[derive(Debug, Deserialize)]
struct MemoryUpdateArgs {
    note_id: String,
    content: String,
    tags: Option<Vec<String>>,
}
```

**Execute change:**
- On update (delete + re-store), pass new tags and fresh `created_at`

**Description update:**
```
Update a previously saved note with new content. Use this when:
- The user corrects or changes previously stored information.
- A saved procedure is superseded by a better approach.
- You discover that stored information is inaccurate or outdated.
- Tags need to be changed.

First use memory_search to find the note and its note_id, then call this with the
note_id and the new content. Preserve useful context from the old note where applicable.
```

#### 5c. Update `MemorySearchTool`

**Schema changes:**
```json
{
  "properties": {
    "tags": {
      "type": "array",
      "items": { "type": "string" },
      "description": "Optional tag filter — only return notes matching ANY of these tags"
    }
  }
}
```

**Arg struct change:**
```rust
#[derive(Debug, Deserialize)]
struct MemorySearchArgs {
    query: String,
    top_k: Option<usize>,
    tags: Option<Vec<String>>,
    include_conversation: Option<bool>,
}
```

**Execute change:**
- After hybrid search returns results, post-filter: if `tags` is provided, keep only results whose metadata `tags` array contains at least one of the requested tags
- Results now include `tags`, `created_at` fields from metadata:
  ```json
  {
    "note_id": "note-abc123",
    "text": "...",
    "score": 0.85,
    "source": "user_memory",
    "tags": ["procedure", "rust"],
    "created_at": "2026-02-27T17:48:00Z"
  }
  ```

**Description update:**
```
Search your persistent memory for relevant information. Use this proactively:
- At the START of a conversation when the request relates to a domain you may have
  worked on before.
- Before any complex or multi-step task, to find related procedures and known pitfalls.
- When the user references something discussed in a previous conversation.
- When you're unsure about user preferences or project conventions.

Results are ranked by relevance using semantic similarity and keyword matching.
By default searches only your saved notes; set include_conversation to true to also
search the current conversation history. Use tags to filter results by category.
Results with an empty note_id (from conversation search) cannot be updated or deleted.
```

#### 5d. Update `MemoryDeleteTool`

**Description:** No change needed (already clear).

**Verification:**
- `cargo test -p tools` — all tool tests pass
- `cargo build` — full build succeeds

---

### Step 6: Expand system prompt memory section (runner crate)

**File:** `crates/runner/src/bootstrap.rs`

**Changes to `build_system_prompt` function:**

Replace the current `memory_note` section with the expanded version. The section remains conditional on `memory_enabled: bool`.

Draft prompt text (to be reviewed by user before finalizing):

```rust
let memory_note = if memory_enabled {
    "\n\n## Memory & Knowledge Management\n\n\
     You have persistent memory that carries knowledge across all conversations with this user.\n\
     Memory is personal to the user and persists indefinitely.\n\n\
     ### When to Search Memory\n\
     - At the START of a conversation, if the user's request relates to a domain or project \
       you may have worked on before, search memory for relevant context, preferences, and conventions.\n\
     - Before any complex or multi-step task, search for related procedures and known pitfalls.\n\
     - When the user refers to something you discussed previously, search for that context.\n\
     - When you're unsure about user preferences or conventions, search before assuming.\n\n\
     ### When to Save to Memory\n\
     - When the user shares preferences, personal details, or how they like things done. \
       Tag: \"preference\"\n\
     - When you discover a working approach after trial-and-error (save what works AND what failed). \
       Tag: \"procedure\"\n\
     - When you find that something doesn't work or has a non-obvious pitfall. \
       Tag: \"correction\"\n\
     - When you discover project or codebase conventions. \
       Tag: \"convention\"\n\
     - When you learn durable facts about the user's setup, environment, or domain. \
       Tag: \"fact\"\n\
     - Add domain-specific tags for better retrieval (e.g., \"rust\", \"aws\", \"deployment\").\n\n\
     ### How to Write Good Notes\n\
     - Make notes self-contained — they will be retrieved out of context in future conversations.\n\
     - Include WHEN and WHERE a note applies (e.g., \"For project Oxydra...\", \"When using libSQL...\").\n\
     - For procedures: include both what works AND what to avoid.\n\
     - Keep notes atomic — one concept per note leads to better search retrieval.\n\
     - Do NOT save: secrets, passwords, API keys, one-off command outputs, or transient status.\n\n\
     ### Maintaining Memory\n\
     - When you find a saved note that contradicts what you've since learned, update it.\n\
     - When a procedure you saved no longer works, update it with the corrected approach.\n\
     - When updating notes, preserve useful context from the old version where applicable.\n\
     - Older notes may be outdated — consider verifying critical information if a note \
       was created long ago."
} else {
    ""
};
```

**Update existing test:**
- `build_system_prompt_includes_memory_guidance_when_memory_is_enabled` — update assertion to check for expanded content (e.g., check for "When to Search Memory" instead of old content)
- `build_system_prompt_omits_memory_guidance_when_memory_is_disabled` — unchanged

**Verification:**
- `cargo test -p runner` — bootstrap tests pass
- Manual review of generated prompt

---

### Step 7: Update guidebook documentation (docs)

**File:** `docs/guidebook/06-memory-and-retrieval.md`

**Changes:**
1. Add section on "Embedding Backend Configuration" describing the `embedding_backend` config option
2. Update "Note Storage Pipeline" section to mention tags and `created_at` in metadata
3. Add section on "Tag-Based Filtering" in `memory_search`
4. Update the "Memory Tools" reference to reflect new parameters

**File:** `docs/guidebook/04-tool-system.md`

**Changes:**
1. Update the memory tools table to reflect new parameters (tags, created_at)
2. Update tool descriptions

---

## Verification Checklist

After all steps are complete:

- [ ] `cargo build` — full workspace builds without errors or warnings
- [ ] `cargo clippy --all-targets` — no clippy warnings
- [ ] `cargo test` — full test suite passes
- [ ] `cargo test -p types` — config contracts including new `EmbeddingBackend`
- [ ] `cargo test -p memory` — note storage/retrieval with tags
- [ ] `cargo test -p tools` — memory tool tests with new parameters
- [ ] `cargo test -p runner` — bootstrap prompt tests updated
- [ ] `cargo build -p memory --no-default-features` — Blake3 fallback still compiles
- [ ] Manual test: save a note with tags, search with tag filter, verify results
- [ ] Manual test: verify `created_at` appears in search results
- [ ] Manual test: verify system prompt includes expanded memory guidance

## Migration Safety

- **No database schema migration needed** — tags and `created_at` are stored in existing `metadata_json` fields in the `chunks` table. No new columns or tables.
- **Config backward compatible** — `embedding_backend` has a default value; existing configs without this field work fine.
- **Trait change** — `store_note` gains new optional parameters. All implementations updated in the same change.
- **Existing notes** — notes saved before this change will have `null` tags and `null` created_at in search results. This is handled gracefully (shown as empty/absent in results).

## Future Enhancements (Not in this plan)

- **Auto-injection from persistent memory** — when semantic embeddings are default and proven reliable, add configurable auto-injection of persistent memory into every turn's context. Architecture already supports this (just query `memory:{user_id}` in `fetch_retrieved_memory_message`).
- **Confidence scoring** — add an LLM-settable `confidence` field (high/medium/low) on notes.
- **Cross-session summary** — at end of conversation, auto-summarize key takeaways to persistent memory.
- **Memory review schedule** — a periodic scheduled task that reviews and prunes stale notes.
- **`memory_list` tool** — paginated listing of all notes with metadata for self-review.
