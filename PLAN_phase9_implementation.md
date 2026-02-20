# Phase 9 Implementation Plan: Context Window Management + Hybrid Retrieval

## Problem Statement

Phase 8 established durable conversation persistence (`sessions`, `conversation_events`, `session_state`) with ordered replay and migration safety, but it does not yet manage context-window pressure or retrieval quality for long-running sessions.  
Phase 9 must add token-budgeted context assembly, hybrid retrieval (vector + FTS), and race-safe rolling summarization without breaking existing runtime/memory behavior.

## Baseline Observations (Current Code)

1. `Memory` trait currently exposes only `store`, `recall`, `forget` (`crates/types/src/memory.rs`).
2. `LibsqlMemory` persists ordered message payloads as JSON and runs additive SQL migrations (`crates/memory/src/lib.rs`, migrations `0001`-`0005`).
3. `AgentRuntime` already persists/restores session messages and enforces turn/max-cost guards, but has no context-token budgeting or summarizer flow (`crates/runtime/src/lib.rs`).
4. Model capabilities already include `max_context_tokens`, giving a source of per-model context limits (`crates/types/src/model.rs`, pinned model catalog).
5. Config currently has runtime turn limits and memory backend settings, but no phase-9 knobs for budget thresholds/retrieval/summarization (`crates/types/src/config.rs`, `crates/cli/src/lib.rs`).

## Decision Summary

- Keep Phase 8 persistence semantics intact and extend via additive migrations only (`0006+`).
- Preserve crate boundaries: define new phase-9 contracts in `types`, implement in `memory`, consume in `runtime`, wire via `cli`.
- Add local-first token counting and embeddings with phase-recommended crates:
  - `tiktoken-rs` for token estimation/budgeting.
  - `fastembed-rs` for offline embeddings in `memory`.
- Implement hybrid retrieval by running vector similarity and FTS lookup in parallel and combining with a deterministic weighted ranker.
- Implement summarization with epoch-based compare-and-swap so stale summarizer writes cannot overwrite newer turns.

## Scope

### In Scope (Phase 9)

1. Context budget manager integrated into runtime turn preparation.
2. Additive memory schema for chunked retrieval (`files`, `chunks`, `chunks_vec`, `chunks_fts` + required indexes/triggers).
3. Local embedding pipeline and chunk indexing flow.
4. Hybrid retrieval API and ranking merge logic.
5. Rolling summarization trigger and race-safe persistence updates.
6. Config surface for budget/retrieval/summarization controls.
7. Tests covering phase-9 verification gate.

### Out of Scope (Defer)

1. Multi-provider hosted embeddings as primary path (keep local-first default).
2. Advanced retrieval learning-to-rank tuning beyond deterministic weighted merge.
3. Hierarchical branch/merge memory trees beyond required summarization flow.
4. Security sandbox changes (Phase 10).

## Architecture Constraints

1. No schema wipe/rewrite; phase-9 tables are introduced through forward migrations only.
2. Embedded local libSQL remains default; remote Turso mode remains optional and config-driven.
3. Runtime must continue to function when memory is disabled.
4. No silent fallbacks on retrieval/summary failures; surface explicit errors consistent with existing `MemoryError`/`RuntimeError`.
5. Avoid coupling runtime to concrete `LibsqlMemory` internals (use trait-based contracts from `types`).

## Implementation Workstreams

### Workstream 1 — Contracts and Config Surfaces (`types`, `cli`)

1. Add phase-9 config structs (nested under runtime/memory) for:
   - budget ratio trigger (default near 0.85),
   - safety buffer tokens,
   - retrieval top-k and weighting (`vector_weight`, `fts_weight`),
   - summarization target ratio and min-turn threshold.
2. Validate bounds in `AgentConfig::validate` (non-zero limits, 0..=1 ratio/weight invariants, weight sum constraints).
3. Extend CLI override structs and `runtime_limits` conversion to include new phase-9 knobs.
4. Add/update config contract tests and example config docs.

### Workstream 2 — Memory Trait Extension for Retrieval/Summary (`types`)

1. Keep `Memory` trait for persistence compatibility.
2. Add a phase-9 extension trait (e.g., `MemoryRetrieval`) with request/response types for:
   - chunk upsert/index,
   - hybrid query,
   - summary state read/write with epoch check.
3. Add error variants only if required for explicit phase-9 failure modes (avoid broad catch-alls).

### Workstream 3 — Additive Schema + Migrations (`memory`)

Create migrations `0006+` to add:
1. `files` table (dedupe/source tracking metadata).
2. `chunks` table (canonical chunk text + metadata + source linkage).
3. `chunks_vec` table for vector payloads linked to `chunks`.
4. `chunks_fts` FTS5 virtual table and sync triggers for insert/update/delete.
5. Retrieval indexes (`session_id`, `file_id`, recency, lookup keys).

