# Phase 10 Implementation Plan

## Objective

Implement **Phase 10** from `oxydra-project-brief.md`: runner-isolated sandboxing, security policy enforcement, and credential scrubbing, with verification that:

1. runtime starts under an isolation contract,
2. privileged shell/browser pathways are disabled when sidecar is unavailable (default behavior),
3. secret-like values are scrubbed from tool output before they re-enter context/memory.

---

## Deep Current-State Audit (Brief vs Code)

### What Phase 10 requires (brief)

- Runner-managed isolation with runtime guest + sidecar guest model.
- Security policy layer with default-deny behavior and explicit capability sets.
- `wasmtime` path for lightweight tools, sidecar routing for shell/browser, optional Linux hardening.
- Credential scrubbing (`api_key`, `token`, `password`, `bearer`) in runtime output path.
- Verification gate tied to `sandbox`, `runtime`, `runner`.

### What exists now (source code)

- `crates/sandbox` is still a placeholder (`add` function only), with no sandbox trait/backends.
- There is **no `crates/runner` crate** in workspace members.
- `runtime` currently executes tools directly via `ToolRegistry::execute` (no sandbox/runner hop and no security policy object).
- `tools` has phase-4 core tools only (`read_file`, `write_file`, `edit_file`, `bash`) and uses `SafetyTier`; `bash` is `Privileged`.
- `types::config::AgentConfig` has runtime/memory/provider/reliability config, but no Phase-10 security/sandbox/runner config block.
- Runtime currently persists and injects tool output into conversation/memory without secret scrubbing.

### Resulting gap

Phase 10 is not yet started architecturally; we need new crate/module surfaces and runtime wiring, not incremental tweaks.

---

## Scope for Phase 10 in This Repository

### In scope

- Add core contracts and minimal implementation for runner control + sandbox policy path.
- Enforce tool execution policy by `SafetyTier` with sidecar availability checks.
- Add credential scrubbing on tool results and tool errors before context persistence.
- Add tests proving the Phase-10 verification gate.

### Out of scope for this phase cut

- Full browser automation implementation (no browser tool exists yet).
- Full production container/microVM orchestration; provide trait-backed runner control with a concrete local implementation and clear extension points.
- Full web egress enforcement implementation (no web tools currently registered); implement policy scaffolding and tests for deny-by-default behavior.

### Maintainer decision captured

- Default when sidecar channel is unavailable: **continue startup and disable privileged tools (e.g., `bash`)**.
- First implementation cut should prioritize **direct container orchestration integration** in runner.
- Development mode must support running Oxydra **without runner**, with privileged shell/browser paths disabled.
- Credential scrubbing default should include **keyword patterns + high-entropy token heuristics**.

---

## Implementation Workstreams

### WS1 — Security contracts and config surface (`types`)

**Files/crates**
- `crates/types/src/config.rs`
- `crates/types/src/lib.rs`
- new `crates/types/src/security.rs` (or equivalent)
- `crates/types/tests/*` (new/updated)

**Plan**
1. Add typed security config block to `AgentConfig` (runtime isolation mode, sidecar policy, credential scrubbing enable/pattern set, runner endpoint settings).
2. Introduce shared security types for:
   - workspace layout (`shared`, `tmp`, `vault`),
   - tool capability classification / routing decision,
   - sidecar availability mode and fail-loud behavior.
3. Keep defaults safe and explicit (no silent downgrade).
4. Add validation errors for invalid combinations.

**Exit criteria**
- Config deserializes/validates with deterministic defaults.
- Invalid security combinations fail startup validation.

---

### WS2 — Add `runner` crate and control-plane contract

**Files/crates**
- new `crates/runner/` (Cargo + src)
- root `Cargo.toml` workspace members
- runner tests

**Plan**
1. Create `runner` crate with `RunnerControl` client-facing API:
   - initialize runtime isolation,
   - request sidecar for user/session,
   - report sidecar channel health/availability.
2. Implement container orchestration path first (runner-managed isolation), while keeping transport abstraction boundaries explicit.
3. Implement explicit fail-loud startup states (required controls missing => hard error).
4. Keep runner host-only contract: no direct user command execution in runner process.
5. Provide a runtime-supported dev bypass mode (no runner): startup remains available with privileged tools disabled.

**Exit criteria**
- Runtime can call runner control API and receive deterministic availability state.
- Runner crate has unit/integration tests for handshake and unavailable sidecar signaling.

