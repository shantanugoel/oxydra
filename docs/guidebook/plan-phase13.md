# Phase 13: Model Catalog Governance + Provider Flexibility — Implementation Plan

## Overview

Phase 13 replaces the manually maintained model catalog with one sourced from [models.dev/api.json](https://models.dev/api.json), introduces a named provider registry decoupled from hardcoded provider types, adds a Google Gemini provider, and implements the OpenAI Responses API provider (Gap 5). Every change extends prior phases without rewrites.

**Crates touched:** `types`, `provider`, `runner`, `tui`
**Builds on:** Phases 1, 2, 3, 7, 12

---

## Step 1 — Align ModelDescriptor with models.dev schema

**Goal:** Make `ModelDescriptor` a faithful representation of models.dev per-model objects so the catalog is directly derivable from upstream data.

### What changes

**In `crates/types/src/model.rs`:**

Replace the current `ModelDescriptor` with fields matching the models.dev per-model schema:

```rust
pub struct ModelDescriptor {
    pub id: String,                              // e.g. "gpt-4o", "claude-3-5-sonnet-latest"
    pub name: String,                            // human-readable display name
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,                  // e.g. "gpt-4o", "claude"
    #[serde(default)]
    pub attachment: bool,                         // supports file/image attachments
    #[serde(default)]
    pub reasoning: bool,                          // supports reasoning/thinking
    #[serde(default)]
    pub tool_call: bool,                          // supports function/tool calling
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interleaved: Option<InterleavedSpec>,      // interleaved reasoning output config
    #[serde(default)]
    pub structured_output: bool,                  // supports JSON mode / structured output
    #[serde(default)]
    pub temperature: bool,                        // supports temperature parameter
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub knowledge: Option<String>,                // training data cutoff, e.g. "2024-04"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_date: Option<String>,             // ISO date
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_updated: Option<String>,             // ISO date
    #[serde(default)]
    pub modalities: Modalities,                   // input/output modality lists
    #[serde(default)]
    pub open_weights: bool,                       // whether model weights are open
    #[serde(default)]
    pub cost: ModelCost,                          // per-million-token pricing
    #[serde(default)]
    pub limit: ModelLimits,                        // context/output token limits
}

pub struct InterleavedSpec {
    pub field: String,                            // e.g. "reasoning_content"
}

pub struct Modalities {
    #[serde(default)]
    pub input: Vec<String>,                       // e.g. ["text", "image"]
    #[serde(default)]
    pub output: Vec<String>,                      // e.g. ["text"]
}

pub struct ModelCost {
    #[serde(default)]
    pub input: f64,                               // USD per million input tokens
    #[serde(default)]
    pub output: f64,                              // USD per million output tokens
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
}

pub struct ModelLimits {
    #[serde(default)]
    pub context: u32,                             // max context window tokens
    #[serde(default)]
    pub output: u32,                              // max output tokens
}
```

**Catalog top-level structure** — mirrors models.dev's provider-keyed layout:

```rust
pub struct CatalogProvider {
    pub id: String,                               // e.g. "openai", "anthropic", "google"
    pub name: String,                             // human-readable, e.g. "OpenAI"
    #[serde(default)]
    pub env: Vec<String>,                         // default env vars for API key, e.g. ["OPENAI_API_KEY"]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,                      // default base URL, e.g. "https://api.openai.com/v1"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,                      // documentation URL
    pub models: BTreeMap<String, ModelDescriptor>, // model_id → descriptor
}

pub struct ModelCatalog {
    pub providers: BTreeMap<String, CatalogProvider>,  // provider_id → provider spec
}
```

**Update `ModelCatalog` methods:**

- `get(catalog_provider_id, model_id)` → look up `providers[catalog_provider_id].models[model_id]`
- `validate(catalog_provider_id, model_id)` → same, returning error on miss
- `from_pinned_snapshot()` → deserialize from the new JSON format
- `all_models()` → iterator over `(catalog_provider_id, model_id, &ModelDescriptor)` for convenience

**Pinned snapshot format** — `crates/types/data/pinned_model_catalog.json` changes from the flat `{ "models": [...] }` array to a provider-keyed map matching models.dev's shape (filtered to relevant providers only):

```json
{
  "openai": {
    "id": "openai",
    "name": "OpenAI",
    "env": ["OPENAI_API_KEY"],
    "api": "https://api.openai.com/v1",
    "models": {
      "gpt-4o": { "id": "gpt-4o", "name": "GPT-4o", ... },
      "gpt-4o-mini": { ... }
    }
  },
  "anthropic": { ... },
  "google": { ... }
}
```

### Verification gates

- `models_dev_json_snippet_deserializes` — a recorded subset of models.dev JSON parses into the new structs
- `catalog_serialization_round_trip_is_deterministic` — serialize → deserialize → serialize produces identical output (BTreeMap ordering)
- All existing tests updated for the new schema

---

## Step 2 — ProviderCaps derivation from models.dev fields + Oxydra overlay

**Goal:** Keep `ProviderCaps` as the runtime capability struct used by the agent loop, but derive it from models.dev fields with a small Oxydra-specific override layer for implementation-specific gaps.

### Derivation mapping

| `ProviderCaps` field | Derived from models.dev field | Notes |
|---|---|---|
| `supports_streaming` | **Oxydra overlay default by provider type** | openai: true, gemini: true, anthropic: true (once real streaming is done). Cannot be inferred from models.dev — it's an implementation detail. |
| `supports_tools` | `model.tool_call` | Direct mapping |
| `supports_json_mode` | `model.structured_output` | Direct mapping |
| `supports_reasoning_traces` | `model.reasoning` or `model.interleaved.is_some()` | True if model supports reasoning output |
| `max_context_tokens` | `model.limit.context` | Direct mapping |
| `max_output_tokens` | `model.limit.output` | Direct mapping |
| `max_input_tokens` | `model.limit.context` (same as context for most models) | Can override via overlay if needed |

### Oxydra overlay file

A separate file `crates/types/data/oxydra_caps_overrides.json` holds per-(provider, model) capability overrides that models.dev cannot express:

```json
{
  "overrides": {
    "anthropic/claude-3-5-sonnet-latest": {
      "supports_streaming": true
    }
  },
  "provider_defaults": {
    "openai": { "supports_streaming": true },
    "anthropic": { "supports_streaming": true },
    "google": { "supports_streaming": true }
  }
}
```

### Implementation

- Add `derive_caps(catalog_provider_id: &str, model: &ModelDescriptor, overrides: &CapsOverrides) -> ProviderCaps` function in `types::model`
- `ModelCatalog` gains a `caps_overrides: CapsOverrides` field loaded alongside the pinned snapshot
- The `capabilities()` method on `Provider` trait implementations calls `derive_caps()` instead of reading pre-baked caps from the descriptor

### Verification gates

- `caps_derived_from_model_descriptor` — tool_call=true model yields supports_tools=true
- `caps_overlay_applies` — anthropic model with overlay `supports_streaming=true` overrides the default
- `caps_default_by_provider` — provider-level streaming defaults apply when no model override exists

---

## Step 3 — Provider registry config types + backward-compatible evolution

**Goal:** Transition from hardcoded `providers.openai` / `providers.anthropic` blocks to a named provider registry. Existing configs continue to work unchanged.

### New config types in `crates/types/src/config.rs`

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderType {
    Openai,
    Anthropic,
    Gemini,
    OpenaiResponses,
}

pub struct ProviderRegistryEntry {
    pub provider_type: ProviderType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_headers: Option<BTreeMap<String, String>>,
    /// Which catalog provider namespace to validate models against.
    /// Defaults by provider_type: openai→"openai", anthropic→"anthropic",
    /// gemini→"google", openai_responses→"openai"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_provider: Option<String>,
}
```

**Extend `ProviderConfigs`:**

```rust
pub struct ProviderConfigs {
    #[serde(default)]
    pub openai: OpenAIProviderConfig,        // legacy — kept for backward compat
    #[serde(default)]
    pub anthropic: AnthropicProviderConfig,   // legacy — kept for backward compat
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub registry: BTreeMap<String, ProviderRegistryEntry>,
}
```

### Resolution rules

Add `ProviderConfigs::resolve(provider_name: &str) -> Result<ResolvedProvider, ConfigError>`:

1. If `registry` contains `provider_name` → return it directly
2. If `provider_name == "openai"` → synthesize `ProviderRegistryEntry` from legacy `openai` block (provider_type=Openai, base_url from legacy, api_key from legacy, api_key_env="OPENAI_API_KEY", catalog_provider="openai")
3. If `provider_name == "anthropic"` → same synthesis from `anthropic` block
4. Otherwise → `ConfigError::UnsupportedProvider`

`catalog_provider` defaults per type: `Openai`→`"openai"`, `Anthropic`→`"anthropic"`, `Gemini`→`"google"`, `OpenaiResponses`→`"openai"`.

### Config validation changes

Update `AgentConfig::validate()`:
- Remove the hardcoded `OPENAI_PROVIDER_ID | ANTHROPIC_PROVIDER_ID` match
- Instead: call `self.providers.resolve(&self.selection.provider.0)?` — success means valid provider
- Validate that the selected model exists in the catalog under the resolved entry's `catalog_provider`

### Example TOML (new-style)

```toml
[selection]
provider = "my-openai-proxy"
model = "gpt-4o"

[providers.registry.my-openai-proxy]
provider_type = "openai"
base_url = "https://my-proxy.corp.internal/v1"
api_key_env = "CORP_OPENAI_KEY"

[providers.registry.gemini]
provider_type = "gemini"
api_key_env = "GEMINI_API_KEY"
```

### Verification gates

- `legacy_openai_config_resolves` — default config with `provider = "openai"` still works
- `legacy_anthropic_config_resolves` — same for anthropic
- `registry_entry_overrides_legacy` — registry entry named "openai" takes precedence over legacy block
- `custom_provider_resolves_from_registry` — a custom name like "my-proxy" resolves correctly
- `unknown_provider_rejected` — error on unresolvable provider name
- `model_validated_against_catalog_provider` — selection.model checked against resolved catalog_provider

---

## Step 4 — Provider factory + generic API key resolution

**Goal:** Replace the hardcoded `match` in `tui/bootstrap.rs` with a factory that constructs providers from resolved registry entries.

### New factory in `crates/provider/src/lib.rs`

```rust
pub fn resolve_api_key_for_entry(entry: &ProviderRegistryEntry) -> Option<String> {
    // 1. Explicit api_key in entry
    // 2. entry.api_key_env env var (if set)
    // 3. Default env var by provider_type (OPENAI_API_KEY, ANTHROPIC_API_KEY, GEMINI_API_KEY)
    // 4. Fallback: API_KEY
}

pub fn build_provider(
    instance_id: ProviderId,
    entry: &ProviderRegistryEntry,
    catalog: ModelCatalog,
) -> Result<Box<dyn Provider>, ProviderError> {
    let api_key = resolve_api_key_for_entry(entry)
        .ok_or_else(|| ProviderError::MissingApiKey { provider: instance_id.clone() })?;
    let catalog_provider = entry.effective_catalog_provider();
    let base_url = entry.effective_base_url();

    match entry.provider_type {
        ProviderType::Openai => { /* construct OpenAIProvider */ },
        ProviderType::Anthropic => { /* construct AnthropicProvider */ },
        ProviderType::Gemini => { /* construct GeminiProvider */ },
        ProviderType::OpenaiResponses => { /* construct ResponsesProvider */ },
    }
}
```

### Update provider constructors

All providers gain a generalized constructor that accepts:
- `provider_id: ProviderId` — the instance ID (e.g. "my-openai-proxy")
- `catalog_provider_id: ProviderId` — the catalog namespace for model validation (e.g. "openai")
- `api_key: String`
- `base_url: String`
- `extra_headers: Option<BTreeMap<String, String>>`
- `catalog: ModelCatalog`

This replaces the current pattern where each provider loads `ModelCatalog::from_pinned_snapshot()` internally.

### Update `tui/bootstrap.rs`

Replace:
```rust
match config.selection.provider.0.as_str() {
    OPENAI_PROVIDER_ID => Box::new(OpenAIProvider::new(...)?),
    ANTHROPIC_PROVIDER_ID => Box::new(AnthropicProvider::new(...)?),
    ...
}
```

With:
```rust
let entry = config.providers.resolve(&config.selection.provider.0)?;
let catalog = ModelCatalog::from_pinned_snapshot()?;
let inner = build_provider(config.selection.provider.clone(), &entry, catalog)?;
```

### Verification gates

- `factory_constructs_openai_from_registry_entry` — round-trip with mock key
- `factory_constructs_anthropic_from_registry_entry`
- `api_key_resolved_from_entry_env_var` — env var specified in entry is used
- `api_key_falls_back_to_provider_type_default` — OPENAI_API_KEY used if entry.api_key_env is None
- `extra_headers_passed_through` — headers from entry propagated to provider

---

## Step 5 — Google Gemini provider

**Goal:** Implement a native Gemini provider using Google's REST API, with real SSE streaming.

### Wire protocol

**Non-streaming endpoint:**
```
POST {base_url}/v1beta/models/{model}:generateContent
```

**Streaming endpoint:**
```
POST {base_url}/v1beta/models/{model}:streamGenerateContent?alt=sse
```

**Authentication:**
```
x-goog-api-key: {api_key}
```

**Default base URL:** `https://generativelanguage.googleapis.com`

**Default api_key_env:** `GEMINI_API_KEY`

### Request mapping (Context → Gemini request)

```rust
struct GeminiGenerateContentRequest {
    contents: Vec<GeminiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system_instruction: Option<GeminiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<GeminiToolDeclaration>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    generation_config: Option<GeminiGenerationConfig>,
}

struct GeminiContent {
    role: String,         // "user" or "model"
    parts: Vec<GeminiPart>,
}

struct GeminiPart {
    // One of:
    text: Option<String>,
    function_call: Option<GeminiFunctionCall>,
    function_response: Option<GeminiFunctionResponse>,
}
```

**Message role mapping:**
- `System` → extracted into `system_instruction` (as a `GeminiContent` with parts)
- `User` → `role: "user"`
- `Assistant` → `role: "model"`
- `Tool` (tool result) → `role: "user"` with `function_response` part

**Tool declaration mapping:**
- `FunctionDecl` → `GeminiToolDeclaration` wrapping `function_declarations[]` with `name`, `description`, `parameters` (JSON Schema)

### Response mapping

**Non-streaming response:**
```rust
struct GeminiGenerateContentResponse {
    candidates: Vec<GeminiCandidate>,
    usage_metadata: Option<GeminiUsageMetadata>,
}

struct GeminiCandidate {
    content: GeminiContent,
    finish_reason: Option<String>,  // "STOP", "MAX_TOKENS", etc.
}
```

- `candidates[0].content.parts[].text` → concatenated into `Response.message.content`
- `candidates[0].content.parts[].function_call` → mapped to `ToolCall { id: generated_uuid, name, arguments: args_to_json_string }`
- `finish_reason` → `Response.finish_reason`
- `usage_metadata.prompt_token_count` → `UsageUpdate.prompt_tokens`
- `usage_metadata.candidates_token_count` → `UsageUpdate.completion_tokens`

**Streaming response (SSE):**

Gemini streaming sends SSE events where each `data:` payload is a complete `GeminiGenerateContentResponse` JSON object (one candidate chunk per event). Reuse the existing `SseDataParser`.

- Each chunk's `candidates[0].content.parts[].text` → `StreamItem::Text`
- Each chunk's `candidates[0].content.parts[].function_call` → `StreamItem::ToolCallDelta` (Gemini sends function calls atomically, not fragmented; emit a single complete delta)
- `finish_reason` in a chunk → `StreamItem::FinishReason`
- `usage_metadata` → `StreamItem::UsageUpdate`

**Tool call ID generation:** Gemini does not provide tool call IDs in its response. Generate a UUID for each `function_call` part to satisfy the `ToolCall.id` requirement.

### Error handling

- Map HTTP 401 → `ProviderError::MissingApiKey` or `Unauthorized`
- Map HTTP 429 → `ProviderError::HttpStatus` (retriable)
- Map HTTP 5xx → `ProviderError::HttpStatus` (retriable)
- Parse error JSON envelope: `{ "error": { "code": N, "message": "...", "status": "..." } }`

### File: `crates/provider/src/gemini.rs`

### Verification gates

- `gemini_request_serialization_snapshot` — insta snapshot of a request with system msg + tools
- `gemini_response_normalization` — recorded response maps correctly to `Response`
- `gemini_stream_chunk_parsing` — recorded SSE stream maps to correct `StreamItem` sequence
- `gemini_function_call_response_mapping` — tool call round-trip including generated IDs
- `gemini_tool_result_encoding` — `Tool` role messages map to `function_response` parts

---

## Step 6 — OpenAI Responses API provider (Gap 5)

**Goal:** Implement stateful `/v1/responses` provider with `previous_response_id` chaining and dedicated SSE event parsing.

### Wire protocol

**Endpoint:**
```
POST {base_url}/v1/responses
```

**Authentication:** `Authorization: Bearer {api_key}`

**Default base URL:** `https://api.openai.com` (same as OpenAI chat completions)

**Provider type key:** `openai_responses` (in ProviderType enum)

**Catalog provider:** `"openai"` (uses the same model namespace as OpenAIProvider)

### Request format

```rust
struct ResponsesApiRequest {
    model: String,
    input: Vec<ResponsesInputItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_response_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,       // system prompt
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ResponsesToolDeclaration>,
    store: bool,                         // must be true for chaining to work
    stream: bool,
}

// Input item types (tagged enum):
enum ResponsesInputItem {
    Message { role: String, content: String },
    FunctionCallOutput { call_id: String, output: String },
}

struct ResponsesToolDeclaration {
    r#type: String,                     // "function"
    name: String,
    description: String,
    parameters: serde_json::Value,      // JSON Schema
    strict: Option<bool>,
}
```

**Context → Request mapping:**
- `System` messages → `instructions` field (concatenated)
- `User`/`Assistant` messages → `input` items with appropriate role
- `Tool` result messages → `FunctionCallOutput` items with `call_id` and output JSON
- `Context.tools` → `tools[]` as function declarations

**Chaining behavior:**
- Provider holds `previous_response_id: Arc<Mutex<Option<String>>>`
- On first turn: `previous_response_id` is None, full input is sent
- On subsequent turns: `previous_response_id` is set, only new messages since the last response are sent in `input`
- Track `last_input_message_count: Arc<Mutex<usize>>` to know which messages are "new"
- On desync (4xx indicating invalid/expired previous_response_id): clear state and resend full history

### SSE streaming

**Important:** The Responses API SSE uses `event:` lines (not just `data:` lines). The current `SseDataParser` only handles `data:` lines. We need to extend it (or create a new variant) to capture the `event:` type.

Extended SSE parser behavior:
- Capture `event:` line as the event type
- Capture `data:` line(s) as the payload
- On blank line: flush the event with `(event_type, data_payload)`

**SSE event mapping:**

| SSE Event Type | Action |
|---|---|
| `response.output_text.delta` | Parse delta text → `StreamItem::Text` |
| `response.function_call_arguments.delta` | Parse argument fragment → `StreamItem::ToolCallDelta` |
| `response.output_item.added` | Track new output items (tool calls get IDs here) |
| `response.completed` | Extract `response.id` → update `previous_response_id`; extract usage → `StreamItem::UsageUpdate`; emit `StreamItem::FinishReason` |
| `response.failed` | Emit error |
| `response.in_progress`, `response.created` | Informational; ignore or log |

### Provider state

```rust
pub struct ResponsesProvider {
    client: Client,
    provider_id: ProviderId,
    catalog_provider_id: ProviderId,
    model_catalog: ModelCatalog,
    base_url: String,
    api_key: String,
    previous_response_id: Arc<Mutex<Option<String>>>,
    last_input_count: Arc<Mutex<usize>>,
}
```

- `provider_id()` returns the instance ID (could be "openai-responses" or custom name)
- Model validation uses `catalog_provider_id` ("openai") to look up in catalog
- `previous_response_id` cleared on session reset or on desync error

### File: `crates/provider/src/responses.rs`

### Verification gates

- `responses_request_serialization_snapshot` — full request with tools, system, and previous_response_id
- `responses_sse_event_parsing` — recorded SSE stream with event types maps correctly
- `previous_response_id_updated_on_completion` — state updated after response.completed
- `partial_input_on_chained_turn` — only new messages sent when previous_response_id is set
- `desync_fallback_resets_and_retries` — 4xx on previous_response_id clears state and resends full history
- `responses_provider_uses_openai_catalog` — model validation uses "openai" catalog namespace

---

## Step 7 — Snapshot governance: fetch, filter, merge, pin, verify

**Goal:** Provide a complete operator workflow for regenerating the pinned catalog from models.dev and verifying it hasn't drifted.

### CLI subcommands

Add to the runner binary (or a new dedicated binary — runner is simplest):

**`oxydra catalog fetch`** (or a library function called by it):
1. `GET https://models.dev/api.json`
2. Parse the full JSON into `ModelsDevFullCatalog` (the top-level structure with all providers)
3. Filter to configured provider set (default: `openai`, `anthropic`, `google`)
4. Optionally filter models to a curated allowlist (configurable; default: include all for selected providers)
5. Map each provider's models to `CatalogProvider { id, name, env, api, models }` using the models.dev schema
6. Merge with Oxydra caps overrides (from `oxydra_caps_overrides.json`)
7. Canonicalize: BTreeMap ordering, deterministic JSON pretty-print
8. Write to `crates/types/data/pinned_model_catalog.json`
9. Write overlay to `crates/types/data/oxydra_caps_overrides.json` (preserving existing overrides, adding missing model keys with defaults)

**`oxydra catalog verify`**:
1. Run the same fetch + filter + canonicalize pipeline into a temporary buffer
2. Byte-compare with the committed `pinned_model_catalog.json`
3. Exit 0 if identical, exit 1 with diff summary if not

**`oxydra catalog show`** (optional convenience):
- Pretty-print the current pinned catalog with model counts per provider

### CI integration

Add a CI step:
```yaml
- name: Verify model catalog snapshot
  run: cargo run -p runner -- catalog verify
```

This fails the build if someone modifies models.dev data upstream and the pinned snapshot hasn't been regenerated.

### Offline operation

The runtime **never** fetches models.dev at startup. It always uses the committed pinned snapshot (loaded via `include_str!`). The fetch is an explicit operator action.

### Verification gates

- `regenerate_snapshot_is_deterministic` — run regen twice (from same source data), outputs byte-identical
- `verify_detects_drift` — modify one field in pinned JSON, verify exits non-zero
- `filter_excludes_non_configured_providers` — only openai/anthropic/google in output
- `overlay_file_preserves_existing_overrides` — regen doesn't clobber manually added overrides

---

## Step 8 — Startup validation + deprecation warnings + wiring

**Goal:** Wire everything together with proper startup validation, deprecation detection, and consistent provider/model mapping.

### Startup validation changes

In `tui/bootstrap.rs`:
1. Load config via figment (unchanged)
2. Resolve provider via `config.providers.resolve(selection.provider)` → `ResolvedProvider`
3. Load catalog via `ModelCatalog::from_pinned_snapshot()`
4. Validate model exists: `catalog.validate(resolved.catalog_provider, selection.model)`
5. Check for deprecation: if model descriptor in models.dev data has indicators of being deprecated (e.g., `last_updated` is very old, or if we add a `deprecated` override in the overlay), emit a `tracing::warn!`
6. Build provider via factory: `build_provider(instance_id, entry, catalog)`
7. Wrap in `ReliableProvider`

### Deprecation detection

Since models.dev doesn't have a universal "deprecated" field, we handle deprecation via:
- An explicit `deprecated: bool` field in the Oxydra overlay per model
- At startup: if the resolved model has `deprecated = true` in overlay, log a warning with the model ID

### Provider ID consistency

- `ResponsesProvider.provider_id()` returns the instance ID (e.g., "openai-responses" or whatever the registry key is)
- Model validation uses the `catalog_provider` from the registry entry (defaults to "openai")
- This means the same OpenAI model catalog serves both `OpenAIProvider` and `ResponsesProvider`

### Verification gates

- `deprecated_model_emits_warning` — tracing subscriber captures the warning
- `provider_type_model_mismatch_rejected` — selecting a Gemini model with an OpenAI provider fails validation
- `openai_responses_provider_uses_openai_catalog` — ResponsesProvider validates against "openai" models
- `full_bootstrap_with_registry_config` — end-to-end config loading with registry provider

---

## Dependency graph

```
Step 1 (ModelDescriptor schema)
  ├── Step 2 (ProviderCaps derivation)
  ├── Step 7 (Snapshot governance — needs schema)
  └── Step 3 (Provider registry config)
        └── Step 4 (Provider factory)
              ├── Step 5 (Gemini provider)
              └── Step 6 (Responses provider)
Step 8 depends on all of 1–7
```

Steps 5 and 6 can be implemented in parallel once Step 4 is complete.
Step 7 can be implemented in parallel with Steps 3–6 once Step 1 is complete.

---

## Migration notes

### Breaking changes

- `pinned_model_catalog.json` format changes from flat array to provider-keyed map. This is a build-time-only artifact; no runtime migration needed.
- `ModelDescriptor` struct changes. All code that accesses `ModelDescriptor.caps` directly must switch to `derive_caps()` or use the provider's `capabilities()` method.
- `AgentConfig::validate()` relaxes the provider check from hardcoded strings to registry resolution. This is backward-compatible (old configs still work).

### What stays the same

- `Provider` trait — unchanged
- `StreamItem` enum — unchanged
- `ReliableProvider` — unchanged
- `ProviderStream` type alias — unchanged
- Runtime agent loop — unchanged (it only uses `Provider` trait + `ProviderCaps`)
- `SseDataParser` — extended (new event-aware mode), existing mode unchanged

---

## Risks and mitigations

| Risk | Mitigation |
|---|---|
| models.dev schema changes | Pin to a snapshot; runtime never fetches; regen is explicit operator action |
| Responses API `previous_response_id` unreliable (known issue) | Implement desync detection + fallback to full history resend |
| Gemini tool calling edge cases | Implement text-only streaming first; tool support second; set `supports_tools` via overlay |
| Backward-compat config breakage | Legacy flat fields always synthesized as fallback; registry is purely additive |
| Large catalog snapshot size | Filter to relevant providers only (openai, anthropic, google) at regen time |
| SSE event-type parsing for Responses API | Extend `SseDataParser` to capture `event:` lines; test against recorded payloads |
