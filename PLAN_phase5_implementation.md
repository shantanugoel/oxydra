# Phase 5 Implementation Plan — Agent Loop, Cancellation, and Runtime Testing

## 1) Current Project State (post-review)

After reviewing `oxydra-project-brief.md` and the workspace code:

- **Phases 1-4 are largely implemented** in `types`, `provider`, `tools`, and `tools-macros`.
- `provider` already includes:
  - OpenAI request/response normalization,
  - streaming SSE parsing and `StreamItem` emission,
  - model catalog validation and API key precedence handling.
- `tools` includes Phase 4 core tools (`read`, `write`, `edit`, `bash`) and registry-level timeout/output-size policy hooks.
- `runtime` is still a **stub crate** (`add(left, right)`), so the Phase 5 runtime loop is the next major build surface.
- Baseline verification currently passes locally:
  - `cargo fmt --all --check`
  - `cargo clippy --workspace --all-targets --all-features -- -D warnings`
  - `cargo test -p types -p provider -p tools -p tools-macros -- --skip live_openai_smoke_normalizes_response`

---

## 2) Phase 5 Scope

Implement the first production runtime loop in `crates/runtime`:

1. **Minimal deterministic turn cycle**
   - send context to provider
   - stream/collect assistant output
   - detect tool calls
   - execute tools
   - append tool results and continue until completion
2. **Runtime guards**
   - cancellation (`CancellationToken`)
   - per-turn timeout
   - max-turn budget
   - max-cost budget
3. **Deterministic testing**
   - `MockProvider` + `MockTool` coverage for runtime behavior and guardrails.

Out of scope for this phase:
- schema self-correction loop and recovery retries (Phase 6),
- parallel safe-tool execution (Phase 6),
- second provider/config switching (Phase 7),
- memory persistence (Phase 8+).

---

## 3) Suggested Crates — Evaluation and Decision

| Crate | Suggested In Brief | Phase 5 Decision | Why |
|---|---|---|---|
| `tokio-util` (`CancellationToken`) | Phase 5 | **Adopt now** | Required for cooperative cancellation and future subagent lifecycle reuse (Phase 13). |
| `mockall` | Phase 5+ | **Adopt now (dev-dependency)** | Enables deterministic trait-level runtime tests for `Provider` and tool execution paths. |
| `insta` | Phase 5+ | **Adopt now for provider snapshots (carry-over hardening)** | Prevents request/response serialization drift while runtime is being added. |
| `futures` (`join_all`) | Mentioned for Phase 6 behavior | **Defer** | Needed for parallel tool execution in Phase 6, not required for Phase 5 sequential loop. |
| `genai` / `rig-core` | Optional ecosystem strategy | **Defer** | Keep Phase 5 focused on internal runtime contract; evaluate adapter-only integrations in later phases. |

---

## 4) Carry-Over Items from Earlier Phases (to close now or track explicitly)

These were identified during review and should be handled as part of Phase 5 readiness/hardening:

1. **Provider default endpoint policy mismatch**
   - `OpenAIProvider` currently defaults to `https://openrouter.ai/api` while naming and tests are OpenAI-centric.
   - Action: decide and document canonical default policy (OpenAI-native vs OpenRouter default with explicit naming/config).

2. **Live provider smoke test behavior**
   - `live_openai_smoke_normalizes_response` requires `OPENAI_API_KEY`; local/CI runs should not fail by default.
   - Action: gate behind explicit opt-in (`#[ignore]`, feature flag, or dedicated CI job).

3. **Cost guard prerequisite gap**
   - Phase 5 requires max-cost budgeting, but current runtime/types surfaces do not yet expose clear per-model pricing metadata.
   - Action: define interim cost accounting strategy (token budget fallback vs typed pricing metadata addition) before finalizing runtime guard API.

4. **CI gate completeness**
   - Workflow currently runs rustfmt + cargo-deny only.
   - Action: add clippy + phase-relevant tests once runtime lands to prevent regressions.

---

## 5) Implementation Plan (ordered, dependency-aware)

### Step A — Runtime crate foundation
- Add runtime dependencies:
  - `types`, `tools`
  - `tokio` (runtime/time/select)
  - `tokio-util` (cancellation token)
  - `tracing`
  - `thiserror` only if runtime-local error wrappers are needed (prefer existing `types::RuntimeError` first).
- Define core runtime structures:
  - `TurnState` enum (`Streaming`, `ToolExecution`, `Yielding`, `Cancelled`, etc.).
  - `RuntimeGuards` / `RuntimeLimits` (`turn_timeout`, `max_turns`, `max_cost`).
  - `AgentRuntime` struct (provider handle, tool registry, guard config).

### Step B — Minimal turn-cycle execution path
- Implement a single public run entry point (e.g., `run_turn_loop` / `run_session`) that:
  1. validates guard preconditions,
  2. calls provider (`stream` preferred, with fallback to `complete`),
  3. accumulates assistant text and tool-call deltas,
  4. reconstructs tool calls deterministically,
  5. executes tools (sequentially in Phase 5),
  6. appends assistant and tool-result messages to context,
  7. repeats until no tool calls remain.

### Step C — Guardrail enforcement
- **Cancellation:** check token between major loop stages and during awaited operations (`tokio::select!` where appropriate).
- **Per-turn timeout:** wrap provider/tool stages in bounded timeout.
- **Max-turn budget:** enforce strict upper bound on recursive/tool-driven loops.
- **Max-cost budget:** implement selected accounting strategy (see Carry-Over item #3) and enforce fail-fast.

### Step D — Test architecture
- Add `mockall`-driven tests in `crates/runtime` for:
  - normal single-turn response without tools,
  - tool-call loop completion path,
  - cancellation before/during provider turn,
  - timeout behavior,
  - max-turn exceeded,
  - max-cost exceeded.
- Add regression tests for tool-call delta assembly assumptions used by runtime integration.

### Step E — Carry-over hardening tasks (same PR or immediate follow-up)
- Stabilize live smoke test gating strategy.
- Resolve/document provider default endpoint policy.
- (Optional but recommended) add `insta` snapshots in `provider` for stable request normalization fixtures.

---

## 6) Verification Gates for Phase 5

Run and require green before phase closure:

1. `cargo fmt --all --check`
2. `cargo clippy --workspace --all-targets --all-features -- -D warnings`
3. `cargo test -p types -p provider -p tools -p tools-macros -p runtime -- --skip live_openai_smoke_normalizes_response`

If snapshot tests are added:

4. `cargo test -p provider snapshot`

---

## 7) Phase 5 Exit Criteria

Phase 5 is complete when all are true:

- `runtime` has a functional deterministic agent loop over `Provider` + tool registry.
- Cancellation, timeout, max-turn, and cost guards are implemented and tested.
- `MockProvider`-based tests validate loop behavior without network dependencies.
- Carry-over gaps are either fixed in-phase or explicitly tracked with clear follow-up ownership and rationale.
- Verification gates are automated and passing.
