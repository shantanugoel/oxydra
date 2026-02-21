# Phase 10 Implementation Plan: Runner and Isolation Infrastructure

## Goal
Implement Phase 10 from `oxydra-project-brief.md` so Oxydra can be launched through a runner-managed isolation flow, bootstrap runtime via runner metadata, execute shell commands through a sidecar daemon, and support explicit degraded `--insecure` process mode.

## Source Review Summary (What exists today)
- Workspace currently includes: `types`, `provider`, `tools`, `tools-macros`, `runtime`, `memory`, `sandbox`, `channels`, `gateway`, `cli`; there is **no `runner` crate** and **no `shell-daemon` crate**.
- `crates/sandbox/src/lib.rs` is still scaffold code (no traits/backends for shell/browser sessions).
- `crates/cli/src/lib.rs` currently owns startup/bootstrap concerns (config loading, provider construction, memory backend wiring, runtime limits derivation).
- `crates/tools/src/lib.rs` runs `bash` directly on host via `sh -lc`/`cmd /C`; no sidecar transport abstraction exists yet.
- `crates/runtime` has mature turn loop/budget/memory logic, but no runner bootstrap envelope handling and no shell-daemon session integration path.

## Phase 10 Scope
### In scope
- Add `SandboxTier` + runner/bootstrap protocol types.
- Add `runner` crate and `shell-daemon` crate.
- Add shell session protocol and sandbox backends (`VsockShellSession`, `LocalProcessShellSession`).
- Cut over startup ownership from legacy standalone CLI bootstrap path to runner-era bootstrap surfaces.
- Add explicit degraded behavior for missing sidecar / `--insecure`.

### Out of scope (Phase 11+)
- Full Wasmtime capability/mount enforcement.
- Egress policy implementation for web tools.
- Channel onboarding, gateway routing, MCP, persona/scheduler features.

## Cross-Platform Isolation + Transport Options
### Transport mapping baseline
- **MicroVm tier:** use **vsock where available** (notably Firecracker on Linux), with host-side Unix socket bridging where needed.
- **Container tier:** use **Unix domain sockets** for runner<->guest/sidecar channels.
- **Process tier (`--insecure`):** local process IPC (Unix sockets/pipes), with shell/browser disabled.

### Backend option A (Recommended): Hybrid by OS
- **Linux MicroVm:** Firecracker (`/dev/kvm`) + vsock transport.
- **macOS MicroVm:** Docker Sandboxes microVM path (Docker Desktop 4.58+; virtualization.framework).
- **Container tier (both):** Docker/container backend using Unix sockets.
- Notes: keeps true Linux microVM posture while using a practical microVM-capable path on macOS.

### Backend option B: Docker Sandbox-first
- Use Docker Sandboxes as primary backend on macOS; on Linux rely on Docker's documented legacy container-based sandbox behavior.
- Pros: lowest implementation complexity.
- Cons: Linux does not get Firecracker-style microVM guarantees.

### Backend option C: Linux-first microVM only
- Linux uses Firecracker microVMs; macOS runs Container/Process tier until a mac-native microVM backend is implemented.
- Pros: strongest Linux isolation posture quickly.
- Cons: capability asymmetry across platforms.

### Backend option D: Fully custom VM backends
- Linux Firecracker + macOS custom Virtualization.framework backend (no Docker sandbox dependency).
- Pros: maximum control over lifecycle/transport.
- Cons: largest build/test burden and longest delivery path.

## Workstreams and Ordered Execution

### WS1: Types/Foundation Contracts
1. Extend `crates/types` with:
   - `SandboxTier` enum: `MicroVm | Container | Process`.
   - Runner global/per-user config structs (typed, validated).
   - Length-prefixed bootstrap envelope payload types (runner -> oxydra-vm).
   - Shell daemon RPC message types: `SpawnSession`, `ExecCommand`, `StreamOutput`, `KillSession`.
2. Export these contracts through `types::lib`.
3. Add serde + validation tests for config and protocol payload compatibility.

### WS2: Runner Crate Introduction
1. Add `crates/runner` to workspace and implement:
   - Runner config loading (global + per-user files).
   - First-run workspace provisioning at `<workspace_root>/<user_id>/{shared,tmp,vault}`.
   - Tier resolution logic (`--insecure` forces `SandboxTier::Process`).
