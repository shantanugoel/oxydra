# Phase 11 Implementation Plan
## Application-layer security policy + WASM tool isolation

## 1) Scope and outcomes

This plan implements **Phase 11** from `oxydra-project-brief.md` (Appendix phase table + Chapter 7 security guidance), building directly on the current Phase 10 runner/bootstrap baseline.

Primary outcomes:

1. Introduce **wasmtime-based isolation** for filesystem/web/vault tools with capability-scoped mounts.
2. Add a **SecurityPolicy layer** with command allowlisting, path traversal prevention, and explicit deny reasons.
3. Add **credential-like output scrubbing** (keyword + high-entropy heuristic) before tool output is persisted or returned to the model/user.
4. Deliver the **new Phase 11 tool surface** and remove legacy tool-name compatibility aliases.
5. Satisfy the Phase 11 verification gate:
   - `web_fetch` blocks metadata/private destinations (`169.254.169.254`, `192.168.x.x`, etc.),
   - `vault_copyto` runs as two auditable sequential WASM invocations,
   - file tools enforce mount boundaries,
   - legacy `read_file`/`write_file`/`edit_file`/`bash` names are no longer exposed.

---

## 2) Current baseline (review summary)

Reviewed in detail:

- `oxydra-project-brief.md` (full file)
- `crates/tools/src/lib.rs`
- `crates/sandbox/src/lib.rs`
- `crates/runtime/src/lib.rs`
- `crates/runtime/src/tool_execution.rs`
- `crates/runtime/src/tests.rs`
- `crates/runtime/tests/openai_compatible_e2e.rs`
- `crates/types/src/tool.rs`
- `crates/types/src/runner.rs`
- `crates/types/tests/{tool_contracts,core_types_contracts,runner_contracts}.rs`
- `crates/provider/src/tests.rs`
- `crates/cli/src/lib.rs`
- `crates/{sandbox,tools,runtime,types}/Cargo.toml`

Key baseline observations:

1. `tools` currently exposes native host tools named `read_file`, `write_file`, `edit_file`, `bash` with no mount/path isolation and no output scrubbing.
2. `sandbox` currently focuses on shell-daemon session transport/backends (vsock/unix/local process), not WASM tool runners.
3. `runtime` validates tool args and executes tools, but does not apply a security policy or redact sensitive output.
4. Runner bootstrap already provides workspace root + sidecar endpoint, which is sufficient to derive `shared/tmp/vault` policy boundaries.
5. A large test surface across `runtime`, `provider`, `types`, and `cli` currently hardcodes legacy tool names.

---

## 3) Target Phase 11 architecture

## 3.1 Tool-surface hard cutover

Move from legacy names to a new explicit Phase 11 surface (no compatibility aliases), with file-class and web-class separation.

Planned canonical set:

- File RO: `file_read`, `file_search`, `file_list`
- File RW: `file_write`, `file_edit`, `file_delete`
- Web: `web_fetch`, `web_search`
- Vault flow: `vault_copyto` (and `vault_copyfrom` if needed by existing behavior)
- Shell: `shell_exec` (sidecar-backed privileged shell operation; legacy `bash` removed)

> Assumption: the above naming is adopted for the hard cutover; if you want different canonical names, keep this plan structure and swap names centrally.

## 3.2 Security policy enforcement model

Every tool invocation should pass through a single deterministic policy pipeline:

1. **Tool name + class policy** (known tool, allowed mode)
2. **Path policy** (canonicalization + mount containment + traversal blocking)
3. **Command policy** (for `shell_exec`: allowlist + explicit deny reason)
4. **Execution isolation policy** (WASM capability profile / sidecar constraints)
5. **Output policy** (size truncation then credential scrubbing)

All policy denials must return structured, auditable errors (no silent drops).

## 3.3 WASM isolation model

Introduce `wasmtime` runners in `sandbox` for non-shell tool classes:

- RO tools mount `shared/tmp/vault` read-only.
- RW tools mount `shared/tmp` read-write; **never** mount vault.
- Web tools mount no filesystem and use host-function HTTP proxy only.
- Vault copy runs in two host-orchestrated WASM invocations (vault-read step then shared/tmp-write step), ensuring no single instance sees both mount sets.

## 3.4 Web egress policy model

`web_fetch` / `web_search` must:

1. Parse/normalize URL.
2. Resolve hostname to IP(s) before connect.
3. Reject blocked ranges (loopback, link-local, RFC1918, metadata endpoints).
4. Execute through controlled host proxy path only.

---

## 4) Workstreams and file-level implementation plan

## WS1 — Add security policy primitives and integration points

**Primary crates/files**
- `crates/sandbox/src/lib.rs` (or split into `policy.rs` + `wasm.rs` modules)
- `crates/tools/src/lib.rs`
- `crates/runtime/src/tool_execution.rs`

**Tasks**
1. Define `SecurityPolicy` interfaces and concrete Phase 11 default implementation.
2. Add canonical path checks (`std::fs::canonicalize` + containment checks against workspace roots).
3. Add explicit command allowlist logic for shell tool class.
4. Thread policy checks through tool execution paths so denials produce explicit `ToolError`.

**Deliverable**
- Single enforcement path used for all tool invocations.

---

## WS2 — Implement WASM tool runner and capability profiles

**Primary crates/files**
- `crates/sandbox/Cargo.toml` (add `wasmtime` and supporting deps)
- `crates/sandbox/src/lib.rs` (new WASM execution abstractions + audit hooks)
- `crates/tools/src/lib.rs` (wire tool handlers to sandbox WASM runner)

**Tasks**
1. Add WASM runner abstraction in `sandbox` with per-tool capability profile.
2. Define mount profile constructors for RO, RW, web, and vault-step invocations.
3. Add auditable invocation metadata (tool name, profile, mount set, request ID).
4. Keep existing shell-daemon session traits intact for shell/browser channel behavior from Phase 10.

**Deliverable**
- Production path for file/web/vault tools routes through WASM runner, not direct host fs/network calls.

---

## WS3 — Tool registry cutover to Phase 11 surface

**Primary crates/files**
- `crates/tools/src/lib.rs`
- `crates/cli/src/lib.rs` (tests that assert disabled shell tool behavior)
- `crates/runtime/src/tests.rs`
- `crates/runtime/tests/openai_compatible_e2e.rs`

**Tasks**
1. Replace legacy constants and registrations with new tool names.
2. Remove compatibility alias registration (`read_file`/`write_file`/`edit_file`/`bash`).
3. Keep sidecar bootstrap availability behavior, but under new shell tool name.
4. Update tests and fixtures to assert unknown-tool failure for removed legacy names.

**Deliverable**
- Runtime exposes only the new Phase 11 tool surface.

---

## WS4 — Web-fetch/web-search network hardening

**Primary crates/files**
- `crates/sandbox/src/lib.rs`
- `crates/tools/src/lib.rs`
- new/updated tests under `crates/tools/src/lib.rs` and/or `crates/sandbox/src/lib.rs`

**Tasks**
1. Implement URL + DNS resolution checks before outbound calls.
2. Block `127.0.0.0/8`, `169.254.0.0/16`, `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`, and metadata endpoints.
3. Ensure direct raw socket access is never used for web tools.
4. Add explicit tests for `169.254.169.254` and `192.168.x.x` rejection.

**Deliverable**
- Web tools are deterministic, deny-by-default for private/metadata network targets.

---

## WS5 — Vault copy two-step orchestration and auditability

**Primary crates/files**
- `crates/tools/src/lib.rs`
- `crates/sandbox/src/lib.rs`
- runtime/tool tests for audit assertions

**Tasks**
1. Implement `vault_copyto` orchestration with two separate WASM invocations.
2. Ensure invocation A has vault-read/no shared-write, invocation B has shared/tmp-write/no vault.
3. Emit auditable structured records linking both invocations by one operation ID.
4. Add tests asserting:
   - exactly two invocations emitted,
   - mount sets differ as required,
   - operation succeeds/fails with clear error propagation.

