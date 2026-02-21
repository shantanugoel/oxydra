# Chapter 10: Testing and Quality

## Testing Strategy

Oxydra follows a cumulative testing strategy where later phases add tests but never remove earlier checks. Each phase's verification gate is represented by automated tests.

## Test Coverage by Crate

### `types` — Contract Tests

**Location:** `crates/types/tests/`

- **Serde round-trips:** Core types (`Context`, `Message`, `ToolCall`, `Response`) serialize and deserialize without data loss
- **Error conversions:** Domain errors (`ProviderError`, `ToolError`, `MemoryError`) convert correctly to `RuntimeError` via `#[from]`
- **Model catalog:** Validates the pinned JSON snapshot loads correctly, model IDs are unique, and capability lookups return expected values

### `provider` — Provider Contract Tests

**Location:** `crates/provider/src/tests.rs`

- **API key precedence:** Explicit keys override provider-specific env vars, which override generic fallback
- **Request normalization:** Internal `Context` is correctly mapped to provider-specific wire formats (OpenAI and Anthropic) — verified via `insta` snapshot tests
- **Streaming:** SSE parsing handles fragmented frames, split Unicode sequences, and multi-tool delta reassembly
- **Error mapping:** Transport errors, HTTP status codes (401, 429, 5xx), and response parse errors map to correct `ProviderError` variants
- **Reliability:** `ReliableProvider` retries on transport failures but surfaces fatal errors immediately

**Mock infrastructure:**
- `SequencedProvider` — trait-based mock that returns a scripted sequence of results
- `spawn_one_shot_server` — spawns a local TCP server to mock real HTTP responses

### `tools` — Tool and Registry Tests

**Location:** `crates/tools/src/tests.rs`

- **Core tool logic:** Unit tests for `ReadTool`, `WriteTool`, `EditTool`, `BashTool`
- **Security policy:** `ToolRegistry` policy hooks gate privileged tools and enforce path restrictions
- **WASM runner:** `VaultCopyToTool` two-step invocation is tested for audit trail integrity
- **Truncation/timeouts:** Registry correctly truncates oversized output and enforces execution timeouts

### `tools-macros` — Compile-Fail Tests

**Location:** `crates/tools-macros/tests/compile.rs`

Uses `trybuild` to verify that the `#[tool]` macro:
- Accepts async functions with supported argument types
- Rejects non-async functions with a clear compile error
- Rejects unsupported argument types with a clear compile error

### `runtime` — Integration Tests

**Location:** `crates/runtime/src/tests.rs`, `crates/runtime/tests/openai_compatible_e2e.rs`

- **Session loop:** Agent loop with tool call reconstruction from streams and multi-turn execution
- **Security policies:** Denial of out-of-workspace file access and command allowlist enforcement
- **Data safety:** Scrubbing/redaction of sensitive strings from tool outputs
- **Sidecar integration:** Shell command execution via Unix domain socket to shell-daemon
- **Resilience:** Fallback from streaming to completion on failure; budget/limit enforcement
- **Parallelism:** `ReadOnly` tools execute concurrently while `SideEffecting` tools run sequentially

**Mock infrastructure:**
- `MockProvider` / `MockTool` — `mockall`-generated mocks for trait contracts
- `FakeProvider` — scripted stateful provider for complex streaming/completion scenarios
- `RecordingMemory` — in-memory `Memory` implementation that records all operations

### `memory` — Persistence Tests

**Location:** `crates/memory/src/tests.rs`

- **libSQL integration:** Durability of `store`, `recall`, `forget` across restarts
- **Hybrid search:** Merging vector similarity and FTS results with weighted ranking
- **Schema migrations:** Automatic migration pipeline with data preservation
- **Concurrency:** `write_summary_state` epoch-based CAS prevents lost updates

## E2E Test Infrastructure

### `test.sh`

```bash
test.sh local  # Runs mock-provider E2E tests
test.sh live   # Runs smoke test against OpenRouter (requires OPENROUTER_API_KEY)
test.sh all    # Runs both
```

The `local` test (`openai_compatible_runtime_e2e_exposes_tools_and_executes_loop`) mocks the LLM provider and verifies the full agent loop: context construction → provider call → tool detection → execution → response assembly.

The `live` test (`live_openrouter_tool_call_smoke`) sends a real request through OpenRouter to verify end-to-end provider communication.

## CI Pipeline

**File:** `.github/workflows/quality-checks.yml`

The CI pipeline runs on every push and PR:

| Check | Tool | Policy |
|-------|------|--------|
| Formatting | `cargo fmt --check` | Hard fail on formatting violations |
| Supply chain | `cargo deny check` | License allowlist + source verification |
| Linting | `clippy` | Deny all warnings (no `#[allow]` directives) |
| Tests | `cargo test` | Full test suite for all crates |
| Snapshots | Provider-specific `insta` tests | Verify wire format stability |

## `deny.toml` Policies

- **Bans:** Warns on multiple versions of the same crate in the dependency tree
- **Licenses:** Strict allowlist of OSI-approved licenses (MIT, Apache-2.0, BSD-2/3-Clause, ISC, MPL-2.0, Unicode-3.0, Zlib, OpenSSL)
- **Sources:** Denies unknown git or registry sources to ensure supply chain integrity

## Mocking Patterns

### Trait-Based Mocking (`mockall`)

Used for simple interface contracts. Example:

```rust
mock! {
    pub TestProvider {}
    #[async_trait]
    impl Provider for TestProvider {
        fn provider_id(&self) -> &ProviderId;
        async fn complete(&self, ctx: &Context) -> Result<Response, ProviderError>;
        // ...
    }
}
```

### Stateful Fakes

For complex scenarios requiring multi-step behavior:

- `FakeProvider` — returns scripted responses in sequence, supports both streaming and non-streaming
- `RecordingMemory` — in-memory persistence that records all operations for assertion
- `SequencedProvider` — returns different results on successive calls (for retry testing)

### Local HTTP Servers

`spawn_one_shot_server` creates an ephemeral TCP server that returns a predefined HTTP response. Used to test real HTTP client behavior (headers, status codes, SSE parsing) without external dependencies.

## Snapshot Testing

The `insta` crate is used extensively in the `provider` crate to verify serialization stability:

- OpenAI request body with tools
- Anthropic request body with system messages
- Response parsing from both providers
- Error envelope parsing

Snapshots are committed to the repository and reviewed as part of PRs. Any serialization change shows up as a snapshot diff.

## Key Testing Principles

1. **Every phase gate has tests** — phase closure requires passing automated verification
2. **Security-first testing** — every tool and runtime test includes policy enforcement scenarios
3. **Regression is cumulative** — later phases add tests but never remove earlier checks
4. **No clippy suppressions** — code must satisfy clippy without `#[allow]` directives
5. **Deterministic mocks** — external dependencies are mocked for reproducible test results
