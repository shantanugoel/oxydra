# Phase 2 Implementation Plan: Provider Trait + OpenAI Chat Completions (Non-Streaming)

## Objective
Implement Phase 2 from `oxydra-project-brief.md` by introducing a stable provider abstraction and a first working OpenAI Chat Completions adapter, while preserving the Phase 1 small-core boundaries and typed contracts.

## Source Alignment (Reviewed)
This plan is derived from the full brief, with primary and cross-phase inputs:
- **Chapter 1**: strict crate boundaries, foundational typed contracts, deterministic config behavior.
- **Chapter 2**: provider abstraction, dynamic dispatch via `#[async_trait]`, `ProviderCaps`, type-first model identity/catalog validation, OpenAI chat normalization.
- **Chapters 3-5**: runtime/tooling compatibility expectations (object-safe trait surface, generic message/tool structures, no provider-specific leakage).
- **Chapter 7**: credential handling expectations (no secret leakage in logs/errors).
- **Appendix / Progressive Build Plan**: Phase 2 verification gate and crate strategy (`types` + `provider`, OpenAI non-streaming first, unknown model validation required).

## Current Baseline (Phase 1 Already Complete)
- Workspace and crate layering are initialized.
- `types` already contains core message/context/response structs, error enums, typed IDs (`ModelId`, `ProviderId`), and tracing init.
- `provider` crate is still a scaffold and needs full Phase 2 implementation.

## Phase 2 Scope
### In Scope
1. `Provider` trait contract (dyn-dispatch friendly) and capability metadata.
2. Typed model/provider catalog + full snapshot population path + startup/request-time validation.
3. OpenAI Chat Completions **non-streaming** provider implementation.
4. Mapping between internal `Message`/`ToolCall` and OpenAI wire format.
5. Test coverage for model validation + successful response normalization + error paths.
6. Provider credential resolution flow (`explicit -> OPENAI_API_KEY -> API_KEY`) with redaction-safe errors.

### Out of Scope (Deferred)
- SSE streaming and stream item pipeline (Phase 3).
- Additional providers (Anthropic/Gemini) and provider switching (Phase 7).
- Retry/backoff/fallback orchestration wrapper (`ReliableProvider`) beyond minimal scaffolding.

## Implementation Work Plan

### 1) Extend `types` crate for provider contracts
**Files likely touched:** `crates/types/src/lib.rs`, `crates/types/src/model.rs` (or split modules), `crates/types/src/error.rs`, new provider/type modules as needed.

Planned additions:
- `ProviderCaps` struct with at least:
  - `supports_streaming`
  - `supports_tools`
  - `supports_json_mode`
  - `supports_reasoning_traces`
  - token window metadata fields (as available)
- `ModelDescriptor` and `ModelCatalog` types for typed model metadata lookup.
- `Provider` trait using `#[async_trait]` and `Send + Sync` compatibility for future `Box<dyn Provider + Send + Sync>` usage.
  - Phase 2-required method: `complete(...) -> Result<Response, ProviderError>`
  - `capabilities(...)` accessor
  - Optional forward-compatible stream method signature stub (without implementing streaming behavior yet).
- `ProviderError` enrichment for transport/auth/status/parse failures without exposing secrets.

Design constraints:
- Keep contracts provider-agnostic and reusable by future provider adapters.
- Do not leak OpenAI-specific payload types into shared public API.

### 2) Implement model validation path
**Files likely touched:** new/updated types and provider modules.

Planned behavior:
- Build a pinned internal model catalog for supported Phase 2 models (OpenAI entries at minimum).
- Validate incoming `ModelId` against catalog before request execution.
- Return deterministic `ProviderError::UnknownModel` on invalid model IDs (required verification gate).

Catalog population approach for Phase 2:
- Add a full pinned model snapshot to the repo now (deterministic, offline-safe), then parse it into typed `ModelDescriptor` values at startup.
- Add a reusable regeneration engine in this phase (library/internal task entrypoint) that can refresh the snapshot from a pinned source format and write the committed artifact.
- Keep the regeneration helper source-driven (input JSON -> canonical snapshot file). Direct network fetch from `https://models.dev/api.json` is deferred to the later operator-facing refresh command in the CLI phase.
- Defer operator-facing CLI exposure (`oxydra models --refresh`) to the dedicated CLI phase later, so Phase 2 crate boundaries stay intact.
- Keep runtime lookup local (committed artifact/in-code), never live-fetching from the network during startup.
- Include fixture tests that verify snapshot parsing, expected required fields, and unknown-model rejection behavior.

### 3) Implement OpenAI non-streaming adapter in `provider`
**Files likely touched:** `crates/provider/Cargo.toml`, `crates/provider/src/lib.rs`, new `openai.rs`/mapping modules, tests.

Planned implementation:
- Add dependencies (latest stable): `reqwest`, `serde`, `serde_json`, `async-trait`, `tracing`, plus `types` crate linkage.
- Introduce `OpenAIProvider` with configuration for:
  - base URL (default OpenAI endpoint)
  - API key input with deterministic resolution (`explicit -> OPENAI_API_KEY -> API_KEY` development fallback)
- Implement request normalization:
  - internal `Context`/`Message` -> OpenAI chat-completions payload
- Implement response normalization:
  - OpenAI `choices[0].message` + tool call payload -> internal `Response`
- Ensure errors are mapped into `ProviderError` variants with actionable but non-secret messages.

### 4) Keep Phase 3/7 compatibility surfaces ready
Planned safeguards:
- Preserve abstraction seams for upcoming streaming support (Phase 3) without partial SSE logic now.
- Keep provider implementation and catalog layout extensible for additional providers and routing/reliability wrappers (Phase 7).
- Maintain tracing spans around provider call lifecycle for later observability expansion.

### 5) Verification and tests (Phase 2 gate)
**Primary gate from brief:** send prompt to OpenAI and get normalized `Response`; unknown model IDs fail validation.

Planned tests:
- `types` unit tests:
  - model catalog lookup/validation behavior
  - pinned catalog parsing fixture behavior
  - provider caps/type serialization where applicable
- `provider` unit/integration-style tests:
  - payload serialization snapshots for chat-completions body
  - response parsing normalization into internal `Response`
  - unknown model failure path
  - credential resolution precedence behavior
  - HTTP/status/auth/parse error mapping
- optional live smoke test (gated by env key) for the official Phase 2 gate: send prompt to OpenAI and assert normalized `Response`.
- Execution command:
  - `cargo test -p types -p provider`
  - (optionally) focused `cargo test` at workspace level if baseline remains green.

## Ordered Execution Sequence
1. Add provider-facing shared types/trait/error updates in `types`.
2. Add pinned model catalog artifact + typed ingestion/validation utilities.
3. Build `OpenAIProvider` request/response mapping in `provider`.
4. Wire validation + provider credential resolution flow end-to-end.
5. Add/adjust tests for all Phase 2 gate conditions.
6. Run phase-targeted tests and fix only Phase 2-related failures.

## Definition of Done
- `Provider` abstraction exists and is dyn-dispatch ready.
- `ProviderCaps` + typed model catalog are implemented, populated, and used for validation.
- OpenAI Chat Completions non-streaming requests/responses are normalized through internal types.
- Unknown model IDs fail deterministically before execution.
- Credential precedence works as specified without secret leakage in errors/logs.
- Phase 2 tests pass in `types` and `provider` crates.
