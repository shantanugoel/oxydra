# Phase 7 Implementation Plan

## Problem Statement

Phase 7 (per `oxydra-project-brief.md`) requires adding a second provider (Anthropic), deterministic configuration loading/validation, runtime provider switching via config, and reliability wrapping (retry/backoff) without breaking existing Phase 1-6 contracts.

## Source Alignment (Reviewed)

- **Chapter 1**: deterministic config precedence + `config_version` validation + credential resolution chain.
- **Chapter 2**: provider trait stays stable; add Anthropic adapter and `ReliableProvider`.
- **Chapters 3-5**: runtime/tool loops already exist and should remain provider-agnostic.
- **Appendix (crate ecosystem + progressive plan)**:
  - Introduce `config` or `figment` in Phase 7.
  - Keep provider abstraction internal and vendor-specific wire formats isolated.
  - Verification includes provider serialization snapshots and credential precedence fixtures.

## Current Codebase Baseline (Gap Analysis)

- `crates/provider/src/lib.rs` currently implements **OpenAI only** (`OpenAIProvider`), with OpenAI request/response mapping, streaming, and API key precedence (`explicit -> OPENAI_API_KEY -> API_KEY`).
- `crates/runtime/src/lib.rs` already consumes `Box<dyn Provider>` and is provider-agnostic in the turn loop.
- `crates/types/src` has core provider/model/message/tool types, but **no typed config module yet**.
- `crates/cli/src/lib.rs` is still placeholder code; no config loading/provider factory path exists.
- `crates/types/data/pinned_model_catalog.json` currently contains only OpenAI models; Anthropic model validation would fail without catalog updates.

## Implementation Approach

### 1) Add typed Phase 7 config surface in `types`

**Files**
- Add: `crates/types/src/config.rs`
- Update: `crates/types/src/lib.rs`
- Add tests: `crates/types/tests/config_contracts.rs`

**Planned content**
- `AgentConfig` root with `config_version`, runtime limits, selected provider/model, and provider-specific settings.
- Provider config structs for `openai` and `anthropic` with optional explicit `api_key`.
- Reliability settings (retry attempts, backoff base/max/jitter toggle).
- Validation entrypoint that:
  - rejects unsupported/unknown major `config_version`,
  - verifies provider/model selection shape before startup.

**Why here**
- Keeps single source config schema in foundation layer (`types`) per brief and avoids config-shape duplication across CLI/provider/runtime.

---

### 2) Add Anthropic provider adapter in `provider`

**Files**
- Update/split: `crates/provider/src/lib.rs` (or small internal modules)
- Optional new snapshot fixtures under provider tests

**Planned content**
- `AnthropicConfig` and `AnthropicProvider`.
- `/v1/messages` request serializer mapping internal `Context` + tools to Anthropic message/tool schema.
- Response normalizer mapping Anthropic blocks (`text`, `tool_use`, etc.) into internal `Response` / `ToolCall`.
- Shared credential resolution helper for:
  - explicit config key
  - provider env key (`ANTHROPIC_API_KEY` or `OPENAI_API_KEY`)
  - generic `API_KEY` fallback (kept explicit and controlled).
- Ensure provider/model catalog validation is enforced before network call (same as OpenAI path).

**Testing**
- Snapshot test(s) for Anthropic request payload stability.
- Unit tests for response normalization and error mapping (`Transport`, `HttpStatus`, `ResponseParse`).
- Credential precedence fixtures for Anthropic.

---

### 3) Introduce `ReliableProvider` wrapper in `provider`

**Files**
- Update: `crates/provider/src/lib.rs` (or `reliable.rs`)
- Update tests in provider crate

**Planned content**
- `ReliableProvider` wrapping `Box<dyn Provider>` (or generic wrapper), preserving `Provider` trait contract.
- Retry policy with bounded attempts and exponential backoff.
- Retriable-classification policy:
  - retry: transport failures, stream connection loss, HTTP 429/5xx
  - no retry: model validation errors, auth/permission 4xx, deterministic parse/schema errors.

**Testing**
- Deterministic fake provider tests validating:
  - retries happen for retriable failures then succeed,
  - no retry for non-retriable failures,
  - max retries enforced.

---

### 4) Implement config loader + provider switching in `cli`

**Files**
- Update: `crates/cli/Cargo.toml`
- Replace placeholder: `crates/cli/src/lib.rs`
- Add tests: `crates/cli/src/lib.rs` test module or `crates/cli/tests/*.rs`

**Planned content**
- Add loader crate **`figment`** (selected for Phase 7).
- Deterministic precedence chain:
  1. in-code defaults
  2. `/etc/oxydra/agent.toml` + `/etc/oxydra/providers.toml`
  3. `~/.config/oxydra/agent.toml` + `~/.config/oxydra/providers.toml`
  4. `./.oxydra/agent.toml` + `./.oxydra/providers.toml`
  5. env overrides (`OXYDRA__...`)
  6. CLI flags (highest)
- Use Figment profiles (`default` + environment profiles like `dev`/`prod`) so profile-specific overrides layer on top of shared defaults.
- Config validation gate on startup, including `config_version`.
- Provider factory that instantiates OpenAI or Anthropic from config and wraps with `ReliableProvider`.
- Provider switching by changing config only (no runtime code changes).

**Testing**
- Precedence fixture tests (file/env/profile/explicit overrides).
- Environment nesting tests for `OXYDRA__...` key paths.
- `config_version` accept/reject tests.
- Provider switch smoke tests (openai vs anthropic path selection from config).

---

### 5) Update model catalog data and compatibility checks

**Files**
- Update: `crates/types/data/pinned_model_catalog.json`
- Update/add tests in `crates/types/tests/core_types_contracts.rs` and provider tests

**Planned content**
- Add at least one Anthropic model descriptor with provider id `anthropic` and capability metadata.
- Keep deterministic ordering/format aligned with existing catalog utilities.

**Why**
- `Provider::capabilities()` path validates against catalog; Anthropic support requires catalog entries.

---

### 6) Verification Gate (Phase 7 completion criteria)

Run after implementation:

1. `cargo fmt --all --check`
2. `cargo clippy --workspace --all-targets --all-features -- -D warnings`
3. `cargo test -p types -p provider -p tools -p tools-macros -p runtime -- --skip live_openai_smoke_normalizes_response`
4. `cargo test -p provider snapshot`
5. `cargo test -p cli`

Optional CI follow-up:
- Extend workflow test matrix to include `-p cli` so config/provider-switch regressions are gated in PRs.

## Execution Order (Minimal-Risk)

1. `types` config schema + tests
2. Anthropic provider serializer/normalizer + tests
3. `ReliableProvider` + retry tests
4. CLI loader/provider factory + precedence tests
5. Model catalog update + validation tests
6. Full verification gate

## Notes / Decision

- **Config loader crate is now fixed to `figment`** for this phase.
- **Plan impact**:
  - CLI implementation should use Figment providers/composition (`Serialized` defaults + TOML files + env + CLI overrides).
  - Add profile-selection coverage (`dev`/`prod`) in CLI config tests.
  - Replace any prior `config`-crate-specific assumptions in implementation notes and tests with Figment-native layering.
