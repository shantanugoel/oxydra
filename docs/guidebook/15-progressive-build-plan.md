# Chapter 15: Progressive Build Plan

## Overview

This chapter tracks the implementation status of all 21 phases, documents identified gaps in completed phases, and provides the forward plan for phases 13-21. The plan is designed so no phase requires rewriting work from a previous phase — each phase extends the prior phase's output.

## Phase Status Summary

| Phase | Focus | Status | Notes |
|-------|-------|--------|-------|
| 1 | Types + error hierarchy + tracing | **Complete** | All types, traits, error enums, tracing spans |
| 2 | Provider trait + OpenAI (non-streaming) | **Complete** | Provider trait, OpenAI impl, model catalog. **Gap: ResponsesProvider not implemented** |
| 3 | Streaming support + SSE parsing | **Complete** | OpenAI SSE streaming, StreamItem enum, bounded channels |
| 4 | Tool trait + `#[tool]` macro + core tools | **Complete** | Tool trait, proc macro, file/shell/web/vault tools |
| 5 | Agent loop + cancellation + testing | **Complete** | Turn loop, CancellationToken, MockProvider, budget guards |
| 6 | Self-correction + parallel tool execution | **Complete** | Validation error feedback, join_all for ReadOnly tools |
| 7 | Anthropic provider + config + switching | **Complete** | Anthropic impl, figment config, ReliableProvider. **Gap: streaming is a shim** |
| 8 | Memory trait + libSQL persistence | **Complete** | Memory trait, libSQL, SQL migrations, session management |
| 9 | Context window + hybrid retrieval | **Complete** | Token budgeting, FTS5, vector search, fastembed/blake3. **Gap: upsert_chunks unimplemented** |
| 10 | Runner + isolation infrastructure | **Complete** | Runner, bootstrap envelope, shell daemon, sandbox tiers. All Phase 10 gaps resolved: daemon mode, log capture, container/microvm bootstrap delivery, Firecracker config generation, control plane metadata. |
| 11 | Security policy + WASM tool isolation | **Complete** | Security policy, SSRF protection, scrubbing. All Phase 11 gaps resolved: real wasmtime + WASI sandboxing with preopened directory enforcement. |
| 12 | Channel trait + TUI + gateway daemon | **Complete** | Channel trait, TUI adapter, gateway WebSocket server. **Gaps:** TUI rendering + interactive client missing; `runner --tui` is probe-only |
| 13 | Model catalog + provider registry | Planned | |
| 14 | External channels + identity mapping | Planned | |
| 15 | Multi-agent orchestration | Planned | |
| 16 | Observability (OpenTelemetry) | Planned | |
| 17 | MCP support | Planned | |
| 18 | Session lifecycle controls | Planned | |
| 19 | Scheduler system | Planned | |
| 20 | Skill system | Planned | |
| 21 | Persona governance | Planned | |

## Identified Gaps in Completed Phases

Three gaps were identified during the initial code review of phases 1-12, and two additional gaps were identified in a subsequent review. All five are documented here and incorporated into the forward plan.

### Gap 1: Anthropic SSE Streaming (Phase 7)

**What was planned:** Real SSE streaming from Anthropic's event stream API, parsing `content_block_delta`, `message_delta`, and other Anthropic-specific SSE event types.

**What was built:** The Anthropic provider's `stream()` method calls `complete()` internally and wraps the full response as synthetic stream items. It produces correct output but does not actually stream — the user sees the full response arrive at once rather than token-by-token.

**Impact:** Functional correctness is unaffected. User experience degrades for long responses (no progressive display). Token budget enforcement cannot react mid-stream.

**Fix scope:** Implement a real SSE parser for Anthropic's `/v1/messages` streaming endpoint, handling `message_start`, `content_block_start`, `content_block_delta`, `message_delta`, and `message_stop` event types. The existing `StreamItem` enum already supports all needed variants.

### Gap 2: TUI Visual Rendering (Phase 12)

