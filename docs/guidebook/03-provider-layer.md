# Chapter 3: Provider Layer

## Overview

The provider layer normalizes disparate LLM APIs into a unified internal representation. Every provider — regardless of wire format — produces the same `Response` and `StreamItem` types consumed by the runtime. This allows the agent loop to be entirely provider-agnostic.

## The Provider Trait

Defined in `types/src/provider.rs`:

```rust
#[async_trait]
pub trait Provider: Send + Sync {
    fn provider_id(&self) -> &ProviderId;
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
- `capabilities` — returns `ProviderCaps` for a given model, looked up from the embedded catalog

`ProviderStream` is a type alias for `tokio::sync::mpsc::Receiver<StreamItem>`.

## Provider Capabilities

```rust
pub struct ProviderCaps {
    pub supports_streaming: bool,
    pub supports_tools: bool,
    pub supports_json_mode: bool,
    pub supports_reasoning_traces: bool,
    pub max_input_tokens: Option<u64>,
    pub max_output_tokens: Option<u64>,
}
```

Capabilities are looked up from the model catalog at request time. The runtime uses these to decide whether to call `stream()` or fall back to `complete()`, and whether to include tool declarations in the context.

## Model Catalog

The `ModelCatalog` is a registry of `ModelDescriptor` entries embedded into the binary at build time from `types/data/pinned_model_catalog.json`. This pinned snapshot ensures deterministic behavior without network access at startup.

Currently cataloged models:

| Provider | Models |
|----------|--------|
| Anthropic | `claude-3-5-haiku-latest`, `claude-3-5-sonnet-latest` |
| OpenAI | `gpt-4.1`, `gpt-4.1-mini`, `gpt-4.1-nano`, `gpt-4o`, `gpt-4o-mini`, `o3-mini` |

Each descriptor includes capabilities, token limits, and deprecation status. Requests against unknown model IDs fail at validation time before any network call.

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

Uses `ANTHROPIC_API_KEY` as the provider-specific environment variable, falling back to `API_KEY`.

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
- Streaming chunk parsing
- Error envelope parsing

## Planned: OpenAI Responses API

The project brief describes a `ResponsesProvider` targeting OpenAI's stateful `/v1/responses` endpoint, which persists conversation state server-side via `previous_response_id` chaining. This reduces token transmission overhead for long sessions and is well-suited for multi-agent subagent delegation (clean context isolation without duplicating memory payloads).

This provider is not yet implemented. The current `OpenAIProvider` uses the stateless `/v1/chat/completions` endpoint exclusively, re-sending the full conversation history on every turn. The rolling summarization in the memory layer partially mitigates the token overhead for long sessions.

The `Provider` trait, `StreamItem` enum, and `ReliableProvider` wrapper are all designed to support a Responses API implementation without modification.