**Deliverable**
- Vault copy semantics match Chapter 7 isolation requirements.

---

## WS6 — Credential output scrubbing

**Primary crates/files**
- `crates/runtime/Cargo.toml` (if additional regex crate features needed)
- `crates/runtime/src/tool_execution.rs` (or dedicated `scrubbing.rs`)
- `crates/runtime/src/tests.rs`

**Tasks**
1. Add compiled `RegexSet` + token heuristics (keyword + high-entropy) scrubber.
2. Scrub tool output before it is appended as `MessageRole::Tool` content.
3. Preserve non-sensitive text and avoid over-redaction for normal output.
4. Add tests for `api_key`, `token`, `password`, `bearer` style content and high-entropy blobs.

**Deliverable**
- Tool outputs are redacted consistently and visibly (e.g., `[REDACTED]` markers).

---

## WS7 — Update provider/type/runtime fixtures impacted by tool-name cutover

**Primary crates/files**
- `crates/provider/src/tests.rs`
- `crates/types/tests/tool_contracts.rs`
- `crates/types/tests/core_types_contracts.rs`
- potentially `tools-macros` fixtures only where they represent runtime-default tool names

**Tasks**
1. Update test payloads/snapshots to the new canonical tool names.
2. Keep provider normalization logic unchanged unless assumptions are tool-name specific.
3. Ensure no test implicitly validates removed legacy names as runtime defaults.

**Deliverable**
- Test suite reflects hard cutover behavior without transitional aliases.

---

## 5) Execution order (dependency-safe)

1. **Policy primitives + WASM abstractions first** (WS1 + WS2).
2. **Tool registry and names cutover** (WS3).
3. **Network and vault policy-specific behavior** (WS4 + WS5).
4. **Runtime output scrubbing** (WS6).
5. **Cross-crate fixture/snapshot updates** (WS7).
6. Run full verification gate and fix regressions in touched scope only.

---

## 6) Testing and verification plan

## 6.1 New/updated tests required for Phase 11 gate

1. `web_fetch` rejects metadata/private ranges (`169.254.169.254`, `192.168.x.x`).
2. `vault_copyto` emits two auditable invocations with disjoint mount profiles.
3. File tools reject traversal / out-of-bound paths.
4. Credential-like output redaction is applied.
5. Legacy tool names return unknown-tool errors (no compatibility aliases).

## 6.2 Regression coverage to keep green

- Existing runtime loop behavior (streaming, tool-call accumulation, correction loop)
- Sidecar shell disabled/enabled status behavior from Phase 10 bootstrap
- Provider request/response normalization tests

## 6.3 Verification commands

Recommended gate for this phase:

1. `cargo fmt --all --check`
2. `cargo clippy --workspace --all-targets --all-features -- -D warnings`
3. `cargo test -p types -p sandbox -p tools -p runtime -p provider -p cli`

If runner/shell-daemon interfaces are touched by refactors, extend with:

- `cargo test -p runner -p shell-daemon`

---

## 7) Risks and mitigations

1. **Blast radius from tool-name hard cutover**  
   Mitigation: centralize names as constants, perform one-pass test fixture update, add explicit “legacy name denied” tests.

2. **Over-redaction in scrubbing**  
   Mitigation: combine keyword and entropy heuristics; add negative tests to protect normal output.

3. **WASM integration complexity in monolithic sandbox module**  
   Mitigation: introduce small internal modules (`policy`, `wasm_runner`) while preserving public API shape.

4. **Network policy bypass via hostname tricks**  
   Mitigation: resolve hostnames before checks and validate all resolved IPs.

---

## 8) Definition of done for Phase 11

Phase 11 is complete when:

1. New tool surface is active and legacy aliases are removed.
2. WASM isolation + capability mount profiles are enforced for file/web/vault tools.
3. Security policy blocks forbidden path/command/network behavior with auditable errors.
4. Credential-like output is scrubbed before tool results enter runtime context.
5. All tests and verification gates above pass cleanly.
