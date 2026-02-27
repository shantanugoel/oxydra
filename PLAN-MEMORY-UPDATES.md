# Memory + Autonomy Enhancement Plan

## Objective

Make the agent highly proactive, autonomous, and action-oriented by improving how it:
- recalls durable cross-session knowledge,
- keeps track of active goals in long multi-step execution,
- stores high-value learnings from both success and failure,
- updates stale/incorrect memory safely,
- and asks the user only when truly blocked.

This is the final standalone roadmap.

---

## Hard Constraints for This Plan

1. **Native libSQL/Turso vector retrieval only** (DiskANN path).
2. **No Rust-side hybrid retrieval fallback path**.
3. **No backward-compat migration layer** for old embedding-selection behavior.
4. **No remote embedding backend**.
5. **No fastembed backend**.

---

## Key Technical Clarification

- libSQL/Turso can natively index and query vectors (`libsql_vector_idx`, `vector_top_k`) for fast ANN retrieval.
- The database does **not** generate embeddings from text.
- We still need an embedding generator in the app layer.

Therefore, this plan uses:
- **model2vec-rs** for semantic embeddings (configurable potion model size), and
- **deterministic embeddings** as a lightweight non-semantic fallback mode.

---

## Final Design Decisions

| Decision | Choice | Rationale |
|---|---|---|
| Retrieval engine | **libSQL native vector index/query only** | Best performance path; eliminates duplicated retrieval implementations. |
| Embedding backend | **`model2vec` or `deterministic`** | Keeps semantic quality while preserving a minimal deterministic mode. |
| Semantic model choice | **Configurable `potion-8m` / `potion-32m`** | Allows tuning quality vs memory/latency footprint. |
| Default semantic profile | Start with `potion-8m`, keep `potion-32m` selectable | Practical default footprint with upgrade path for better recall. |
| Fastembed | **Not used** | Reduces dependency surface and binary bloat concerns. |
| Remote embeddings | **Not used** | Keep architecture fully local for this phase. |
| Deterministic mode | **Documented and explicit** | Predictable, tiny-dependency fallback for constrained/offline scenarios. |
| Note metadata | `note_id`, `source`, `tags`, `created_at`, `updated_at`, `schema_version` | Searchability, freshness, migrations. |
| Timestamp authority | Backend-owned | Prevents malformed/hallucinated timestamps. |
| Tag filtering | DB-level filtering in retrieval query path | Better precision/perf vs broad post-filtering. |
| Working memory | Session-scoped scratchpad with hard size limits | Keeps long autonomous runs coherent. |
| Autonomy loop | `Plan -> Recall -> Execute -> Reflect -> Learn` | Pushes proactive action and correction behavior. |
| Retry guard | Up to 3 distinct attempts before escalation | Avoids infinite loops while staying action-biased. |

---

## What “Deterministic” Means (Must Be Documented)

`deterministic` embeddings are a fixed, non-semantic vectorization method derived from content hashing (current implementation pattern: stable hash -> normalized numeric vector).

Properties:
- same text -> same vector every time,
- tiny dependency footprint,
- fast and stable,
- **not truly semantic** (paraphrases may not cluster well).

Use cases:
- minimal builds,
- deterministic test behavior,
- constrained environments where semantic model load is undesirable.

---

## Scope

### In Scope
- model2vec-based embedding pipeline (`potion-8m` / `potion-32m` configurable).
- deterministic backend retained and clearly defined.
- native libSQL vector retrieval path only.
- note metadata/tags/timestamps and note identity correctness fixes.
- DB-level tag filtering support in memory queries.
- memory tool schema/behavior upgrades.
- session-scoped working-memory scratchpad.
- autonomy prompt upgrades with reflection/retry behavior.
- guidebook updates for config/tools/memory/runtime.

### Out of Scope
- any remote embedding mode,
- fastembed support,
- backward-compat migration shims for deprecated embedding selection,
- Rust-side retrieval fallback implementation.

---

## Implementation Plan (Test-Gated)

## Phase 0 — Baseline Safety

**Goal:** Freeze baseline and test harness before changes.

**Actions:**
1. Capture baseline tests for touched crates (`types`, `memory`, `tools`, `runtime`, `runner`).
2. Add explicit checks for expected new defaults after config changes.

**Verification:**
- `cargo test -p types -p memory -p tools -p runtime -p runner -- --test-threads=1`

---

## Phase 1 — Config & Types (Model2vec + Deterministic)

**Files:**
- `crates/types/src/config.rs`
- `crates/types/src/lib.rs`
- `crates/types/tests/config_contracts.rs`

**Changes:**
1. Add `memory.embedding_backend` enum:
   - `model2vec`
   - `deterministic`
2. Add `memory.model2vec_model` enum:
   - `potion_8m`
   - `potion_32m`
3. Add validation/defaults:
   - default backend: `model2vec`
   - default model: `potion_8m`
4. Remove old env-var-based embedding backend selection behavior from config contract.
5. Add doc comments defining deterministic semantics.

**Verification:**
- `cargo test -p types -- --test-threads=1`

---

## Phase 2 — Embedding Adapter Refactor (No Fastembed)

**Files:**
- `crates/memory/src/indexing.rs`
- `crates/memory/src/lib.rs`
- `crates/memory/Cargo.toml`
- `crates/runner/Cargo.toml`

