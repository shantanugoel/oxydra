# Phase 12 Completion Plan (Security/Sandbox Gaps)

## Goal

Close the currently identified **missing/partial** security and sandboxing flows from Chapter 7, while explicitly deferring agreed out-of-scope items.

## Scope

### In scope

1. Runner bootstrap envelope transport to `oxydra-vm` over startup socket (length-prefixed JSON frame).
2. Runtime bootstrap consumption through real runner-to-VM transport path (not `None` placeholder in process launch path).
3. Runner usage of per-user config surfaces that are currently validated but not applied (`mounts`, `resources`, `credential_refs` wiring to launch/runtime policy envelope).
4. Strengthen lightweight tool isolation from modeled capability metadata to real execution isolation boundary (wasmtime-backed execution path and capability enforcement).
5. Harden web egress policy to support strict allowlist/proxy-governed mode and remove policy bypasses.
6. Add `RunnerControl` transport + handler wiring (health/shutdown protocol path).
7. Tighten startup/runtime status reporting so degraded states are explicit and auditable.

### Out of scope for now

- Browser session implementation details beyond current availability/status plumbing.
- HITL approval-gate integration in runtime tool execution flow.

---

## Implementation Workstreams

## 1) Bootstrap Envelope End-to-End Transport

### Current gap
Envelope encode/decode exists, but runner does not transmit it into spawned `oxydra-vm` startup flow.

### Plan
- Add startup socket/frame path in runner launch for runtime guest.
- Send `RunnerBootstrapEnvelope::to_length_prefixed_json()` over that channel after guest spawn.
- Update `oxydra-vm` startup to read frame from startup socket/stdin transport and pass bytes into `bootstrap_vm_runtime`.

### Acceptance criteria
- `oxydra-vm` process startup path no longer calls `bootstrap_vm_runtime(None, ...)` in normal runner-launched mode.
- Sidecar endpoint data is sourced from runner frame transport, not env-based fallback.
- Integration test proves shell availability toggles correctly from transmitted sidecar metadata.

---

## 2) Apply Per-User Config in Effective Runtime Launch

### Current gap
Per-user config validates but launch/runtime paths do not consume mounts/resources/credential refs.

### Plan
- Define effective per-user launch settings struct resolved from global + user config.
- Wire mount/resource settings into container/microVM/process launch request path.
- Define credential-ref resolution contract (without exposing secret values in model-visible surfaces) and pass only runtime-safe references.

### Acceptance criteria
- Runner launch behavior reflects non-default user config values.
- Resource/mount settings are observable in launch specs/tests.
- Credential refs are plumbed through typed runtime config path without raw secret leakage.

---

## 3) Lightweight Tool Isolation Hardening

### Current gap
Capability profiles are modeled/audited, but lightweight tools currently execute host-native logic directly.

### Plan
- Introduce wasmtime-backed lightweight tool execution path with per-tool capability profile enforcement.
- Keep existing tool API surface stable while moving execution backend behind `WasmToolRunner`.
- Preserve two-step vault copy separation guarantees with enforced disjoint mounts.

### Acceptance criteria
- Filesystem/network capabilities are enforced by runtime isolation boundary, not only policy checks.
- Existing tool contract tests pass with new backend.
- Vault two-step behavior remains auditable and unchanged semantically.

---

## 4) Web Egress Policy Tightening

### Current gap
IP/range blocking exists, but strict outbound governance (allowlist/proxy mode) is incomplete and private-base-url bypass remains possible.

### Plan
- Add explicit egress policy modes: default safe mode + strict allowlist/proxy mode.
- Enforce allowlist/proxy checks uniformly for both `web_fetch` and `web_search` requests.
- Remove/guard bypass paths so private destination allowance requires explicit operator-controlled policy mode.

### Acceptance criteria
- Strict mode blocks non-allowlisted destinations deterministically.
- No per-tool argument can silently bypass strict egress controls.
- Regression tests cover loopback/private/metadata/allowlisted destinations.

---

## 5) RunnerControl Protocol Wiring

### Current gap
`RunnerControl` type exists without transport/handler implementation.

### Plan
- Add control endpoint (unix/vsock/local channel as appropriate) for runner control operations.
- Implement typed request/response handling for `HealthCheck` and `ShutdownUser`.
- Wire into runner lifecycle with explicit error/status outcomes.

### Acceptance criteria
- Control operations are executable through implemented transport path.
- Shutdown/health paths are validated with integration tests.
- Errors are structured and auditable.

---

## 6) Startup/Status Observability Tightening

### Current gap
Degraded/disabled states are present but not fully unified across startup and health surfaces.

### Plan
- Standardize startup status payloads for shell/browser availability, sidecar validity, and sandbox tier posture.
- Ensure gateway/runner/VM paths expose consistent degraded reasons.
- Add targeted tests for degraded startup permutations.
- Most of the code across the workspace in any crates does not have much or any tracing at all. At decent (but don't go overboard) tracing at appropriate levels

### Acceptance criteria
- Startup logs and status surfaces consistently represent effective capability state.
- Degraded reasons are deterministic and test-covered.

---

## Suggested Execution Order

1. Bootstrap envelope transport (unblocks real runtime wiring).
2. Per-user config application (launch correctness baseline).
3. RunnerControl protocol path.
4. Egress policy tightening.
5. Lightweight tool isolation hardening.
6. Observability/status unification pass.

## Verification Gate (for these changes)

- `cargo fmt --all --check`
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- `cargo test -p types -p sandbox -p tools -p shell-daemon -p runner -p runtime -- --test-threads=1`