Design rule: preserve existing `sessions` and `conversation_events` behavior unchanged.

### Workstream 4 — Embedding + Indexing Pipeline (`memory`)

1. Add local embedding adapter using `fastembed-rs` (feature-gated if startup footprint needs control).
2. Implement deterministic chunking strategy for conversation/tool text with metadata (`session_id`, role/source, sequence range).
3. On new persisted content:
   - normalize text,
   - generate chunk rows,
   - generate vectors,
   - update `chunks_vec` + `chunks_fts` atomically where possible.
4. Add idempotency guard (hash-based or unique key strategy) to avoid duplicate re-indexing.

### Workstream 5 — Hybrid Retrieval Query Engine (`memory`)

1. Implement vector search and FTS search concurrently.
2. Normalize scores and merge via configurable weighted rank.
3. Return top-k records with stable tie-breaking (score, recency, chunk_id).
4. Expose retrieval API returning text snippets + metadata for runtime injection.

### Workstream 6 — Context Budget Manager (`runtime`)

1. Add a `ContextBudgetManager` component that computes:
   - system tokens,
   - retrieved memory tokens,
   - conversation history tokens,
   - tool schema tokens,
   - reserved safety buffer.
2. Use model `max_context_tokens` where available; otherwise use configured fallback.
3. Before each provider call:
   - assemble candidate context,
   - inject hybrid memory snippets,
   - trim/prioritize history deterministically to remain in budget.
4. Ensure persisted transcript remains complete even if provider-facing context is compacted.

### Workstream 7 — Rolling Summarization + Epoch Safety (`runtime` + `memory`)

1. Trigger summarization when budget threshold is exceeded.
2. Use asynchronous summarizer path with captured `(session_id, epoch, upper_sequence)` snapshot.
3. Persist summary/truncation only when epoch still matches (compare-and-swap style update).
4. If epoch changed, discard stale summary result and retry on a later cycle.
5. Persist summary artifacts in dedicated phase-9 memory structures (table or structured `session_state` update), preserving replay invariants.

### Workstream 8 — Testing and Verification Gate

Required checks to close Phase 9:

1. **Budget enforcement test:** runtime never exceeds model/context limit after memory injection.
2. **Summarization trigger test:** threshold crossing initiates summarizer flow.
3. **Race safety test:** stale summarizer result cannot overwrite newer epoch.
4. **Hybrid retrieval relevance test:** vector+FTS merge returns deterministic top-k.
5. **Migration continuity test:** Phase-8 DB migrates to Phase-9 schema without data loss.
6. **Memory-disabled behavior test:** runtime still operates with existing phase-5/8 semantics.
7. **Config validation tests:** invalid budget/weight/summarization settings fail fast.

## Deliverables

1. Phase-9 config and contract extensions in `types`/`cli`.
2. New memory migrations (`0006+`) and retrieval/indexing implementation.
3. Runtime context budget manager with retrieval-aware context assembly.
4. Race-safe summarization pipeline.
5. Automated tests covering all phase-9 verification criteria.
6. Documentation updates (`README`/example config) reflecting new knobs and behavior.

## Risks and Mitigations

1. **Risk:** Vector SQL shape differs between local libSQL builds and remote Turso.  
   **Mitigation:** keep an abstraction boundary in `memory`; validate vector DDL/query behavior with migration tests in local mode first.

2. **Risk:** Token estimates drift from provider-reported usage.  
   **Mitigation:** treat `tiktoken-rs` as preflight estimator and keep provider usage as post-call telemetry for calibration.

3. **Risk:** Summarization introduces ordering regressions.  
   **Mitigation:** enforce epoch compare-and-swap and deterministic sequence cutoffs in transactional updates.

4. **Risk:** Retrieval adds latency to each turn.  
   **Mitigation:** parallelize vector/FTS queries, cap top-k, and cache embeddings for unchanged chunks.

## Phase Exit Criteria

Phase 9 is complete when:

1. Runtime consistently stays within context-token limits using configured budgets.
2. Hybrid retrieval is active and returns stable relevant snippets from persisted memory.
3. Rolling summarization triggers automatically and is race-safe under concurrent turns.
4. Existing phase-8 persistence/recovery remains intact after phase-9 migrations.
5. Phase-9 tests pass alongside existing workspace quality gates.

## Open Questions to Confirm

1. Should phase-9 delivery include external document ingestion APIs immediately, or keep indexing limited to conversation/tool outputs first?
A: Defer this to later. Add this to a section "TODO not covered under any phases" in the oxydra-project-brief.md file
2. Preferred default hybrid rank weighting: `vector 0.7 / fts 0.3` (recommended) vs another ratio?
A: Recommended option is ok
3. For first cut, should summarization run inline (simpler, higher latency) or background (recommended, lower latency with epoch retries)?
A: Inline is fine. background option to be added in the TODO section of oxydra-project-brief as mentioned.
