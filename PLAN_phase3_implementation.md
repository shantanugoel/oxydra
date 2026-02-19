# Phase 3 Implementation Plan: Streaming Support + SSE Parsing

## Scope Reviewed from `oxydra-project-brief.md`

This plan is aligned to:

- **Chapter 1** (layering constraints): keep channel/UI concerns out of core streaming implementation.
- **Chapter 2** (provider abstraction): preserve typed model validation and provider trait contract; add streaming in a provider-agnostic shape.
- **Chapter 3** (runtime contract): stream items must support deterministic turn-loop consumption and tool-call detection.
- **Chapter 4** (async streaming mechanics): use bounded `tokio::sync::mpsc`, dedicated parsing task, robust chunk accumulation.
- **Appendix / Progressive Build Plan** (Phase 3 row + alignment): deliver real-time OpenAI token streaming with SSE parsing, backpressure, and connection-loss signaling.

## Phase 3 Goal

Implement OpenAI streaming in the `provider` layer so tokens can be emitted in real-time via structured stream events, while preserving existing non-streaming behavior from Phase 2.

## In Scope

1. Provider-facing streaming API surface (trait + types as needed).
2. OpenAI Chat Completions SSE request/response streaming path.
3. Structured stream event emission:
   - `Text`
   - `ToolCallDelta`
   - `ReasoningDelta`
   - `UsageUpdate`
   - `ConnectionLost`
   - `FinishReason`
4. Bounded-channel backpressure handling.
5. Parser/accumulator handling for fragmented tool-call arguments.
6. Test coverage for SSE parsing and stream event normalization.

## Out of Scope (Phase 3)

- Runtime turn-loop integration logic (full agent loop is Phase 5).
- Tool execution/self-correction behavior (Phases 4 and 6).
- Anthropic/Gemini streaming adapters (later phases).
- Full gateway/channel integration.

## Crate Evaluation Through Phase 3 (Decision)

| Crate | Phase in brief | Decision for now | Rationale |
|---|---|---|---|
| `reqwest` | Phase 2+ | **Keep (already in use)** | Matches default core-path strategy and supports streaming transport. |
| `eventsource-stream` | Phase 3 | **Adopt for Phase 3 SSE parsing** | Gives explicit SSE parsing while keeping runtime/provider behavior under our control. |
| `reqwest-eventsource` | Phase 3 (consider) | **Do not adopt initially** | Useful convenience wrapper, but adds abstraction where we need explicit loss/backpressure semantics first. |
| `genai` | Phase 2 optional | **Do not adopt for Phase 3 core path** | Brief positions it as optional adapter only; current direct-provider path already aligns with architecture constraints. |
| `rig-core` | Phase 3+ evaluation | **Defer** | Brief marks it selective/experimental and warns to keep custom runtime as system of record. |

Implementation note: if SSE edge cases become costly with `eventsource-stream`, we can reevaluate `reqwest-eventsource` behind a provider-internal adapter without changing external traits.

## Proposed Implementation Workstreams

### Workstream 1 — Contract Extensions (types + provider boundary)

1. Add a shared `StreamItem` enum (in `types`) with the Phase 3 required variants.
2. Extend `Provider` trait with a streaming method suitable for dynamic dispatch and bounded-channel usage.
3. Keep `complete` intact for backwards compatibility; avoid regressions in existing tests.

**Notes**
- Although the phase table lists `provider` as primary touched crate, trait/type ownership currently lives in `types`, so a minimal `types` change is expected.

### Workstream 2 — OpenAI SSE Transport in `provider`

1. Add a streaming request path (`stream: true`) for Chat Completions.
2. Parse SSE frames (`data:` payloads + `[DONE]`) into typed internal delta structures.
3. Normalize deltas into `StreamItem` values and send through bounded `mpsc`.
4. Emit finish reason and usage updates when present in stream payload.

### Workstream 3 — Delta Accumulation + Fragment Safety

1. Implement accumulator state for tool-call argument fragments keyed by tool-call index/id.
2. Handle fragmented JSON argument chunks safely before downstream validation.
3. Preserve partial state across chunk boundaries and flush cleanly on finish.

### Workstream 4 — Connection Resilience Semantics

1. Detect abrupt stream termination and emit `ConnectionLost`.
2. For non-resumable streams (OpenAI chat completions), surface structured loss events instead of silent failure.
3. Ensure stream task shutdown is explicit and non-leaky.

### Workstream 5 — Verification and Gates

1. Add provider tests for:
   - SSE text delta normalization.
   - Tool-call delta fragmentation and reassembly behavior.
   - Finish reason/usage propagation.
   - Connection-loss signaling.
2. Keep/update existing non-streaming tests to ensure no regression.
3. Verification commands:
   - `cargo test -p provider`
   - `cargo clippy -p provider --all-targets -- -D warnings`
4. Add/confirm a minimal real-time stdout demonstration path for manual gate validation.

## File-Level Change Plan (Expected)

- `crates/types/src/model.rs`
  - Add streaming item types and any supporting delta structs.
- `crates/types/src/provider.rs`
  - Extend provider trait with stream method.
- `crates/types/src/lib.rs`
  - Re-export new stream types.
- `crates/provider/src/lib.rs`
  - Add OpenAI SSE request/parse/normalize pipeline + bounded channel task.
  - Add parser and accumulator tests.
- `crates/provider/Cargo.toml`
  - Add any required SSE parsing dependency (`eventsource-stream` or equivalent) and reqwest streaming support flags.

## Risk Points and Mitigations

1. **Trait evolution risk** (breaking downstream crates):
   - Mitigation: additive API only, keep existing `complete`.
2. **Parser edge cases** (fragmented JSON/tool arguments):
   - Mitigation: focused unit tests for fragmented/escaped content.
3. **Backpressure deadlocks**:
   - Mitigation: bounded channel + explicit send/close/error handling.
4. **Connection drop ambiguity**:
   - Mitigation: explicit `ConnectionLost` emission and deterministic termination behavior.

## Definition of Done (Phase 3 Exit)

Phase 3 is complete when:

1. OpenAI chat completions can stream token deltas in real time through provider streaming APIs.
2. SSE payloads are normalized into required `StreamItem` variants with bounded-channel transport.
3. Connection-loss behavior is explicit and tested.
4. Existing Phase 2 non-streaming behavior remains green.
5. Provider tests and clippy pass with no warnings/errors.