**What was planned:** A `ratatui` + `crossterm` terminal rendering loop providing a full visual terminal interface.

**What was built:** The TUI crate contains the `TuiChannelAdapter` (WebSocket client to gateway), `TuiUiState` (state tracking), and all gateway connection logic. The gateway WebSocket communication works end-to-end. However, the actual terminal rendering loop — drawing widgets, handling keyboard input visually, scrolling — is missing.

**Impact:** The TUI can communicate with the gateway programmatically but cannot present a visual interface to the user. The channel adapter and state management are complete; only the rendering layer is absent.

**Fix scope:** Implement the `ratatui` rendering loop with `crossterm` event handling. The state management (`TuiUiState`) and gateway communication are already in place and can drive the rendering directly.

### Gap 3: MemoryRetrieval::upsert_chunks (Phase 9)

**What was planned:** A `upsert_chunks` method on the `MemoryRetrieval` trait for batch-indexing document chunks into the vector/FTS stores.

**What was built:** The method signature exists in `LibsqlMemory`'s implementation of `MemoryRetrieval`, but the body returns `Err("not implemented")`. Document indexing works through the message storage path (conversation events are stored and retrievable), but the explicit chunk upsert API is non-functional.

**Impact:** The hybrid retrieval pipeline works for conversation-based content. External document ingestion through `upsert_chunks` is not available. This blocks future use cases like codebase indexing or document upload.

**Fix scope:** Implement the method body — embed chunk text via the existing embedding pipeline, insert into `chunks` + `chunks_vec` tables, and update the FTS5 index. All the infrastructure (embedding backends, table schema, SQL migrations) already exists.

### ~~Gap 4: WASM Tool Isolation is Simulated (Phase 11)~~ — Resolved

**What was planned:** Tools execute inside `wasmtime` WASM sandboxes with strict WASI capability grants. File tools get preopened directory mounts, web tools get no filesystem access, and the guest physically cannot access unmounted paths.

**What was built (original):** The `HostWasmToolRunner` enforced capability-scoped mount policies via host-side `std::fs` calls — logical isolation rather than hardware-enforced WASM sandboxing.

**Resolution:** `WasmWasiToolRunner` now executes all file tool operations inside a `wasmtime` sandbox with WASI preopened directories as the hard security boundary. Key details:

- `crates/wasm-guest/` — a new `wasm32-wasip1` binary implementing all eight file operations (`file_read`, `file_write`, `file_edit`, `file_delete`, `file_list`, `file_search`, `vault_copyto_read`, `vault_copyto_write`) using only WASI `std::fs`, compiled and embedded at build time via `crates/sandbox/build.rs` + `include_bytes!`
- Per-profile WASI preopened directories enforce the mount boundary: the guest physically cannot open paths outside its preopened FDs regardless of path construction
- Epoch-based interruption (~10 s timeout) and `StoreLimitsBuilder` (64 MB memory cap) bound runaway guests
- Host-side capability profile checks are retained as defense-in-depth (fast path before WASM instantiation cost)
- Web tools (`web_fetch`, `web_search`) continue to execute host-side via `reqwest` — they have no filesystem access by design
- `WasmWasiToolRunner` is feature-gated behind `wasm-isolation` (default on); `HostWasmToolRunner` remains as the fallback when the feature is disabled
- **TOCTOU eliminated**: the WASI boundary makes symlink-escape and check-to-use races structurally impossible for file tools

### Gap 5: OpenAI Responses API Provider (Phase 2)

**What was planned:** A `ResponsesProvider` implementation targeting OpenAI's `/v1/responses` endpoint with stateful session chaining via `previous_response_id`. This API persists conversation state server-side, reducing token transmission overhead for long multi-turn sessions. The project brief described `input` array format, typed `output` items, built-in tool declarations, and distinct SSE event types.

