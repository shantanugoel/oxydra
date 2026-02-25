# Chapter 15: Progressive Build Plan

## Overview

This chapter tracks the implementation status of all 21 phases, documents identified gaps in completed phases, and provides the forward plan for phases 13-21. The plan is designed so no phase requires rewriting work from a previous phase — each phase extends the prior phase's output.

## Phase Status Summary

| Phase | Focus | Status | Notes |
|-------|-------|--------|-------|
| 1 | Types + error hierarchy + tracing | **Complete** | All types, traits, error enums, tracing spans |
| 2 | Provider trait + OpenAI (non-streaming) | **Complete** | Provider trait, OpenAI impl, model catalog. |
| 3 | Streaming support + SSE parsing | **Complete** | OpenAI SSE streaming, StreamItem enum, bounded channels |
| 4 | Tool trait + `#[tool]` macro + core tools | **Complete** | Tool trait, proc macro, file/shell/web/vault tools, LLM-callable memory tools |
| 5 | Agent loop + cancellation + testing | **Complete** | Turn loop, CancellationToken, MockProvider, budget guards |
| 6 | Self-correction + parallel tool execution | **Complete** | Validation error feedback, join_all for ReadOnly tools |
| 7 | Anthropic provider + config + switching | **Complete** | Anthropic impl, figment config, ReliableProvider. |
| 8 | Memory trait + libSQL persistence | **Complete** | Memory trait, libSQL, SQL migrations, session management |
| 9 | Context window + hybrid retrieval | **Complete** | Token budgeting, FTS5, vector search, fastembed/blake3, note storage/deletion API. **Gap: upsert_chunks unimplemented** |
| 10 | Runner + isolation infrastructure | **Complete** | Runner, bootstrap envelope, shell daemon, sandbox tiers. All Phase 10 gaps resolved: daemon mode, log capture, container/microvm bootstrap delivery, Firecracker config generation, control plane metadata. |
| 11 | Security policy + WASM tool isolation | **Complete** | Security policy, SSRF protection, scrubbing. All Phase 11 gaps resolved: real wasmtime + WASI sandboxing with preopened directory enforcement. |
| 12 | Channel trait + TUI + gateway daemon | **Complete** | Channel trait, TUI adapter, gateway WebSocket server, ratatui rendering loop, standalone `oxydra-tui` binary, `runner --tui` exec wiring. **Resolved:** multi-line input (Alt+Enter), runtime activity visibility (`TurnProgress`). **Protocol v2:** `runtime_session_id` renamed to `session_id` throughout; `GatewayClientHello` gains `create_new_session` field; `ToolExecutionContext` threaded as parameter (not shared state) for concurrent-session safety. |
| 13 | Model catalog + provider registry | **Complete** | Provider registry, Gemini, Responses, catalog commands, caps overrides, cached catalog resolution, `skip_catalog_validation` escape hatch, updated CLI (`fetch --pinned`, unfiltered cache) |
| 14 | External channels + identity mapping | Planned | |
| 15 | Multi-agent orchestration | Planned | |
| 16 | Observability (OpenTelemetry) | Planned | |
| 17 | MCP support | Planned | |
| 18 | Session lifecycle controls | Planned | |
| 19 | Scheduler system | **Complete** | Durable store, polling executor, 4 LLM tools, conditional notification, cron/interval/once cadences |
| 20 | Skill system | Planned | |
| 21 | Persona governance | Planned | |

## Identified Gaps in Completed Phases

Three gaps were identified during the initial code review of phases 1-12, and two additional gaps were identified in a subsequent review. All five are documented here and incorporated into the forward plan.

### Gap 1: MemoryRetrieval::upsert_chunks (Phase 9)

**What was planned:** A `upsert_chunks` method on the `MemoryRetrieval` trait for batch-indexing document chunks into the vector/FTS stores.

**What was built:** The method signature exists in `LibsqlMemory`'s implementation of `MemoryRetrieval`, but the body returns `Err("not implemented")`. Document indexing works through the message storage path (conversation events are stored and retrievable), but the explicit chunk upsert API is non-functional.

**Impact:** The hybrid retrieval pipeline works for conversation-based content. External document ingestion through `upsert_chunks` is not available. This blocks future use cases like codebase indexing or document upload.