2. Implement per-tier startup behavior:
   - `MicroVm` (Linux): launch Firecracker-based guests (`oxydra-vm` + `shell-vm`) and wire vsock bootstrap/control channels.
   - `MicroVm` (macOS): use Docker Sandboxes VM path and use supported socket channels (do not assume general host vsock exposure).
   - `Container`: launch containerized guests and use Unix socket bootstrap/control channels.
   - `Process` (`--insecure`): start host process only, skip shell-vm, mark shell/browser unavailable.
3. Emit startup logs with selected tier and clear degraded-mode warning in process tier.

### WS3: Shell Daemon Crate Introduction
1. Add `crates/shell-daemon` to workspace.
2. Implement async framed RPC server for:
   - `SpawnSession`
   - `ExecCommand`
   - `StreamOutput`
   - `KillSession`
3. Provide minimal session manager with deterministic IDs and bounded output streaming.
4. Add integration test proving command execution (`echo`/`printf`) returns stdout over protocol.

### WS4: Sandbox Abstraction Implementation
1. Replace scaffold in `crates/sandbox` with:
   - `ShellSession` trait and `BrowserSession` trait.
   - `VsockShellSession` client backend (guest channel).
   - `LocalProcessShellSession` backend (dev/test harness).
2. Add explicit connection/status modeling so sidecar absence is surfaced, not silently ignored.
3. Implement process-tier hardening attempt hooks:
   - Linux: best-effort Landlock attempt.
   - macOS: best-effort Seatbelt attempt/log hook.
   - Both produce auditable log events (success/failure/unsupported).

### WS5: Runtime + Tools Wiring
1. Add runner-era bootstrap ingestion in oxydra-vm startup path (length-prefixed JSON envelope).
2. Wire runtime/tool initialization to sandbox session availability:
   - If sidecar socket is valid: shell tool routes through session backend.
   - If unavailable: shell/browser tools are disabled with explicit status.
3. Keep current tool contract stable where possible, but switch execution backend from direct host shell to daemon-backed transport when enabled.

### WS6: Bootstrap Ownership Cutover
1. Move ownership of startup wiring (config/provider/memory/runtime-limits assembly) out of legacy standalone `cli` startup path into runner-era bootstrap path.
2. Keep reusable builders where useful, but make runner/bootstrap path the canonical entrypoint for Phase 10.
3. Ensure process-tier direct execution remains possible for development while auto-disabling sidecar-dependent tools.

## Verification Plan (Phase 10 Gate)
1. **Unit tests**
   - `types`: `SandboxTier`, runner config, bootstrap envelope encoding/decoding.
   - `sandbox`: shell session backend behavior + unavailable-sidecar semantics.
   - `shell-daemon`: protocol command lifecycle and stream framing.
2. **Integration tests**
   - Runner provisions per-user workspace directories on first start.
   - Runner emits expected tier logs and warnings for `--insecure`.
   - Runtime + shell daemon end-to-end: basic shell command returns stdout.
   - Process tier: shell/browser disabled status is explicit and test-asserted.
3. **Repo validation command**
   - `cargo fmt --all --check`
   - `cargo clippy --workspace --all-targets --all-features -- -D warnings`
   - `cargo test -p types -p sandbox -p shell-daemon -p runner -p runtime`

## Risks and Mitigations
- **Transport complexity (vsock/unix differences):** define transport abstraction early; keep Unix socket fallback for local/dev and CI.
- **Cross-platform hardening drift:** isolate Landlock/Seatbelt behind target-specific modules with explicit no-op + warnings on unsupported platforms.
- **Bootstrap regression risk:** add contract tests for length-prefixed envelope parsing and startup failure behavior (missing/invalid sidecar address).
- **Scope creep into Phase 11:** gate Phase 10 to runner/daemon/session transport only; defer Wasmtime capability policy to next phase.

## Deliverables Checklist
- [ ] `runner` crate added and wired into workspace.
- [ ] `shell-daemon` crate added and wired into workspace.
- [ ] `SandboxTier` and runner/bootstrap protocol types in `types`.
- [ ] `sandbox` crate implements session traits + backends.
- [ ] Runtime startup consumes bootstrap envelope and handles sidecar availability explicitly.
- [ ] `--insecure` process mode works with clear warning + disabled shell/browser behavior.
- [ ] Phase 10 verification tests passing with fmt/clippy/test gate.

## Implementation Order (Recommended)
1. WS1 (`types`) -> 2. WS3 (`shell-daemon` protocol server) -> 3. WS4 (`sandbox` client/backends) -> 4. WS2 (`runner`) -> 5. WS5 (`runtime/tools`) -> 6. WS6 (cutover/cleanup + full verification).