**What was built:** Only the `OpenAIProvider` exists, targeting the stateless `/v1/chat/completions` endpoint. The full conversation history is re-sent on every turn. No `ResponsesProvider` struct, no `/v1/responses` endpoint usage, no `previous_response_id` tracking, and no Responses API SSE event parsing exist in the codebase.

**Impact:** Functional correctness is unaffected — the chat completions API handles all current use cases. Token efficiency degrades for long multi-turn sessions since the full history must be transmitted every turn (mitigated by the rolling summarization in the memory layer). The Responses API's server-side state management would be particularly valuable for multi-agent subagent delegation (clean context isolation without duplicating memory payloads).

**Fix scope:** Implement `ResponsesProvider` as a new provider struct in `crates/provider/`, targeting `/v1/responses`. Track `previous_response_id` in session state. Implement the `input` array serialization format and `output` item parsing. Add a separate SSE parser branch for Responses API event types (`response.output_item.added`, `response.output_text.delta`, etc.). The existing `Provider` trait, `StreamItem` enum, and reliability wrapper all support this without modification.

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

- `Provider` trait with `#[async_trait]`, `complete()`, `stream()`, `capabilities()` methods
- `ProviderCaps` struct declaring feature support
- `OpenAiProvider` implementing the trait against `/v1/chat/completions`
- `ModelCatalog` loaded from embedded JSON snapshot (8 models: Claude 3.5 Haiku/Sonnet, GPT-4.1/4.1-mini/4.1-nano/4o/4o-mini, o3-mini)
- Model selection validated against catalog at startup
- **Gap:** `ResponsesProvider` for OpenAI's stateful `/v1/responses` API was planned but not implemented; only the stateless chat completions endpoint is used

**Gaps / follow-ups to complete Phase 2:**
- **OpenAI Responses API provider:** add `ResponsesProvider`, SSE parsing for `/v1/responses`, and session-level `previous_response_id` handling for server-side state.

### Phase 3: Streaming + SSE Parsing

**Crates:** `provider`

- `StreamItem` enum: `Text`, `ToolCallDelta`, `ReasoningDelta`, `UsageUpdate`, `FinishReason`
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
- Credential resolution: explicit config → provider env var → generic fallback
- **Gap:** `stream()` is a shim wrapping `complete()`

**Gaps / follow-ups to complete Phase 7:**
- **Anthropic streaming:** implement true SSE streaming for `/v1/messages` with `content_block_delta` and related event types.

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
- `fastembed-rs` local embeddings (384-dimensional, ONNX-based)
- `blake3` deterministic fallback (64-dimensional)
- `chunks_fts` FTS5 virtual table for keyword search
- **Gap:** `upsert_chunks` method returns "not implemented"

**Gaps / follow-ups to complete Phase 9:**
- **Memory retrieval upserts:** implement `upsert_chunks` to index external chunks into `chunks`/`chunks_vec`/FTS tables.

### Phase 10: Runner + Isolation Infrastructure

**Crates:** `types`, `runner`, `sandbox`, `tools`, `runtime`, `shell-daemon`

- `SandboxTier` enum: `MicroVm`, `Container`, `Process`
- Runner global config: workspace root, user map, default tier, guest image references
- Runner per-user config: mount paths, resource limits, credential references
- Per-user workspace provisioning: `<workspace_root>/<user_id>/{shared,tmp,vault}`
- Length-prefixed JSON bootstrap envelope from runner to `oxydra-vm` via startup socket
- Shell daemon protocol: `SpawnSession`, `ExecCommand`, `StreamOutput`, `KillSession`
- `ShellSession` / `BrowserSession` traits with `VsockShellSession` and `LocalProcessShellSession`
- `--insecure` mode: Process tier, no VM/container, shell/browser tools disabled, warning emitted