**Fix scope:** Implement the method body — embed chunk text via the existing embedding pipeline, insert into `chunks` + `chunks_vec` tables, and update the FTS5 index. All the infrastructure (embedding backends, table schema, SQL migrations) already exists.

## Completed Phases: Detail

### Phase 1: Types + Error Hierarchy + Tracing

**Crates:** `types`

Built the foundation layer with zero internal dependencies:
- Universal structs: `Message`, `ToolCall`, `Context`, `Response`, `UsageUpdate`
- All trait definitions: `Provider`, `Tool`, `Memory`, `MemoryRetrieval`, `Channel`
- Error hierarchy: `ProviderError`, `ToolError`, `RuntimeError`, `MemoryError`, `ChannelError`, `ConfigError`
- Newtype identifiers: `ModelId(String)`, `ProviderId(String)`
- Config schema structs for all crates
- Runner protocol types (bootstrap envelope, shell daemon frames)
- Gateway protocol frames (`GatewayClientFrame`, `GatewayServerFrame`)
- `tracing` spans established from day one

### Phase 2: Provider Trait + OpenAI (Non-Streaming)

**Crates:** `types`, `provider`

- `Provider` trait with `#[async_trait]`, `complete()`, `stream()`, `capabilities()`, `catalog_provider_id()` methods
- `ProviderCaps` struct declaring feature support (including `max_context_tokens`)
- `OpenAiProvider` implementing the trait against `/v1/chat/completions`
- `ModelCatalog` loaded from embedded JSON snapshot, organized by catalog provider namespace
- `ModelDescriptor` fully aligned with models.dev schema (capability flags, pricing, limits, modalities)
- Model selection validated against catalog at startup

### Phase 3: Streaming + SSE Parsing

**Crates:** `provider`

- `StreamItem` enum: `Text`, `ToolCallDelta`, `ReasoningDelta`, `UsageUpdate`, `FinishReason`, `ConnectionLost` (added during TUI phases), `Progress(RuntimeProgressEvent)` (added for runtime activity visibility)
- OpenAI SSE parser handling `data: [DONE]`, chunked tool call deltas, and usage updates
- Bounded `mpsc` channels for backpressure between SSE parser and consumer
- Tool chunk accumulator for reconstructing fragmented JSON arguments

### Phase 4: Tool Trait + Macro + Core Tools

**Crates:** `types`, `tools`, `tools-macros`

- `Tool` trait: `schema()`, `execute()`, `timeout()`, `safety_tier()`
- `#[tool]` procedural macro generating `FunctionDecl` from function signatures and doc comments
- Core tools: file read/write/edit/delete/search/list, bash, web fetch/search, vault copy
- `SafetyTier` enum: `ReadOnly`, `SideEffecting`, `Privileged`
- `ToolRegistry` with policy enforcement (timeout, output truncation, safety tier gating)
- LLM-callable memory tools: `memory_search` (ReadOnly), `memory_save`, `memory_update`, `memory_delete` (SideEffecting) — per-user scoped note management with UUID-based `note_id` identifiers, per-turn `ToolExecutionContext` for user/session propagation, registered during bootstrap when memory is configured

### Phase 5: Agent Loop + Cancellation + Testing

**Crates:** `runtime`

- `AgentRuntime` struct holding provider, tool registry, limits, memory
- `run_session` / `run_session_internal` turn loop
- `TurnState` enum: `Streaming`, `ToolExecution`, `Yielding`, `Cancelled`
- `CancellationToken` threaded through all async operations via `tokio::select!`
- Per-turn timeout, max-turn budget, max-cost budget enforcement
- `MockProvider` and `MockTool` for deterministic testing

### Phase 6: Self-Correction + Parallel Execution

**Crates:** `runtime`

- Validation guard catching `serde_json` deserialization errors
- Error formatted as tool result message and injected back into context
- Parallel execution of contiguous `ReadOnly` tools via `futures::future::join_all`
- Sequential execution of `SideEffecting`/`Privileged` tools with ReadOnly batch flushing

### Phase 7: Anthropic Provider + Config + Switching

**Crates:** `provider`, `types`, `tui`

