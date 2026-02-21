# Phase 12 Implementation Plan (Channel Trait + TUI + Baseline Gateway)

## 1) Scope, objective, and guardrails

### Phase 12 objective (from project brief)
Deliver a baseline channel stack where:
- `Channel` trait exists in `types`.
- `tui` is a first-class channel adapter.
- `gateway` routes TUI traffic end-to-end to a running `oxydra-vm`.
- Output is streamed to TUI.
- `Ctrl+C` cancels only the active turn (does not kill the guest/runtime).
- `oxydra-runner --tui` is connect-only; if no running guest exists, exit clearly.

### Hard constraints
- Keep strict crate boundaries and minimal-core philosophy.
- Do not introduce Phase 14+ scope (external channels, sender allowlists, advanced identity mapping, lane hardening, subagents, OTel, scheduler, MCP).
- Treat `cli -> tui` as hard cutover (no compatibility alias crate).

---

## 2) Current repository baseline (relevant to Phase 12)

- `crates/channels` and `crates/gateway` are still template placeholders.
- `crates/types` has provider/tool/memory/runner traits and protocol types, but no `Channel` abstraction yet.
- `crates/runner` supports spawning user guests and bootstrap envelope wiring, but has no TUI connect-only path.
- `crates/cli` currently owns VM bootstrap assembly (`bootstrap_vm_runtime*`) from Phase 10.
- `crates/runtime` already has cancellation (`CancellationToken`), session-scoped execution (`run_session_for_session`), and memory persistence hooks needed for reconnect behavior.
- Existing protocol precedent: strongly typed serde enums + framed transport (length-delimited JSON in shell-daemon; length-prefixed bootstrap envelope in types/runner).

---

## 3) Phase 12 design decisions (explicit answers)

## 3.1 Transport: how TUI reaches gateway
**Decision:** TUI uses **WebSocket** to connect to gateway (`axum` server side, `tokio-tungstenite` client side) over loopback endpoint exposed by the running `oxydra-vm`.

**Why:**
- Matches Phase 12 crate guidance (`axum` + `tokio-tungstenite`).
- Supports full-duplex behavior needed for prompt send + streamed output + cancel control.
- Keeps baseline path aligned with future channel multiplexing design.
- Avoids introducing a second bespoke transport for TUI in this phase.

**Minimal-core application:**
- Gateway-to-runtime stays **in-process** within `oxydra-vm` in Phase 12 (no new internal RPC layer yet).

## 3.2 Framing protocol required before transport work
**Decision:** Define transport-agnostic gateway frame types in `types` first, then bind them to WebSocket.

Create typed envelopes (serde-tagged enums), e.g.:
- `GatewayClientFrame`: `hello`, `send_turn`, `cancel_active_turn`, `health_check`.
- `GatewayServerFrame`: `hello_ack`, `turn_started`, `assistant_delta`, `turn_completed`, `turn_cancelled`, `error`, `health_status`.

Protocol requirements before transport coding:
- Stable `request_id` / `turn_id` correlation.
- Explicit `runtime_session_id` in relevant frames.
- Versionable envelope shape (protocol version field in handshake).
- Round-trip serde contract tests in `types/tests`.

This mirrors existing repository protocol style and prevents transport-first schema drift.

## 3.3 Ctrl+C behavior and signal handling requirements
**Behavior contract:**
- If a turn is active: `Ctrl+C` => cancel active turn only.
- If no turn is active: `Ctrl+C` => exit TUI gracefully.
- Never treat `Ctrl+C` as guest/runtime shutdown in TUI mode.

**Signal handling requirements:**
- TUI event loop must consume `Ctrl+C` from terminal input path (raw mode key event handling).
- Also register `tokio::signal::ctrl_c()` fallback to avoid platform/terminal-mode edge cases.
- Translate either signal source into a single internal `UiIntent::CancelActiveTurn` event.
- Gateway maps cancel request to per-turn `CancellationToken`, not process termination.

## 3.4 Reconnect semantics when TUI dies mid-turn
**Phase 12 baseline semantics:**
- WebSocket disconnect **does not auto-cancel** running turn.
- Gateway keeps per-user active turn state until completion/cancel.
- On reconnect, TUI resumes the same runtime session (`runtime_session_id`) and receives:
  - active-turn status if still running, then new deltas from that point onward;
  - completed outcome if turn finished while disconnected.
- No token-by-token historical replay guarantee in Phase 12; durable continuity comes from persisted session history in runtime/memory.

This provides deterministic behavior while avoiding Phase 14-level identity/session mapping complexity.

---

## 4) Work breakdown by crate

## 4.1 `types` crate (core contracts first)
1. Add `channel` module with:
   - `Channel` trait (`send`, `listen`, `health_check`) using async trait object-safe signatures.
   - Core channel event structs/enums used by gateway and adapters.
