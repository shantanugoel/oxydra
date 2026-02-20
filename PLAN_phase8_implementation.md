# Phase 8 Implementation Plan: Memory Trait + libSQL Persistence

## Problem Statement

Phase 8 currently targets SQLite persistence via `rusqlite`, but we want an async-first path that remains local-first while opening a clean path to native vector search later.  
This plan standardizes Phase 8 on Turso/libSQL and keeps Phase 9 retrieval work additive (no rewrite).

## Decision Summary

- Adopt `libsql` as the Phase 8 database driver for the `memory` crate.
- Keep **embedded/local file mode** as the default to preserve offline/local-first operation.
- Add **optional Turso remote mode** via configuration for shared/replicated deployments.
- Keep the `Memory` trait unchanged (`store`, `recall`, `forget`) so runtime contracts remain stable.
- Use versioned SQL migrations from day one (recommended: `refinery` or equivalent migration runner).

## Scope

### In Scope (Phase 8)

1. `memory` crate persistence backend using async `libsql`.
2. Initial durable schema for conversations/sessions and replay.
3. Migration framework wiring and first migration set.
4. Runtime integration to store and restore sessions across process restarts.
5. Config surface for local DB path and optional Turso URL/auth token.
6. Phase-appropriate tests for persistence and migration safety.

### Out of Scope (Deferred to Phase 9+)

1. Hybrid ranking quality tuning (vector weighting/BM25 weighting).
2. Rolling summarization and token-budget policy logic.
3. Embedding model selection optimization.
4. Security hardening of execution sandbox (Phase 10).

## Architecture Constraints (From the Brief)

- Preserve strict crate boundaries (`memory` depends on `types`; runtime consumes trait, not backend internals).
- Keep local-first defaults and make remote connectivity opt-in.
- Avoid schema over-normalization in Phase 8; store JSON payloads where useful for fast delivery.
- Ensure all changes are forward-compatible with Phase 9 vector + FTS expansion.

## Implementation Workstreams

### Workstream 1 — Memory Interface and Error Surface

1. Confirm `Memory` trait contract in `types` is sufficient (`store`, `recall`, `forget`).
2. Define/extend `MemoryError` variants for:
   - connection/init failures,
   - migration failures,
   - query/serialization failures,
   - not-found semantics.
3. Keep error propagation explicit; no silent fallbacks between local and remote modes.

### Workstream 2 — libSQL Backend Foundation

1. Introduce `libsql` backend in `crates/memory`.
2. Add configuration-backed connection strategy:
   - local embedded database path (default),
   - optional Turso/libSQL remote URL + auth token.
3. Implement deterministic startup initialization:
   - open connection,
   - run pending migrations,
   - verify required tables/indexes exist.

### Workstream 3 — Initial Phase 8 Schema

Use a JSON-first schema that is simple to evolve:

- `sessions` (session metadata, creation/update timestamps, agent identity)
- `conversation_events` (ordered messages/tool outputs as JSON payloads)
- `session_state` (lightweight state snapshots/checkpoints)
- migration bookkeeping table managed by migration runner

Design notes:
- Keep ordering deterministic (`session_id`, monotonic turn/event sequence).
- Add minimal indexes for restore and recent-session listing.
- Defer vector/FTS tables to Phase 9 migrations.

### Workstream 4 — Runtime Integration

1. Wire runtime save points after each completed turn/tool result.
2. Implement session restore path for CLI/runtime startup.
3. Ensure cancellation/error paths do not corrupt persisted ordering.
4. Preserve existing runtime behavior when memory is disabled by config.

### Workstream 5 — Testing and Verification Gate

Required Phase 8 verification:

1. **Persistence restart test:** conversation survives process restart and restores in order.
2. **Migration forward test:** migrate from schema v1 baseline to current successfully.
3. **Config mode tests:** embedded local mode works by default; remote mode validates required auth fields.
4. **Trait-level behavior tests:** `store`, `recall`, `forget` semantics are deterministic.
5. **Failure-path tests:** surfaced errors for unreachable DB/migration failure (no silent success).

## Deliverables

1. `memory` crate running on `libsql`.
2. Versioned migration files and migration runner integration.
3. Runtime + config wiring for local and optional Turso remote modes.
4. Automated tests covering Phase 8 verification gate.
5. Documentation updates aligned with Chapter 6 + Appendix phase table.

## Risks and Mitigations

1. **Risk:** Remote Turso configuration drift in development.  
   **Mitigation:** Keep local embedded mode as default and CI baseline.
2. **Risk:** Async database usage introduces ordering races.  
   **Mitigation:** Use explicit ordering keys and transactional writes for turn/event persistence.
3. **Risk:** Future Phase 9 schema pressure.  
   **Mitigation:** Reserve additive migration path and avoid over-normalized v1 schema.

## Phase Exit Criteria

Phase 8 is complete when:

1. Session persistence/recovery works reliably across restarts.
2. Migrations are repeatable and non-destructive.
3. Runtime can operate with local embedded libSQL by default.
4. All Phase 8 tests pass and establish a stable base for Phase 9 retrieval features.