---

### WS3 — Implement sandbox crate contracts and policy mapping

**Files/crates**
- `crates/sandbox/src/lib.rs` (replace placeholder)
- `crates/sandbox/Cargo.toml`
- sandbox tests

**Plan**
1. Define `Sandbox` trait and execution request/response types.
2. Add policy mapping from `SafetyTier` to mount/access profile:
   - read-only tools => read mounts,
   - side-effecting tools => constrained read/write mounts,
   - privileged tools => sidecar route requirement.
3. Add workspace path canonicalization helpers (`shared`/`tmp`/`vault`) and traversal rejection.
4. Add wasmtime-backed execution adapter interface (minimal executable slice for this phase), with optional Linux hardening feature hooks.

**Exit criteria**
- Policy mapping and path guards are unit tested.
- Sandbox crate is no longer placeholder code.

---

### WS4 — Runtime integration: policy gate, sidecar disablement, scrubbing

**Files/crates**
- `crates/runtime/src/lib.rs`
- `crates/runtime/src/tool_execution.rs`
- runtime module split as needed
- `crates/runtime/src/tests.rs`

**Plan**
1. Add runtime security manager that composes:
   - runner state,
   - sandbox/policy evaluator,
   - credential scrubber.
2. Replace direct tool execution path with policy-aware execution (`execute_with_policy` + runtime-level checks).
3. Enforce sidecar dependency for privileged tools:
   - if sidecar unavailable: remove/disable privileged tool path and return explicit tool error.
4. Scrub both successful and failed tool output before:
   - appending tool messages to context,
   - persisting to memory.
5. Emit tracing events for policy decisions (allow/deny/disabled), without leaking secret values.

**Exit criteria**
- Privileged tool execution is blocked when sidecar is unavailable.
- Scrubbed output never contains raw secret matches.

---

### WS5 — CLI wiring and operator-facing config/docs

**Files/crates**
- `crates/cli/src/lib.rs`
- `examples/config/agent.toml`
- `README.md` (security/runtime section)

**Plan**
1. Map new security/runner config into runtime construction.
2. Wire runtime startup to runner initialization; enforce fail-loud mode when configured.
3. Document operator knobs and defaults for:
   - isolation mode,
   - sidecar availability behavior,
   - scrubbing controls.

**Exit criteria**
- CLI can build runtime with security config end-to-end.
- Example config includes phase-10 security block.

---

## Verification Plan (Phase Gate)

## Automated tests to add/update

1. **Runtime isolation startup**
   - startup fails when isolation is required and runner handshake fails.
2. **Sidecar-dependent privileged tool behavior**
   - `bash` denied/disabled when sidecar unavailable,
   - `bash` allowed when sidecar available and policy permits.
3. **Credential scrubbing**
   - tool output containing patterns like `api_key=...`, `Bearer ...`, `password=...`, `token=...` is redacted.
   - redaction applies to both success and error tool paths.
4. **Sandbox policy mapping**
   - `SafetyTier` maps to expected capability profile.
5. **Config validation**
   - invalid security config combinations fail early.

## Suggested gate command

```bash
cargo fmt --all --check && \
cargo clippy --workspace --all-targets --all-features -- -D warnings && \
cargo test -p types -p sandbox -p runner -p runtime -p cli
```

---

## Dependency-Ordered Execution Sequence

1. WS1 (`types`) — define contracts/config first.
2. WS2 (`runner`) — introduce runner control API and crate.
3. WS3 (`sandbox`) — implement policy and sandbox interfaces.
4. WS4 (`runtime`) — wire policy + sidecar gating + scrubber.
5. WS5 (`cli` + docs) — expose config and startup behavior.
6. Run full verification gate and fix regressions.

---

## Risks and Mitigations

1. **Risk:** adding runner/sandbox abstractions could overreach into a rewrite.
   - **Mitigation:** keep runtime loop unchanged; inject policy checks only at tool execution boundary.
2. **Risk:** false-positive credential redaction can destroy useful output.
   - **Mitigation:** use targeted regex set + tested replacement strategy + explicit redaction markers.
3. **Risk:** platform-specific transport/hardening divergence.
   - **Mitigation:** define transport/hardening traits now; implement Unix baseline + feature-gated extensions.

---

## Open Decisions for Maintainer Confirmation

- None pending; phase-10 planning defaults above reflect maintainer decisions captured during planning.