**Phase 10 gaps — resolved:**
- ~~**Container/microvm bootstrapping:**~~ Bootstrap envelope is now written to a file (`workspace.tmp/bootstrap/runner_bootstrap.json`) before launch and bind-mounted into containers at `/run/oxydra/bootstrap` with `OXYDRA_BOOTSTRAP_FILE` env var. MicroVM guests receive it via base64-encoded `boot_args` injection into Firecracker config. macOS microvm inherits the container path via Docker.
- ~~**Runner control plane:**~~ `--daemon` flag added; daemon loop binds a Unix control socket, serves health/shutdown RPCs via `serve_control_unix_listener`, captures logs from stdout/stderr via log pump threads, and cleans up the socket on shutdown.
- ~~**Linux microvm config:**~~ `RunnerGuestImages` now has dedicated `firecracker_oxydra_vm_config` and `firecracker_shell_vm_config` fields (separate from OCI image tags). `launch_microvm_linux` validates their presence and uses `generate_firecracker_config()` to produce runtime-specific configs with bootstrap injection.

**Minor remaining gap:**
- **Docker container log capture:** Log pumps capture stdout/stderr for process-spawned guests but Docker containers (launched via bollard in detached mode) do not have log streaming wired. Use bollard's `logs()` streaming API to pump container output to log files, matching the existing process-tier log capture pattern.

### Phase 11: Security Policy + WASM Tool Isolation

**Crates:** `sandbox`, `tools`, `runtime`

- `HostWasmToolRunner` with capability-scoped mount profiles per tool class
- File tools: `shared`+`tmp`+`vault` read-only for reads, `shared`+`tmp` read-write for writes
- Web tools: no FS mounts, host-function HTTP proxy only
- SSRF protection: hostname resolution before blocklist checks, rejecting loopback, link-local, RFC1918, metadata endpoints
- Vault copy: two sequential invocations (no single invocation gets both vault and shared/tmp)
- `SecurityPolicy`: command allowlist, path traversal blocking, HITL gates
- `RegexSet` output scrubbing for credentials (api_key, token, password, bearer patterns)
- Entropy-based secret detection (Shannon entropy >= 3.8)
**Phase 11 gaps — resolved:**
- ~~**WASM isolation simulated:**~~ `WasmWasiToolRunner` now executes file operations inside a `wasmtime` WASM sandbox with WASI preopened directories as the hard boundary. A `wasm32-wasip1` guest binary (`crates/wasm-guest/`) is compiled and embedded via `crates/sandbox/build.rs`. The `WasmToolRunner` trait, capability profiles, and mount policy definitions are unchanged — only the execution backend changed. `HostWasmToolRunner` is retained as a fallback when the `wasm-isolation` feature is disabled.

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
- **Gap:** `ratatui` + `crossterm` rendering loop not implemented

**Gaps / follow-ups to complete Phase 12:**
- **Interactive TUI client:** add a real TUI binary (ratatui + crossterm) so users can send/receive messages; clarify `runner --tui` as a probe or wire it to launch the interactive client.

## Forward Plan: Phases 13-21

### Phase 13: Model Catalog Governance + Provider Flexibility

**Crates:** `types`, `provider`, `runner`, `tui`
**Builds on:** Phases 1, 2, 12

**Scope:**
- Promote pinned model metadata to a complete typed schema (`ModelDescriptor` with deprecation state, pricing, capability flags) and enforce validation at startup
- Implement deterministic snapshot generation command invokable through TUI/runner and reproducible in CI
- Replace per-provider config blocks with a provider registry: named providers with `provider_type`, `base_url`, `api_key_env`, and optional headers
- Allow multiple providers per type; models reference `provider_id` instead of hard-coded provider keys
- Add Google Gemini provider with configurable base URLs
- Implement OpenAI Responses API provider (Gap 5) with `previous_response_id` chaining and SSE parsing

**Verification gate:** Catalog snapshot command produces deterministic typed artifact; config validation rejects unknown/invalid model metadata and provider references; model selection resolves to a named provider; Gemini and OpenAI Responses providers pass contract tests; snapshot regeneration is reproducible in CI.

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

### Phase 19: Scheduler System

**Crates:** `runtime`, `memory`, `types`, `tools`, `gateway`
**Builds on:** Phases 10, 15, 18