**Changes:**
1. Replace fastembed strategy with model2vec strategy.
2. Implement model2vec adapter supporting potion model selection.
3. Keep deterministic adapter path.
4. Remove fastembed feature usage from memory/runner wiring.
5. Remove temporary env-var compatibility logic.

**Verification:**
- `cargo build -p memory`
- `cargo build -p runner`
- `cargo test -p memory -- --test-threads=1`

---

## Phase 3 — Native libSQL Vector Retrieval Only

**Files:**
- `crates/memory/src/lib.rs`
- `crates/memory/src/schema.rs` and migrations (if needed)
- `crates/memory/src/tests.rs`

**Changes:**
1. Implement retrieval candidate path through native vector index/query primitives.
2. Ensure vector index creation/maintenance is wired (`libsql_vector_idx(...)`).
3. Remove/retire Rust-side fallback retrieval path from this roadmap.
4. Fail fast with explicit initialization/query errors if native vector capability is unavailable.

**Verification:**
- `cargo test -p memory -- --test-threads=1`
- integration smoke test for `vector_top_k` retrieval flow.

---

## Phase 4 — Metadata, Tags, and Note Identity Correctness

**Files:**
- `crates/types/src/memory.rs`
- `crates/memory/src/lib.rs`
- `crates/memory/src/indexing.rs`
- touched callers/tests in `tools` and `runtime`

**Changes:**
1. Introduce structured note write request (`MemoryNoteStoreRequest`).
2. Normalize tags in backend (trim/lowercase/dedupe/caps).
3. Set backend-owned timestamps (`created_at`, `updated_at`).
4. Persist metadata schema fields consistently.
5. Fix note dedupe semantics to avoid cross-note corruption with identical content.
6. Add regression tests for note independence on update/delete/search.

**Verification:**
- `cargo test -p memory -p tools -p runtime -- --test-threads=1`

---

## Phase 5 — Memory Tool Upgrades

**Files:**
- `crates/tools/src/memory_tools.rs`
- `crates/tools/src/tests.rs`

**Changes:**
1. `memory_save` and `memory_update`: add optional `tags`.
2. `memory_search`: add optional `tags`; return `tags`, `created_at`, `updated_at`.
3. Push tag filtering into retrieval query path (DB-level) where possible.
4. Strengthen tool descriptions for proactive memory saves of durable learnings and corrected procedures.

**Verification:**
- `cargo test -p tools -- --test-threads=1`

---

## Phase 6 — Working-Memory Scratchpad (Session-Scoped)

**Goal:** Prevent long-run drift during autonomous execution.

**Files:**
- `crates/memory/src/lib.rs`
- `crates/tools/src/` (scratchpad tool surface)
- `crates/runner/src/bootstrap.rs`
- runtime/tests as needed

**Changes:**
1. Add small session-scoped scratchpad state in `session_state`.
2. Add strict JSON-schema tool(s) for scratchpad read/write.
3. Enforce hard byte/field limits.
4. Keep scratchpad separate from persistent user memory unless explicitly saved.

**Verification:**
- tool + runtime integration tests for long multi-step tasks.

---

## Phase 7 — Prompt-Level Autonomy Protocol

**File:** `crates/runner/src/bootstrap.rs`

**Changes:**
1. Add explicit operational loop in memory guidance:
   - Plan,
   - Recall memory,
   - Execute tools,
   - Reflect on failures,
   - Save durable learnings.
2. Add retry protocol:
   - do not repeat the same failed action,
   - attempt up to 3 distinct approaches,
   - escalate with concise blocker summary when still blocked.
3. Clarify user-ask boundary: only for missing requirements/credentials/irreversible decisions.

**Verification:**
- `cargo test -p runner -- --test-threads=1`

---

## Phase 8 — Documentation + Full Validation

### Documentation updates

Update:
- `docs/guidebook/02-configuration-system.md`
- `docs/guidebook/04-tool-system.md`
- `docs/guidebook/05-agent-runtime.md`
- `docs/guidebook/06-memory-and-retrieval.md`

Required doc additions:
1. model2vec backend and potion model configuration.
2. native libSQL vector retrieval architecture.
3. deterministic embedding definition, strengths, and limitations.
4. working-memory scratchpad semantics and limits.

### Validation matrix

1. Format/lint:
- `cargo fmt --all --check`
- `cargo clippy -p types -p memory -p tools -p runtime -p runner --all-targets --all-features -- -D warnings`

2. Tests:
- `cargo test -p types -p memory -p tools -p runtime -p runner -- --test-threads=1`

3. Manual checks:
- note save/search/update/delete with tags + timestamps,
- same-content different-note lifecycle correctness,
- native vector retrieval correctness/perf sanity,
- scratchpad continuity in long tasks,
- retry/escalation autonomy behavior.

---

## Risks and Mitigations

| Risk | Mitigation |
|---|---|
| model2vec model quality/latency tradeoffs | benchmark potion-8m vs 32m and keep runtime-selectable config. |
| Native vector feature mismatch by environment | explicit startup capability checks and fail-fast errors. |
| Infinite retry loops | strict 3-distinct-attempt escalation rule. |
| Scratchpad growth | hard schema and byte limits. |
| Note lifecycle corruption | note_id-safe dedupe semantics + regression tests. |

---

## Expected Outcome

After this plan is implemented, the agent should:
- proactively retrieve and apply relevant memory,
- stay coherent across long autonomous workflows,
- recover from failures with distinct retries,
- store durable learnings with strong metadata quality,
- and minimize unnecessary user dependency.
