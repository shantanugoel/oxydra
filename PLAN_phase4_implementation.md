# Phase 4 Implementation Plan (Appendix Progressive Build Plan)

## 1) Objective and Scope

Implement **Phase 4** from the Appendix build plan:
- `Tool` trait with:
  - `fn schema()`
  - `async fn execute()`
  - `fn timeout()`
  - `fn safety_tier()`
- `#[tool]` procedural macro that generates correct `FunctionDecl` JSON metadata from Rust tool functions.
- Core tools: **Read**, **Write**, **Edit**, **Bash**.

Out of scope for this phase (but designed for): runtime self-correction loop and parallel execution policy mechanics (Phase 6), sandbox enforcement (Phase 10), MCP adapter integration (Phase 15).

## 2) Inputs Reviewed

Primary:
- Chapter 5 (Tool Calling, Procedural Macros, Schema Validation)
- Appendix:
  - Recommended Rust Crate Ecosystem
  - Progressive Build Plan (Phase 4 row)
  - Why this order avoids rewrites

Supporting:
- Chapter 1 (crate layering and minimal-core boundaries)
- Chapter 2 (trait design and dyn dispatch expectations)
- Chapter 3 (runtime uses `timeout` and `safety_tier` later)
- Chapter 4 (streaming/tool-call accumulation context for downstream integration)

Source code reviewed:
- `crates/types` (existing error/types/provider traits)
- `crates/tools` (currently scaffold/stub)
- `crates/tools-macros` (currently scaffold/stub)
- `crates/provider` (current tool-call payload normalization shape)

## 3) Current Baseline and Gap Summary

### Implemented today
- `types` has foundational message/provider models and `ToolError`.
- `provider` already handles tool-call parsing/normalization in responses.
- Workspace contains required crates (`types`, `tools`, `tools-macros`).

### Missing for Phase 4
- No `Tool` trait contract yet.
- No `FunctionDecl` schema representation in internal types.
- No `SafetyTier` abstraction.
- `tools` crate has no core tool implementations.
- `tools-macros` crate is not yet a proc-macro implementation.

## 4) Rust Crate Evaluation (Phase-4 Relevant)

This evaluates all ecosystem recommendations that materially affect Phase 4 decisions.

| Concern | Candidate(s) from brief | Evaluation | Phase 4 Decision |
|---|---|---|---|
| Tool trait async dispatch | `async-trait` | Already used in workspace; stable and dyn-compatible on current Rust stable. | **Use now** (types/tool trait boundary). |
| Schema serialization | `serde` + `serde_json` | Already standard across codebase; needed for `FunctionDecl` JSON. | **Use now**. |
| Schema generation | `schemars` or `tools-rs` | `schemars` is low lock-in and works with internal types; `tools-rs` gives faster macro ergonomics but can pull architecture toward external conventions. | **Default: `schemars` + internal macro**. Keep `tools-rs` as optional evaluation spike behind feature flag only if needed. |
| Proc-macro implementation | `syn` + `quote` + `proc-macro2` | Canonical stack for robust attribute parsing and expansion; explicitly recommended for Phase 4. | **Use now** in `tools-macros`. |
| Tool validation runtime | `jsonschema` | Recommended in brief but formally introduced in Phase 6 verification gate. | **Design extension points now; defer runtime adoption to Phase 6**. |
| Runtime timeout/cancellation wiring | `tokio` | Already in workspace and required for async tool execution/timeouts. | **Use existing tokio stack**. |
| Error modeling | `thiserror` | Existing pattern in `types`; preserves per-layer composition. | **Continue existing pattern**. |

## 5) Proposed Architecture for Phase 4

### 5.1 `types` crate (contracts + schema model)
1. Add tool contract module(s) with:
   - `Tool` trait (object-safe where required for registry usage).
   - `SafetyTier` enum (minimum: safe/read-only vs side-effecting/privileged tier).
   - `FunctionDecl` and schema payload structs that serialize to provider-compatible JSON.