**Scope:**
- `SchedulerStore` with durable schedule definitions (one-off + periodic)
- `SchedulerExecutor` evaluating due entries and dispatching as runtime turns
- Schedule management tools: `schedule_list`, `schedule_create`, `schedule_delete`
- Silent execution support (always/conditional/never notify)
- Execution through same policy envelope as interactive turns

**Verification gate:** One-off and periodic entries execute bounded turns with normal policy/audit controls; conditional notification works correctly.

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
| Anthropic SSE streaming | Phase 7 | Before Phase 14 (external channels need reliable streaming) | High |
| TUI ratatui rendering + interactive client | Phase 12 | Before Phase 14 (TUI must be usable before adding more channels) | High |
| ~~WASM tool isolation (simulated → real)~~ | Phase 11 | ~~Before Phase 15~~ **Resolved** — `WasmWasiToolRunner` with wasmtime + WASI preopened directories; `wasm32-wasip1` guest binary embedded via `build.rs`; TOCTOU eliminated | ~~High~~ |
| OpenAI ResponsesProvider | Phase 2 | Phase 13 (provider registry + Responses provider implementation) | Medium |
| upsert_chunks implementation | Phase 9 | Before Phase 20 (skill/document indexing needs chunk ingestion) | Medium |
| ~~Runner control plane + persistent daemon + VM log capture~~ | Phase 10 | ~~Before Phase 12~~ **Resolved** — daemon flag, Unix control socket, log pump threads | ~~High~~ |
| ~~Container/microvm bootstrap wiring (args + bootstrap-stdin + envelope)~~ | Phase 10 | ~~Before Phase 12~~ **Resolved** — bootstrap file written pre-launch, bind-mounted into containers, injected into FC boot_args | ~~Critical~~ |
| ~~Linux microvm config input mismatch (Firecracker vs OCI tags)~~ | Phase 10 | ~~Before Phase 12~~ **Resolved** — dedicated firecracker config fields in RunnerGuestImages, generate_firecracker_config() | ~~High~~ |
| Docker container log capture | Phase 10 | Non-blocking; wire bollard `logs()` streaming to log files for container-tier guests | Low |

These gaps do not block any currently completed phase's functionality but should be resolved before the phases that depend on them.

## Why This Order Avoids Rewrites

- **Phase 1 types** are used unchanged through Phase 21
- **Phase 2 Provider trait** — Phase 7 added Anthropic without changing the trait; future providers follow the same pattern
- **Phase 4 Tool trait** with `safety_tier()` and `timeout()` — Phase 6 uses these for parallel execution, Phase 11 for WASM policy, Phase 17 for MCP
- **Phase 5 CancellationToken** — Phase 15 reuses for subagent lifecycle management
- **Phase 1 tracing** — Phase 16 upgrades subscriber to OpenTelemetry without changing instrumentation code
- **Phase 8 SQL migrations** — Phase 9 added vector/FTS tables via migration, not schema wipe
- **Phase 12 Channel trait** — Phase 14 extends with external adapters without changing the trait
- **Phase 14 identity mapping** — prevents multi-channel history fragmentation before Phase 15 concurrency
- **Phase 19 scheduler** — reuses runtime policy rails (budget, cancellation, tool gating)
- **Phases 20-21** — separate skills from persona policy to prevent LLM mutation of behavioral boundaries

## Test Strategy

- **Phases 1-4** — type/serialization tests, schema tests, provider contract tests
- **Phases 5-11** — runtime integration tests for cancellation, retries, tool safety, memory migration, runner isolation, WASM policy
- **Phases 12-21** — end-to-end tests for TUI/channel adapters, gateway routing, observability, MCP, catalog governance, session controls, scheduler, skills, persona policy
- **Regression policy** — cumulative; later phases add tests but never remove earlier checks
- **CI enforcement** — every phase verification gate is represented by automated tests; `cargo deny`, `clippy`, `rustfmt`, and `cargo test` must all pass