- `AnthropicProvider` implementing the Provider trait against `/v1/messages`
- Anthropic block format serialization (content blocks, tool use blocks)
- `figment` config loader with 6-layer precedence (defaults < system < user < workspace < env < CLI)
- `config_version` field with validation
- `ReliableProvider` wrapper: exponential backoff, retry with jitter, fallback chains
- Provider switching via config without code changes
- Credential resolution: 4-tier (explicit config → custom env var → provider-type env var → generic `API_KEY` fallback)

### Phase 8: Memory Trait + libSQL Persistence

**Crates:** `memory`

- `Memory` trait: `store`, `recall` methods
- Embedded `libsql` in local file mode
- 6 tables: `sessions`, `conversation_events`, `session_state`, `files`, `chunks`, `chunks_vec`
- Versioned SQL migrations
- Session create/restore by ID
- Conversation events stored as JSON with sequence numbers

### Phase 9: Context Window + Hybrid Retrieval

**Crates:** `memory`, `runtime`

- `tiktoken-rs` token counting with `cl100k_base` encoding
- Context budget: system + memory + history + tools + safety buffer
- History filled newest-first, truncated when budget exhausted
- Rolling summarization triggered by configurable token ratio threshold
- `MemoryRetrieval` trait with hybrid search (vector cosine similarity + FTS5 BM25)
- `MemoryRetrieval` extended with `store_note` and `delete_note` methods for direct note management (per-user scoped, UUID-based note identifiers, chunk metadata tagging)
- `fastembed-rs` local embeddings (384-dimensional, ONNX-based)
- `blake3` deterministic fallback (64-dimensional)
- `chunks_fts` FTS5 virtual table for keyword search
- **Gap:** `upsert_chunks` method returns "not implemented"

**Gaps / follow-ups to complete Phase 9:**
- **Memory retrieval upserts:** implement `upsert_chunks` to index external chunks into `chunks`/`chunks_vec`/FTS tables.

### Phase 10: Runner + Isolation Infrastructure

**Crates:** `types`, `runner`, `tools`, `runtime`, `shell-daemon`

- `SandboxTier` enum: `MicroVm`, `Container`, `Process`
- Runner global config: workspace root, user map, default tier, guest image references
- Runner per-user config: mount paths, resource limits, credential references
- Per-user workspace provisioning: `<workspace_root>/<user_id>/{shared,tmp,vault}`
- Length-prefixed JSON bootstrap envelope from runner to `oxydra-vm` via startup socket
- Shell daemon protocol: `SpawnSession`, `ExecCommand`, `StreamOutput`, `KillSession`
- `ShellSession` / `BrowserSession` traits with `VsockShellSession` and `LocalProcessShellSession`
- `--insecure` mode: Process tier, no VM/container, shell/browser tools disabled, warning emitted

### Phase 11: Security Policy + WASM Tool Isolation

**Crates:** `tools`, `runtime`

- `HostWasmToolRunner` with capability-scoped mount profiles per tool class
- File tools: `shared`+`tmp`+`vault` read-only for reads, `shared`+`tmp` read-write for writes
- Web tools: no FS mounts, host-function HTTP proxy only
- SSRF protection: hostname resolution before blocklist checks, rejecting loopback, link-local, RFC1918, metadata endpoints
- Vault copy: two sequential invocations (no single invocation gets both vault and shared/tmp)
- `SecurityPolicy`: command allowlist, path traversal blocking, HITL gates
- `RegexSet` output scrubbing for credentials (api_key, token, password, bearer patterns)
- Entropy-based secret detection (Shannon entropy >= 3.8)

### Phase 12: Channel Trait + TUI + Gateway Daemon

**Crates:** `types`, `channels`, `gateway`, `tui`, `runner`