2. Keep error composition aligned with existing `ToolError` usage.
3. Export new tool-contract symbols from `types::lib`.

### 5.2 `tools-macros` crate (`#[tool]` proc macro)
1. Convert crate to `proc-macro = true`.
2. Add `syn`/`quote`/`proc-macro2` dependencies.
3. Implement `#[tool]` for `async fn` tool declarations:
   - Parse function name, doc comments, and typed args.
   - Generate schema metadata (`FunctionDecl`) from signature/argument model.
   - Emit compile-time diagnostics for unsupported signatures.
4. Ensure generated output satisfies verification gate pattern:
   - `#[tool] async fn read_file(path: String) -> String` compiles.
   - Generated schema JSON is correct and deterministic.

### 5.3 `tools` crate (core tools + registry policy hooks)
1. Implement core tools:
   - `ReadTool`
   - `WriteTool`
   - `EditTool`
   - `BashTool`
2. Ensure each tool provides:
   - schema metadata
   - execution entrypoint
   - timeout default
   - safety tier
3. Add registry surface that can enforce:
   - per-tool timeout
   - output truncation policy
   - safety-tier gate hook (for later HITL/sandbox integration)

## 6) Work Packages and Dependency Order

1. **WP1 - Tool contracts in `types`**
   - Add `Tool` trait + schema structs + `SafetyTier`.
   - Add serde tests for schema JSON shape.
2. **WP2 - Macro crate bootstrapping**
   - Convert `tools-macros` to proc macro crate.
   - Add parser + codegen skeleton and compile checks.
3. **WP3 - Core tool implementations**
   - Implement read/write/edit/bash tools in `tools`.
   - Wire to new trait contract from `types`.
4. **WP4 - Macro + tool integration**
   - Use `#[tool]` for representative functions and validate emitted `FunctionDecl`.
   - Add deterministic tests for generated schema.
5. **WP5 - Phase gate verification**
   - Confirm Phase 4 acceptance test condition from brief.
   - Run formatting, clippy, and targeted tests for touched crates.

Dependencies:
- WP2 depends on WP1 type definitions for generated schema target.
- WP3 depends on WP1 trait contracts.
- WP4 depends on WP2 + WP3.
- WP5 depends on all previous work packages.

## 7) Verification Plan

Phase 4 is complete when all conditions below hold:
1. `#[tool] async fn read_file(path: String) -> String` compiles in test fixture.
2. Generated `FunctionDecl` JSON matches expected schema snapshot.
3. Core `Read/Write/Edit/Bash` tools execute through the common `Tool` contract.
4. Tool defaults expose `timeout()` and `safety_tier()` for runtime consumption.
5. CI-quality checks pass for touched crates:
   - `cargo fmt --all --check`
   - `cargo clippy --workspace --all-targets --all-features -- -D warnings`
   - `cargo test -p types -p tools -p tools-macros`

## 8) Risks and Mitigations

1. **Macro complexity drift**
   - Mitigation: keep macro focused on schema derivation only; avoid embedding runtime policy logic.
2. **Schema compatibility drift with provider payloads**
   - Mitigation: snapshot tests on serialized `FunctionDecl` and adapter-level tests.
3. **Over-coupling to third-party tool frameworks**
   - Mitigation: keep external crates behind feature flags/adapters; no external types in public runtime/tool contracts.
4. **Cross-platform behavior differences in file/bash tools**
   - Mitigation: normalize path handling and command execution boundaries; ensure deterministic error mapping.

## 9) Proposed Defaults (Unless You Prefer Otherwise)

1. Schema generation path: **`schemars` + internal `#[tool]` macro** (recommended default).
2. Macro stack: **`syn` + `quote` + `proc-macro2`**.
3. Keep `jsonschema` integration deferred to Phase 6, but expose compatible interfaces in Phase 4.
4. Keep `tools-rs` as an optional evaluation branch only, not core architecture.

