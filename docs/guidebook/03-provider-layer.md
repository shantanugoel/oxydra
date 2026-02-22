# Chapter 3: Provider Layer

## Overview

The provider layer normalizes disparate LLM APIs into a unified internal representation. Every provider — regardless of wire format — produces the same `Response` and `StreamItem` types consumed by the runtime. This allows the agent loop to be entirely provider-agnostic.

Four provider implementations are available:

| Provider | Type Key | Endpoint | Status |
|----------|----------|----------|--------|
| OpenAI Chat Completions | `openai` | `/v1/chat/completions` | Full streaming |
| Anthropic Messages | `anthropic` | `/v1/messages` | Shim streaming (complete → emit) |
| Google Gemini | `gemini` | `/v1beta/models/{model}:streamGenerateContent` | Full streaming |
| OpenAI Responses | `openai_responses` | `/v1/responses` | Full streaming with session chaining |

## The Provider Trait

Defined in `types/src/provider.rs`:

```rust
#[async_trait]
pub trait Provider: Send + Sync {
    fn provider_id(&self) -> &ProviderId;
    fn catalog_provider_id(&self) -> &ProviderId { self.provider_id() }
    fn model_catalog(&self) -> &ModelCatalog;
    fn capabilities(&self, model: &ModelId) -> Result<ProviderCaps, ProviderError>;
    async fn complete(&self, context: &Context) -> Result<Response, ProviderError>;
    async fn stream(
        &self,
        context: &Context,
        buffer_size: usize,
    ) -> Result<ProviderStream, ProviderError>;
}
```

- `complete` — blocking request/response, returns the full response at once
- `stream` — returns a bounded `mpsc::Receiver<StreamItem>` for real-time token delivery
- `capabilities` — returns `ProviderCaps` for a given model, derived from the catalog with Oxydra overlay applied
- `catalog_provider_id` — returns the catalog provider namespace used for model validation; defaults to `provider_id()` but can be overridden (e.g. `ResponsesProvider` returns `"openai"` while its instance ID is `"openai-responses"`)

`ProviderStream` is a type alias for `tokio::sync::mpsc::Receiver<StreamItem>`.

### Default `capabilities` Implementation

The trait provides a default implementation for `capabilities` that uses the three-tier overlay system:

```rust
fn capabilities(&self, model: &ModelId) -> Result<ProviderCaps, ProviderError> {
    let catalog = self.model_catalog();
    let catalog_provider = self.catalog_provider_id();
    let descriptor = catalog.validate(catalog_provider, model)?;
    Ok(derive_caps(catalog_provider.0.as_str(), descriptor, &catalog.caps_overrides))
}
```

This validates the model against the catalog, then applies `derive_caps()` to merge the baseline descriptor with provider-level and model-specific overrides.

## Provider Capabilities

```rust
pub struct ProviderCaps {
    pub supports_streaming: bool,
    pub supports_tools: bool,
    pub supports_json_mode: bool,
    pub supports_reasoning_traces: bool,
    pub max_input_tokens: Option<u32>,
    pub max_output_tokens: Option<u32>,
    pub max_context_tokens: Option<u32>,
}
```

Capabilities are derived at request time using a three-tier overlay:

1. **Baseline** — `ModelDescriptor::to_provider_caps()` maps models.dev schema fields to `ProviderCaps`
2. **Provider defaults** — `CapsOverrides.provider_defaults["openai"]` applies to all models under that provider
3. **Model-specific** — `CapsOverrides.overrides["openai/gpt-4o"]` overrides specific models (highest priority)

The runtime uses these to decide whether to call `stream()` or fall back to `complete()`, and whether to include tool declarations in the context.

## Model Catalog

The `ModelCatalog` is a registry of `ModelDescriptor` entries organized by catalog provider, embedded into the binary at build time from `types/data/pinned_model_catalog.json`. A companion `oxydra_caps_overrides.json` file provides Oxydra-specific overlays. Both are loaded together via `ModelCatalog::from_pinned_snapshot()`.