- `Channel` trait: `send`, `listen`, `health_check`
- `ChannelRegistry` with thread-safe `RwLock<BTreeMap>` storage
- `GatewayServer`: Axum WebSocket server on `/ws` endpoint
- `GatewayClientFrame` / `GatewayServerFrame` typed JSON protocol
- Session management via `UserSessionState` with per-user broadcast channels
- `RuntimeGatewayTurnRunner` bridging gateway and runtime
- Turn cancellation via `CancelActiveTurn` frame → `CancellationToken`
- Reconnection support with `latest_terminal_frame` buffer
- `TuiChannelAdapter`: WebSocket client implementing `Channel` trait
- `TuiUiState` for UI state tracking
- Gateway endpoint discovery via `tmp/gateway-endpoint` marker file
- `--tui` mode: connect-only to existing `oxydra-vm`, exit with error if no guest running
- `TuiViewModel`: rendering-only view model with message history (capped at 1000), scroll management, input buffer, `ConnectionState` enum
- `MessagePane`, `InputBar`, `StatusBar`: ratatui widgets with per-role styling, streaming cursor, disabled-state visuals
- `EventReader`: dedicated blocking thread for crossterm input → `AppAction` mapping via `mpsc` channel
- `TuiApp`: main `tokio::select!` loop with WebSocket split transport (reader/writer tasks), Hello handshake, reconnection with exponential backoff + jitter, `TerminalGuard` RAII for terminal safety, single-point-of-draw rendering
- `oxydra-tui`: standalone binary entry point (clap CLI, UUID connection ID, multi-threaded tokio runtime)
- `runner --tui` execs `oxydra-tui` binary; `--probe` flag preserves the old print-and-exit behavior
- **Multi-line input (Issue 6a):** Alt+Enter inserts a literal newline into the input buffer. The input bar expands vertically to accommodate multiple lines (up to 10 rows). Cursor position accounting handles logical newlines and visual wrapping.
- **Runtime activity visibility (Issue 6b):** `RuntimeProgressEvent` / `RuntimeProgressKind` types added to `StreamItem`. The runtime emits `StreamItem::Progress` at `ProviderCall` and `ToolExecution` transitions. The gateway turn runner forwards them as `GatewayServerFrame::TurnProgress` frames. `TuiUiState.activity_status` tracks the latest message and it appears in the input bar title while a turn is active.
- **Protocol v2 update:** `GATEWAY_PROTOCOL_VERSION` bumped from 1 to 2. `runtime_session_id` renamed to `session_id` in all protocol types (`GatewaySession`, `GatewayClientHello`, `GatewaySendTurn`, `GatewayCancelActiveTurn`) and all internal state (`UserSessionState`, `TuiUiState`, `RunnerTuiConnection`). `GatewayClientHello` gains `create_new_session: bool` field for explicit session creation control.
- **Tool execution context race fix:** `ToolExecutionContext` removed from `AgentRuntime` shared state (`Arc<Mutex<>>`). Now threaded as a parameter through `run_session_internal` → `execute_tool_call` → `execute_with_context`. The gateway's `RuntimeGatewayTurnRunner` constructs context per-turn and passes it to the runtime, eliminating the race condition that would occur with concurrent sessions.
- **Multi-session gateway core:** Gateway refactored from single-session-per-user (`UserSessionState`) to multi-session model (`UserState` → `SessionState`). Users can have multiple concurrent sessions with independent active turns. Per-user concurrent turn limit enforced via `AtomicU32`. Internal API extracted for in-process channel adapters (D10): `create_or_get_session()`, `submit_turn()`, `cancel_session_turn()`, `subscribe_events()`, `list_user_sessions()`. Broadcast buffer increased from 256 to 1024. UUID v7 session IDs for new sessions; deterministic `runtime-{user_id}` preserved for backward compatibility.
- **Session persistence:** `SessionStore` trait defined in `types` crate. `LibsqlSessionStore` implemented in `memory` crate using `gateway_sessions` table (migration 0020). Sessions persisted on creation, touched on turn completion, resumable from store after restart. Wired into `GatewayServer` via `with_session_store()` constructor and into `runner` bootstrap via `VmBootstrapRuntime.session_store`.


### Phase 13: Model Catalog Governance + Provider Flexibility

**Crates:** `types`, `provider`, `runner`