2. Add gateway frame protocol types:
   - client/server frame enums.
   - handshake/session/turn status payload structs.
3. Export new types from `types/src/lib.rs`.
4. Add protocol contract tests:
   - serde round-trip for each frame variant.
   - invalid/missing field decoding failures where appropriate.

## 4.2 `channels` crate (baseline adapter plumbing)
1. Replace template code with channel-facing primitives:
   - lightweight registry/trait-object helpers for channel adapters.
   - small shared utilities for adapter lifecycle/health checks.
2. Keep it minimal: only what gateway+tui need in Phase 12.
3. Add tests for registry behavior and health-check plumbing.

## 4.3 `gateway` crate (baseline daemon + turn routing)
1. Implement WebSocket server endpoint (axum).
2. Decode/encode typed gateway frames from `types`.
3. Build minimal session manager:
   - stable `runtime_session_id` per user for TUI path (Phase 12 only).
   - track active turn task + `CancellationToken`.
4. Route `send_turn` to runtime (`run_session_for_session`) and emit:
   - `turn_started`
   - streamed output frames (`assistant_delta` path)
   - `turn_completed` / `turn_cancelled` / `error`
5. Implement reconnect attach logic (same user/session resumes in-flight state).
6. Add gateway tests:
   - handshake success/failure
   - turn execution end-to-end with mocked runtime/provider path
   - cancel flow (`cancel_active_turn` affects active turn only)
   - disconnect/reconnect mid-turn semantics

> Note: If runtime streaming cannot be surfaced cleanly through existing APIs, add only a **narrow runtime observer seam** required for Phase 12 output streaming; avoid broader runtime redesign.

## 4.4 `tui` crate cutover (`cli -> tui` hard rename)
1. Perform hard cutover from `cli` crate to `tui` crate (workspace + package updates, no compatibility alias crate).
2. Preserve VM bootstrap assembly behavior currently in `cli` by moving it into behavior-named module(s) inside `tui` crate.
3. Implement TUI channel adapter:
   - WebSocket client to gateway.
   - UI state machine for prompt entry, streaming output rendering, error states.
4. Implement Ctrl+C contract exactly (active-turn cancel vs idle exit).
5. Add TUI tests:
   - frame encode/decode + state transitions
   - signal-to-cancel behavior
   - reconnect flow behavior

## 4.5 `runner` crate (`--tui` connect-only behavior)
1. Add connect-only path/API for TUI mode:
   - validates target user/workspace config.
   - resolves gateway endpoint for already-running guest.
   - probes health before returning success.
2. If no running guest/gateway endpoint is reachable, return explicit error for clear CLI exit messaging.
3. Ensure this path does **not** call guest spawn/start routines.
4. Add runner tests:
   - connect-only success against mocked running endpoint
   - connect-only failure with clear error when guest absent
   - prove no spawn side effects in `--tui` path

---

## 5) Verification gate mapping (Phase 12)

Map each required gate to concrete checks:

1. **`Channel` trait compiles in `types`**
   - `types` unit/integration tests for trait + protocol structs.

2. **`tui` implements channel adapter**
   - compile-time trait implementation tests and client state machine tests.

3. **Gateway routes TUI traffic end-to-end to running `oxydra-vm`**
   - integration test: websocket client sends turn, receives streamed output + completion.

4. **Output streams correctly**
   - integration test asserts ordered incremental output frames before completion frame.

5. **Ctrl+C cancels active turn only**
   - integration test triggers cancel event mid-turn, asserts runtime returns `Cancelled`, gateway process remains alive, subsequent turns still work.

6. **No running guest => runner exits clearly in `--tui`**
   - runner test for explicit no-guest error path.

Recommended validation command set after implementation:
- `cargo fmt --all --check`
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- `cargo test -p types -p channels -p gateway -p tui -p runner -- --test-threads=1`

---

## 6) Explicit non-goals for this phase

- No external channel adapters (Telegram/Slack/Discord/etc.).
- No sender allowlist policy enforcement (Phase 14).
- No advanced multi-channel identity mapping model from Phase 14.
- No lane-based concurrency hardening beyond minimal per-session safety needed for Phase 12.
- No subagent orchestration/routing graphs (Phase 15+).
- No OpenTelemetry rollout (Phase 16).
- No MCP, scheduler, skills, or persona governance work.

---

## 7) Ordered execution sequence (implementation order)

1. Contracts first (`types` channel + gateway frames + tests).
2. `channels` minimal plumbing.
3. `gateway` websocket server + routing + cancellation + reconnect baseline.
4. `cli -> tui` hard cutover + TUI client/channel implementation + signal handling.
5. `runner` connect-only TUI mode.
6. End-to-end tests across `types/channels/gateway/tui/runner`.
7. Final gate: fmt, clippy (warnings denied), targeted test suite.