### ModelDescriptor (models.dev-aligned)

Each model entry is fully aligned with the [models.dev](https://models.dev) per-model schema:

```rust
pub struct ModelDescriptor {
    pub id: String,              // e.g. "gpt-4o"
    pub name: String,            // e.g. "GPT-4o"
    pub family: Option<String>,  // e.g. "gpt-4o"
    pub attachment: bool,        // file/image attachment support
    pub reasoning: bool,         // reasoning/thinking support
    pub tool_call: bool,         // function/tool calling
    pub interleaved: Option<InterleavedSpec>,  // interleaved reasoning output
    pub structured_output: bool, // JSON mode / structured output
    pub temperature: bool,       // temperature parameter support
    pub knowledge: Option<String>,     // training data cutoff
    pub release_date: Option<String>,  // release date (ISO)
    pub last_updated: Option<String>,  // last updated (ISO)
    pub modalities: Modalities,  // input/output modality lists
    pub open_weights: bool,      // open-weight model
    pub cost: ModelCost,         // per-million-token pricing
    pub limit: ModelLimits,      // context window and output limits
}
```

### Capability Overrides

The `CapsOverrides` struct provides Oxydra-specific adjustments that models.dev cannot express (e.g. per-provider streaming support, deprecation status):

```rust
pub struct CapsOverrides {
    pub provider_defaults: BTreeMap<String, CapsOverrideEntry>,
    pub overrides: BTreeMap<String, CapsOverrideEntry>,
}
```

- `provider_defaults` — keyed by provider ID (e.g. `"openai"`)
- `overrides` — keyed by `"provider/model"` (e.g. `"anthropic/claude-3-5-sonnet-latest"`)

Each `CapsOverrideEntry` has optional fields that, when present, override the baseline. The `deprecated` field is tracked exclusively through the overlay.

### Catalog Provider Structure

The catalog is organized by provider namespace (`CatalogProvider`), each containing provider metadata and a model map:

```rust
pub struct CatalogProvider {
    pub id: String,
    pub name: String,
    pub env: Vec<String>,
    pub api: Option<String>,
    pub doc: Option<String>,
    pub models: BTreeMap<String, ModelDescriptor>,
}
```

Currently cataloged providers:

| Catalog Provider | Example Models |
|-----------------|----------------|
| `openai` | `gpt-4.1`, `gpt-4.1-mini`, `gpt-4.1-nano`, `gpt-4o`, `gpt-4o-mini`, `o3-mini` |
| `anthropic` | `claude-3-5-haiku-latest`, `claude-3-5-sonnet-latest` |
| `google` | Gemini models (included in caps overrides, populated by `catalog fetch`) |

Requests against unknown model IDs fail at validation time before any network call.

### Catalog Snapshot Commands

The runner provides CLI commands for managing the pinned catalog:

- **`catalog fetch`** — fetches the full catalog from `https://models.dev/api.json`, filters to configured providers (default: `openai`, `anthropic`, `google`), writes the canonical JSON snapshot and merges capability overrides
- **`catalog verify`** — re-fetches and compares against the committed snapshot, reporting drift
- **`catalog show`** — pretty-prints a summary of the current pinned catalog

The fetch pipeline (`runner/src/catalog.rs`) ensures deterministic output via `BTreeMap` ordering and trailing-newline convention. The `regenerate_snapshot` / `verify_snapshot_bytes` functions are available for CI integration.

## Provider Construction

Providers are constructed from registry entries via `build_provider()` in `provider/src/lib.rs`:

```rust
pub fn build_provider(
    provider_id: ProviderId,
    entry: &ProviderRegistryEntry,
    model_catalog: ModelCatalog,
) -> Result<Box<dyn Provider>, ProviderError>
```

This resolves the API key (4-tier resolution: explicit → custom env → provider-type env → generic `API_KEY` fallback), determines the base URL (entry-specified or type-default), and constructs the appropriate provider implementation based on `entry.provider_type`.

## OpenAI Provider

**File:** `provider/src/openai.rs`

### Wire Protocol

Targets `/v1/chat/completions`. The provider maps `Context` to `OpenAIChatCompletionRequest`:

- Messages are converted to OpenAI's `messages` array format
- Tools are encoded as `"type": "function"` declarations
- `tool_choice` is set to `"auto"` when tools are present
- For streaming, `stream: true` and `stream_options: { include_usage: true }` are set

### Response Normalization

`normalize_openai_response` extracts the first choice's message and maps it to the internal `Response` struct, including tool calls and usage data.

### Streaming

OpenAI streaming uses Server-Sent Events (SSE). The implementation:

1. Spawns an async task that holds the HTTP connection
2. Feeds raw bytes into `SseDataParser`, which handles:
   - Fragmented chunks across network boundaries
   - Line reassembly and `data:` prefix stripping
   - Event flushing on empty lines
   - The `[DONE]` sentinel
3. Parsed payloads are deserialized into `OpenAIChatCompletionStreamChunk`
4. Each chunk is mapped to `StreamItem` variants and sent through a bounded `mpsc` channel

### Tool Call Accumulation

Tool call arguments arrive as fragmented JSON strings across multiple SSE chunks. The `ToolCallAccumulator` maintains a `BTreeMap` indexed by tool call index, merging:
- Tool call ID (first chunk)
- Function name (first chunk)
- Argument string fragments (accumulated across chunks)

The accumulator emits `ToolCallDelta` items as fragments arrive, and the runtime's `ToolCallAccumulator` reassembles them into complete `ToolCall` objects.

### Error Handling

HTTP errors are parsed from OpenAI's JSON error envelope (`error.message` field). Specific HTTP status codes map to `ProviderError` variants:
- 401 → `Unauthorized`
- 429 → `RateLimited`
- 5xx → `HttpStatus` (retriable)

## Anthropic Provider

**File:** `provider/src/anthropic.rs`

### Wire Protocol

Targets `/v1/messages`. Key differences from OpenAI:

- System messages are extracted from the message array and joined into a single top-level `system` string field
- Tool results are encoded as `user` messages with `tool_result` content blocks
- Tool declarations use `input_schema` rather than `parameters`
- The response uses a `content` array of typed blocks (`text`, `tool_use`) rather than a flat `message` object

### Response Normalization

`normalize_anthropic_response` iterates through content blocks:
- `text` blocks → concatenated into message content
- `tool_use` blocks → mapped to `ToolCall` structs with `id`, `name`, and `arguments` (as JSON string)

### Streaming

**Current state: shim implementation.** The `stream()` method calls `complete()` internally and then emits the full response as a sequence of `StreamItem` events through the channel. Real SSE streaming for Anthropic's event stream API is not yet implemented. This means Anthropic responses arrive all at once rather than token-by-token.

### Credential Resolution

Uses the 4-tier credential resolution via `resolve_api_key_for_entry()`, with `ANTHROPIC_API_KEY` as the provider-type-specific default.

## Google Gemini Provider

**File:** `provider/src/gemini.rs`

### Wire Protocol

Uses the Gemini REST API at `https://generativelanguage.googleapis.com`. Two endpoints:

- **Non-streaming:** `POST /v1beta/models/{model}:generateContent`
- **Streaming:** `POST /v1beta/models/{model}:streamGenerateContent?alt=sse`

Authentication is via the `x-goog-api-key` header (not Bearer token).

### Request Mapping

`Context` is mapped to `GeminiGenerateContentRequest`:

- System messages are extracted into a separate `systemInstruction` content block
- User/assistant messages map to Gemini's `contents` array with `user`/`model` roles
- Tool call results are converted to `functionResponse` parts
- Tools are encoded as `functionDeclarations` within a `tools` array

### Response Normalization

`normalize_gemini_response` extracts the first candidate and iterates through its parts:
- `text` parts → concatenated into message content
- `functionCall` parts → mapped to `ToolCall` structs

Usage data is extracted from `usageMetadata` (prompt/candidates/total token counts).

### Streaming

The Gemini provider implements real SSE streaming. The streaming endpoint returns `data:` lines containing full `GenerateContentResponse` JSON objects. Each chunk is parsed via `parse_gemini_stream_payload` and mapped to `StreamItem` variants. Tool call arguments arrive as complete JSON in a single chunk (no fragmented accumulation needed).

### Error Handling

HTTP errors are parsed from Gemini's JSON error envelope (`error.message` field). The same status code mapping applies (401, 429, 5xx).

### Credential Resolution

Uses `GEMINI_API_KEY` as the provider-type-specific default environment variable.

## OpenAI Responses Provider

**File:** `provider/src/responses.rs`

### Wire Protocol

Targets `/v1/responses`, OpenAI's stateful conversation API. Key differences from the Chat Completions API:

- **Session chaining:** Tracks `previous_response_id` in an `Arc<Mutex<Option<String>>>`. After the first turn, subsequent requests include the previous response ID instead of re-sending the full message history. This persists conversation state server-side, reducing token transmission overhead.
- **Input format:** Uses an `input` array of typed items (`message`, `function_call_output`) rather than the chat completions `messages` format.
- **Output format:** Response contains typed `output` items (`message`, `function_call`) with structured content blocks.
- **Fallback:** If a chained request fails, the provider falls back to sending the full context (resets `previous_response_id`).

### Response Normalization

The `ResponsesProvider` parses `ResponsesApiResponse` with typed output blocks:
- `message` output items with `output_text` content → concatenated into message content
- `function_call` output items → mapped to `ToolCall` structs with `call_id`, `name`, and `arguments`

### Streaming

Real SSE streaming with Responses API-specific event types:

- `response.output_item.added` — new output block started
- `response.content_part.added` — content part started
- `response.output_text.delta` — text fragment
- `response.function_call_arguments.delta` — tool call argument fragment
- `response.completed` — final response with usage data and response ID

Tool call arguments are accumulated via `ResponsesToolCallAccumulator`, which tracks active function calls by output index and merges argument fragments.

### Catalog Provider

The `ResponsesProvider` overrides `catalog_provider_id()` to return `"openai"`, since it shares the same model catalog namespace as the Chat Completions provider despite having a different instance ID.

### Credential Resolution

Uses `OPENAI_API_KEY` as the provider-type-specific default (same as the Chat Completions provider).

## Reliability Layer

**File:** `provider/src/retry.rs`

The `ReliableProvider` wraps any `Provider` implementation with retry and backoff logic.

### Retry Policy

Only transient failures are retried:
- `ProviderError::Transport` — network/timeout issues
- `ProviderError::HttpStatus` for status codes 429 (rate limit) and 5xx (server error)

Non-retriable errors (401 Unauthorized, 400 Bad Request, parse errors) are surfaced immediately.

### Backoff Calculation

```
delay = min(base_ms * 2^(attempt - 1), max_ms)
```

Default: base 1000ms, max 30000ms, 3 attempts.

### Stream Reconnection

For streaming requests, if the stream connection is lost (`StreamItem::ConnectionLost`), the `ReliableProvider` enters a retry loop that re-establishes the stream from scratch. Each reconnection attempt follows the same backoff policy.

## Request/Response Flow

```
Runtime                Provider               LLM API
  │                      │                      │
  ├─ Context ──────────► │                      │
  │                      ├─ serialize ─────────►│
  │                      │    (wire format)     │
  │                      │◄── SSE chunks ───────┤
  │                      ├─ parse/accumulate    │
  │  ◄─ StreamItem ──── │                      │
  │  ◄─ StreamItem ──── │                      │
  │  ◄─ StreamItem ──── │                      │
  │                      │                      │
```

## Snapshot Testing

Provider request serialization is verified using `insta` snapshot tests. These ensure that changes to the serialization logic don't silently alter the wire format sent to providers. Snapshots cover:
- OpenAI request with tools
- Anthropic request with system messages and tool results
- Gemini request with tools and system instructions
- Responses API request with session chaining
- Streaming chunk parsing for all provider types
- Error envelope parsing