- **Provider registry:** Replaced flat per-provider config blocks (`providers.openai`, `providers.anthropic`) with `providers.registry.<name>` map of `ProviderRegistryEntry` — each entry has `provider_type`, `base_url`, `api_key`, `api_key_env`, `extra_headers`, and optional `catalog_provider`
- **`ProviderType` enum:** `Openai`, `Anthropic`, `Gemini`, `OpenaiResponses` (serde as `snake_case`)
- **Default registry:** Ships with `"openai"` and `"anthropic"` entries; users add custom entries for proxies, Gemini, Responses, etc.
- **4-tier credential resolution:** explicit `api_key` → custom `api_key_env` → provider-type default env var (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`) → generic `API_KEY` fallback
- **Catalog provider mapping:** `effective_catalog_provider()` defaults by type (`Openai` → `"openai"`, `Gemini` → `"google"`, etc.) or uses explicit `catalog_provider` override
- **`build_provider()` factory:** Constructs providers from registry entries, resolving API key and base URL, dispatching to the correct provider implementation
- **Models.dev-aligned `ModelDescriptor`:** Full schema alignment — `id`, `name`, `family`, `attachment`, `reasoning`, `tool_call`, `interleaved`, `structured_output`, `temperature`, `knowledge`, `release_date`, `last_updated`, `modalities`, `open_weights`, `cost`, `limit`
- **Capability overrides:** `CapsOverrides` with `provider_defaults` and per-model `overrides` keyed as `"provider/model"`; `derive_caps()` merges baseline → provider defaults → model-specific overlays; `is_deprecated()` for deprecation tracking
- **`ProviderCaps` extended:** Added `max_context_tokens` alongside `max_input_tokens`/`max_output_tokens`
- **Google Gemini provider (`gemini.rs`):** Full implementation with real SSE streaming, `v1beta/models/{model}:generateContent` and `streamGenerateContent?alt=sse`, `x-goog-api-key` header authentication
- **OpenAI Responses provider (`responses.rs`):** `/v1/responses` endpoint with `previous_response_id` session chaining via `Arc<Mutex<Option<String>>>`, typed input/output blocks, fallback to full context on chaining errors, `ResponsesToolCallAccumulator`
- **Catalog snapshot commands:** `runner catalog fetch` (fetch from models.dev, filter, write snapshot + overrides), `runner catalog verify` (re-fetch and compare), `runner catalog show` (pretty-print summary)
- **Config validation:** `AgentConfig::validate()` resolves provider via registry; `validate_model_in_catalog()` uses `effective_catalog_provider()`; `ConfigError::UnknownModelForCatalogProvider` replaces old model validation errors


### Phase 19: Scheduler System

**Crates:** `types`, `memory`, `tools`, `runtime`, `gateway`, `runner`, `tui`

Built a complete durable scheduler that lets the LLM create and manage one-off and periodic tasks:

- **Types** (`types/src/scheduler.rs`): `ScheduleCadence` (Cron/Once/Interval), `NotificationPolicy` (Always/Conditional/Never), `ScheduleStatus` (Active/Paused/Completed/Disabled), `ScheduleDefinition`, `ScheduleRunRecord`, `ScheduleSearchFilters`, `ScheduleSearchResult`, `SchedulePatch`
- **Errors** (`types/src/error.rs`): `SchedulerError` enum with InvalidCronExpression, InvalidCadence, NotFound, Store, Execution, LimitExceeded, Unauthorized variants; `Scheduler(SchedulerError)` variant added to `RuntimeError`
- **Configuration** (`types/src/config.rs`): `SchedulerConfig` with `enabled` (default false), `poll_interval_secs`, `max_concurrent`, `max_schedules_per_user`, `max_turns`, `max_cost`, `max_run_history`, `min_interval_secs`, `default_timezone`, `auto_disable_after_failures`; added as `#[serde(default)]` field on `AgentConfig`
- **Database schema** (`memory/migrations/`): Three new migrations — `0017_create_schedules_table.sql`, `0018_create_schedules_indexes.sql` (4 indexes), `0019_create_schedule_runs_table.sql` with cascading delete
- **SchedulerStore** (`memory/src/scheduler_store.rs`): `SchedulerStore` trait with 10 methods; `LibsqlSchedulerStore` implementation using libSQL with dynamic WHERE clause construction for search, pagination support, and atomic `record_run_and_reschedule`
- **Cadence evaluation** (`memory/src/cadence.rs`): `next_run_for_cadence()`, `validate_cadence()`, `parse_cadence()`, `format_in_timezone()` using `cron` and `chrono-tz` crates
- **Scheduler tools** (`tools/src/scheduler_tools.rs`): `schedule_create` (SideEffecting), `schedule_search` (ReadOnly), `schedule_edit` (SideEffecting), `schedule_delete` (SideEffecting) — registered via `register_scheduler_tools()` during bootstrap when scheduler is enabled
- **SchedulerExecutor** (`runtime/src/scheduler_executor.rs`): Background polling loop with `ScheduledTurnRunner` and `SchedulerNotifier` traits; handles notification routing (Always/Conditional/Never), one-shot completion, failure tracking, auto-disable, history pruning
- **Gateway integration** (`gateway/src/lib.rs`): `SchedulerNotifier` implementation on `GatewayServer`; `ScheduledTurnRunner` implementation on `RuntimeGatewayTurnRunner`; `GatewayServerFrame::ScheduledNotification` variant
- **Runner bootstrap** (`runner/src/bootstrap.rs`): `scheduler_store` field on `VmBootstrapRuntime`; conditional scheduler tool registration; system prompt augmentation with scheduler instructions
- **oxydra-vm wiring** (`runner/src/bin/oxydra-vm.rs`): Executor spawned as `tokio::spawn` background task; `CancellationToken` cancelled on shutdown
- **TUI support** (`tui/src/channel_adapter.rs`, `tui/src/ui_model.rs`): `ScheduledNotification` handling in match arms; `last_scheduled_notification` field; display as system messages in chat history
- **Dependencies**: `cron = "0.15"` and `chrono-tz = "0.10"` in memory crate; `tokio-util` in runner crate
- **Tests**: 19 scheduler store and cadence tests in memory crate; 7 scheduler executor tests in runtime crate with mock store, runner, and notifier


## Forward Plan: Phases 14-21

### Phase 14: External Channels + Identity Mapping

**Crates:** `types`, `channels`, `gateway`, `runtime`, `memory`
**Builds on:** Phases 8, 12

**Scope:**
- Implement first external channel adapter (feature-flagged)
- Per-user per-channel sender-ID allowlists with default-deny ingress
- Canonical session identity mapping: `(user_id, channel_id, channel_session_id) → session_id`
- Reconnects reuse same workspace and memory namespace
- Rejected senders audited

**Verification gate:** Messages from external channel trigger agent responses through gateway routing; identity maps deterministically; unauthorized senders rejected and audited.

### Phase 15: Multi-Agent Orchestration

**Crates:** `runtime`, `gateway`
**Builds on:** Phases 5, 12, 14

**Scope:**
- `SubagentBrief` struct for bounded context handoffs (goal, key facts, tool scope, budget)
- Subagent spawning with child `CancellationToken`, per-agent timeout and cost budget
- Lane-based per-user queueing replacing mutex-based rejection
- Gateway session tree tracking parent-child agent relationships

**Verification gate:** One agent delegates to a subagent successfully; lane-based queueing works under concurrent multi-channel load.

### Phase 16: Observability (OpenTelemetry)

**Crates:** `runtime`, `gateway`
**Builds on:** Phases 5, 15

**Scope:**
- Swap `tracing-subscriber` for `tracing-opentelemetry` OTLP exporter
- Distributed traces: turn → provider_call → tool_execution span tree
- Metrics: turn duration, token consumption, cost attribution, tool reliability
- Per-session cost reporting
- Trace context propagation through subagent handoffs

**Verification gate:** Full trace of agent turns visible in Jaeger; per-session cost report; tool success/failure rates exported.

### Phase 17: MCP Support

**Crates:** `tools`, `runtime`
**Builds on:** Phases 4, 5

**Scope:**
- `McpToolAdapter` implementing `Tool` trait
- MCP client supporting stdio and HTTP+SSE transports
- Discovery via configuration (server name, transport, command/URL, safety tier)
- MCP tools registered alongside native tools in same `ToolRegistry`
- Same policy enforcement (timeout, scrubbing, safety tier) as native tools

**Verification gate:** External MCP tool servers discoverable and callable alongside native tools.

### Phase 18: Session Lifecycle Controls

**Crates:** `runtime`, `memory`, `types`, `tui`, `channels`
**Builds on:** Phases 8, 13, 14

**Scope:**
- Explicit "new session" command available through all channels
- Generates new canonical `session_id` preserving user/workspace continuity
- Previous sessions remain recallable for audit
- No implicit rollover heuristics

**Verification gate:** User can start a clean new session; previous sessions remain recallable; session_id rollover is deterministic and auditable.

### Phase 19: Scheduler System — ✅ Complete

**Crates:** `types`, `memory`, `tools`, `runtime`, `gateway`, `runner`, `tui`
**Builds on:** Phases 8, 12, 13

**Implemented:**
- `SchedulerStore` with durable schedule definitions (one-off + periodic via cron, interval, and once cadences)
- `SchedulerExecutor` evaluating due entries on polling interval and dispatching as runtime turns
- Schedule management tools: `schedule_create`, `schedule_search`, `schedule_edit`, `schedule_delete`
- Silent execution support (always/conditional/never notification policies)
- Execution through same policy envelope as interactive turns
- Automatic schedule lifecycle management: one-shot completion, failure tracking, auto-disable
- System prompt augmentation with scheduler tool instructions
- TUI support for displaying scheduled notifications

**Verification gate:** ✅ One-off and periodic entries execute bounded turns with normal policy/audit controls; conditional notification works correctly; all tests pass.

### Phase 20: Skill System

**Crates:** `runtime`, `tools`, `types`, `memory`
**Builds on:** Phases 11, 18, 19

**Scope:**
- `SkillLoader` discovering markdown manifests from `<workspace_root>/<user_id>/skills/`
- `SkillRegistry` for runtime access
- API-mediated skill interaction (not raw filesystem traversal)
- LLM can create/load/use skills; deletion denied by policy

**Verification gate:** Skills discovered and loaded from managed folder; LLM can create/load/use; deletion denied.

### Phase 21: Persona Governance

**Crates:** `runtime`, `types`, `runner`, `gateway`
**Builds on:** Phases 12, 18, 20

**Scope:**
- `SOUL.md` and `SYSTEM.md` parsing and validation
- Loaded at session start, injected into system prompt
- Non-editable by LLM (write/edit/delete rejected by security policy)
- Effective precedence surfaced in diagnostics

**Verification gate:** Persona files load at session start with visible precedence; LLM mutation attempts rejected; operator edits apply on next session start.

## Gap Resolution Plan

The five identified gaps are incorporated as additional work items:

| Gap | Affected Phase | Resolution Target | Priority |
|-----|---------------|-------------------|----------|
| upsert_chunks implementation | Phase 9 | Before Phase 20 (skill/document indexing needs chunk ingestion) | Medium |
| Docker container log capture | Phase 10 | Non-blocking; wire bollard `logs()` streaming to log files for container-tier guests | Low |

These gaps do not block any currently completed phase's functionality but should be resolved before the phases that depend on them.

## Why This Order Avoids Rewrites

- **Phase 1 types** are used unchanged through Phase 21
- **Phase 2 Provider trait** — Phase 7 added Anthropic without changing the trait; future providers follow the same pattern
- **Phase 4 Tool trait** with `safety_tier()` and `timeout()` — Phase 6 uses these for parallel execution, Phase 11 for WASM policy, Phase 17 for MCP; memory tools reuse the same `Tool` trait and `ToolRegistry` infrastructure without any trait changes
- **Phase 5 CancellationToken** — Phase 15 reuses for subagent lifecycle management
- **Phase 1 tracing** — Phase 16 upgrades subscriber to OpenTelemetry without changing instrumentation code
- **Phase 8 SQL migrations** — Phase 9 added vector/FTS tables via migration, not schema wipe
- **Phase 12 Channel trait** — Phase 14 extends with external adapters without changing the trait
- **Phase 14 identity mapping** — prevents multi-channel history fragmentation before Phase 15 concurrency
- **Phase 19 scheduler** — reuses runtime policy rails (budget, cancellation, tool gating); stores in existing memory DB via migrations; uses same `ToolRegistry` and `Tool` trait as native tools
- **Phases 20-21** — separate skills from persona policy to prevent LLM mutation of behavioral boundaries

## Test Strategy

- **Phases 1-4** — type/serialization tests, schema tests, provider contract tests
- **Phases 5-11** — runtime integration tests for cancellation, retries, tool safety, memory migration, runner isolation, WASM policy
- **Phases 12-21** — end-to-end tests for TUI/channel adapters, gateway routing, observability, MCP, catalog governance, session controls, scheduler, skills, persona policy
- **Regression policy** — cumulative; later phases add tests but never remove earlier checks
- **CI enforcement** — every phase verification gate is represented by automated tests; `cargo deny`, `clippy`, `rustfmt`, and `cargo test` must all pass
