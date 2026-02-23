# Oxydra Tools Issues — Root Cause Analysis & Fix Plan

We found a bunch of issues around tools in oxydra, compared them to picobot (source code at: ~/dev/oxydra-home/picobot) and rig (source code at: ~/dev/oxydra-home/rig) which dont have any of these issues, and found root causes. Now we are trying to validate that this is the right root cause and right way to fix. Ask the user if there are any doubts

## Table of Contents

1. [Issue 1: Streaming Tool Call Arguments Are Malformed (Double Accumulation)](#issue-1-streaming-tool-call-arguments-are-malformed-double-accumulation)
2. [Issue 2: Tool Call Validation Failures (Missing/Additional Parameters)](#issue-2-tool-call-validation-failures-missingadditional-parameters)
3. [Issue 3: Internal Configuration Leaked to LLM via Tool Schemas](#issue-3-internal-configuration-leaked-to-llm-via-tool-schemas)
4. [Issue 4: Responses API Ordinal Assignment Bug](#issue-4-responses-api-ordinal-assignment-bug)
5. [Issue 5: `response.completed` Event Emits All Tool Calls With index: 0](#issue-5-responsecompleted-event-emits-all-tool-calls-with-index-0)
6. [Issue 6: JsonSchema Lacks Constraint Support (enum, min/max, etc.)](#issue-6-jsonschema-lacks-constraint-support-enum-minmax-etc)

---

## Issue 1: Streaming Tool Call Arguments Are Malformed (Double Accumulation) - RESOLVED

### Symptoms

When streaming is enabled, tool call arguments arrive as malformed JSON — fragments
appear to repeat/nest inside each other, producing unparseable payloads like:

```
{"ur{"url":"htt{"url":"https://example.com"}
```

Text (non-tool-call) streaming works correctly. Disabling streaming makes the
problem disappear because the `complete()` path bypasses both accumulator layers.

### Root Cause

There are **two independent accumulator layers** that both accumulate tool call
arguments, but they disagree on whether `ToolCallDelta.arguments` carries a
**fragment** (just the new piece) or the **full accumulated string so far**.

#### Layer 1 — Provider-level accumulators

Each provider maintains an accumulator that appends incoming SSE fragments and
then emits a `ToolCallDelta` with the **full accumulated arguments**:

**`crates/provider/src/openai.rs` — `ToolCallAccumulator::merge()` (lines 702–727):**
```rust
fn merge(&mut self, index: usize, id: Option<String>,
         function: OpenAIStreamFunctionDelta) -> ToolCallDelta {
    let entry = self.by_index.entry(index).or_default();
    // ...
    if let Some(arguments) = function.arguments {
        entry.arguments.push_str(&arguments);       // appends the new fragment
    }
    ToolCallDelta {
        // ...
        arguments: (!entry.arguments.is_empty())
            .then(|| entry.arguments.clone()),       // ← emits FULL accumulated string
    }
}
```

**`crates/provider/src/anthropic.rs` — `AnthropicToolCallAccumulator::append_json()`
(lines 720–739):**
```rust
fn append_json(&mut self, block_index: usize, partial_json: &str) -> ToolCallDelta {
    // ...
    let entry = self.by_block_index.entry(block_index).or_default();
    entry.arguments.push_str(partial_json);          // appends the new fragment
    ToolCallDelta {
        // ...
        arguments: (!entry.arguments.is_empty())
            .then(|| entry.arguments.clone()),       // ← emits FULL accumulated string
    }
}
```

**`crates/provider/src/responses.rs` — `ResponsesToolCallAccumulator::append_arguments()`
(lines 715–734):**
```rust
fn append_arguments(&mut self, output_index: u32, delta: &str) -> Option<StreamItem> {
    // ...
    let entry = self.output_index_to_call.entry(output_index).or_default();
    entry.arguments.push_str(delta);                 // appends the new fragment
    Some(StreamItem::ToolCallDelta(ToolCallDelta {
        // ...
        arguments: if entry.arguments.is_empty() {
            None
        } else {
            Some(entry.arguments.clone())            // ← emits FULL accumulated string
        },
    }))
}
```

#### Layer 2 — Runtime-level accumulator

The runtime's `ToolCallAccumulator` in `crates/runtime/src/lib.rs` (lines 358–370)
treats `delta.arguments` as a **fragment to append**:

```rust
fn merge(&mut self, delta: ToolCallDelta) {
    let entry = self.by_index.entry(delta.index).or_default();
    // ...
    if let Some(arguments) = delta.arguments {
        entry.arguments.push_str(&arguments);    // appends what it receives
    }
}
```

#### The Collision

When the provider emits 3 SSE chunks with argument fragments `{"ur`, `l":"htt`,
`ps://example.com"}`:

| SSE chunk | Provider accumulates | Provider emits `delta.arguments` | Runtime appends |
|-----------|---------------------|----------------------------------|-----------------|
| 1 | `{"ur` | `{"ur` | `{"ur` |
| 2 | `{"url":"htt` | `{"url":"htt` | `{"ur{"url":"htt` |
| 3 | `{"url":"https://example.com"}` | `{"url":"https://example.com"}` | `{"ur{"url":"htt{"url":"https://example.com"}` |

The runtime ends up with **garbled, repeated JSON** that fails to parse.

### How rig-core Does It (No Bug)

In rig-core's OpenAI streaming (`rig-core/src/providers/openai/completion/streaming.rs`,
lines 218–243), the delta emitted to the consumer carries **only the new fragment**:

```rust
if let Some(chunk) = &tool_call.function.arguments && !chunk.is_empty() {
    let current_args = match &existing_tool_call.arguments { /* ... */ };
    let combined = format!("{current_args}{chunk}");
    // ...
    // Emit the delta so UI can show progress
    yield Ok(streaming::RawStreamingChoice::ToolCallDelta {
        id: existing_tool_call.id.clone(),
        internal_call_id: existing_tool_call.internal_call_id.clone(),
        content: streaming::ToolCallDeltaContent::Delta(chunk.clone()),
        //                                                ^^^^^  only the NEW fragment
    });
}
```

rig-core accumulates internally for its own final assembly, but the
`ToolCallDeltaContent::Delta` carries only the new chunk — never the full
accumulated string. This means there is only a single source of accumulation.

### Fix Plan

**Step 1:** Change all three provider-level accumulators to emit **only the new
fragment** in `ToolCallDelta.arguments`. The internal accumulation state can
remain for error resilience, but the emitted delta must be a fragment.

Files to change:
- `crates/provider/src/openai.rs` → `ToolCallAccumulator::merge()` — return
  `function.arguments` (the raw fragment) instead of `entry.arguments.clone()`
- `crates/provider/src/anthropic.rs` → `AnthropicToolCallAccumulator::append_json()`
  — return `Some(partial_json.to_owned())` instead of `entry.arguments.clone()`
- `crates/provider/src/responses.rs` →
  `ResponsesToolCallAccumulator::append_arguments()` — return
  `Some(delta.to_owned())` instead of `entry.arguments.clone()`

**Step 2:** Verify the runtime-level `ToolCallAccumulator::merge()` (in
`crates/runtime/src/lib.rs`) correctly does fragment-based `push_str()` — it
already does, so no change needed.

**Step 3:** Update provider unit tests to assert that emitted deltas carry only
the new fragment, not the full accumulated string. Specifically, verify that:
- After appending `{"a` then `":1}`, the second delta's arguments should be
  `":1}`, **not** `{"a":1}`.

---

## Issue 2: Tool Call Validation Failures (Missing/Additional Parameters) - RESOLVED

### Symptoms

Even with streaming disabled, tool calls frequently fail with "schema validation
failed" errors — either "Additional properties are not allowed" or "missing
required property".

### Root Cause

Two compounding problems:

#### 2a. Global `additionalProperties: false` on all tool schemas

`JsonSchema::object()` in `crates/types/src/tool.rs` (line 64) unconditionally
sets `additionalProperties: Some(false)`:

```rust
pub fn object(properties: BTreeMap<String, JsonSchema>, required: Vec<String>) -> Self {
    Self {
        schema_type: JsonSchemaType::Object,
        properties,
        required,
        additional_properties: Some(false),   // ← always strict
        // ...
    }
}
```

This schema is used for **two different purposes**:
1. Sent to the LLM as the tool definition (tells the model what arguments to
   produce)
2. Used by the runtime's jsonschema validator
   (`crates/runtime/src/tool_execution.rs`, lines 36–51) to validate arguments
   the model actually produced

The problem: many LLMs include extra properties in their tool call arguments
(e.g., a `type` field, or an `_internal` field). With `additionalProperties:
false`, any extra field causes immediate validation failure.

#### 2b. No per-provider schema adaptation

All four providers (OpenAI, Anthropic, Gemini, Responses) send the `JsonSchema`
struct as-is to the LLM API. But providers have different requirements:

- **Gemini** does not support `additionalProperties` at all — sending it may
  cause the API to reject the request or confuse the model
- **OpenAI strict mode** requires `additionalProperties: false` AND all
  properties to be in `required`
- **Anthropic** is tolerant of most schema shapes

Since oxydra sends the same strict schema to all providers, Gemini and other
non-OpenAI models may behave unpredictably.

#### 2c. Null values for optional parameters

When an LLM sends `null` for an optional field (e.g., `"output_format": null`),
the schema type is `"string"` which doesn't allow `null`. The jsonschema
validator rejects it. Meanwhile the `Option<String>` deserialization in the
execute path would handle `null` just fine.

### How picobot Does It (No Bug)

Picobot uses raw `serde_json::Value` for schemas (not a typed struct), which
preserves all JSON Schema features:

```rust
schema: json!({
    "type": "object",
    "required": ["query"],
    "properties": {
        "query": { "type": "string", "minLength": 1, "maxLength": 400 },
        "count": { "type": "integer", "minimum": 1, "maximum": 10, "default": 5 },
        "freshness": { "type": "string", "enum": ["day", "week", "month", "year"] }
    },
    "additionalProperties": false
})
```

Note: picobot's `web_search` does NOT expose a `config` object to the LLM at
all (see Issue 3). Only user-facing parameters (`query`, `count`, `freshness`)
are in the schema.

### How rig-core Does It (No Bug)

rig-core applies **per-provider schema sanitization** when building tool
definitions for each API. In `rig-core/src/providers/openai/mod.rs` (lines
32–107):

```rust
pub(crate) fn sanitize_schema(schema: &mut serde_json::Value) {
    // For OpenAI strict mode:
    // - Add additionalProperties: false to all object schemas
    // - Set required = all property keys
    // - Strip siblings next to $ref
    // - Convert oneOf → anyOf
}
```

For Gemini, rig-core has separate logic that flattens `$ref`/`$defs`, strips
unsupported keywords, and converts to Gemini's custom schema format.

### Fix Plan

**Step 1:** Remove global `additionalProperties: false` from
`JsonSchema::object()` in `crates/types/src/tool.rs`. Change line 64 from
`Some(false)` to `None`. This keeps schemas **permissive by default**.

**Step 2:** Add per-provider schema sanitization in each provider's
`From<&FunctionDecl>` implementation:

- **OpenAI** (`crates/provider/src/openai.rs` →
  `OpenAIRequestToolDefinition::from`): Recursively set
  `additionalProperties: false` and add all property names to `required`.
- **Responses** (`crates/provider/src/responses.rs` →
  `ResponsesToolDeclaration::from`): Same as OpenAI.
- **Gemini** (`crates/provider/src/gemini.rs` →
  `GeminiFunctionDeclaration::from`): Strip `additionalProperties` from the
  schema since Gemini doesn't support it.
- **Anthropic** (`crates/provider/src/anthropic.rs`): Pass schemas through
  with minimal modification (Anthropic is lenient).

**Step 3:** In the runtime's `tool_execution.rs`, strip
`additionalProperties` from the schema before building the jsonschema
validator. This makes runtime validation tolerant of extra properties the LLM
adds, while still validating types and required fields.

**Step 4:** Handle `null` values for optional parameters in the
runtime validation. Either:
- Skip the jsonschema validation for individual fields that are
  `null` and not in `required`, or
- Make optional-field sub-schemas accept `null` (e.g.,
  `{"type": ["string", "null"]}`) before validation.

---

## Issue 3: Internal Configuration Leaked to LLM via Tool Schemas - RESOLVED

### Symptoms

The LLM receives tool schemas with parameters that are **infrastructure
configuration details** the LLM should never be choosing. The LLM may
hallucinate values for these, causing unexpected behavior (wrong search
provider, altered base URLs, injected API keys).

### Affected Tools

#### `web_search` — `config` parameter

The `web_search` schema (in `crates/tools/src/lib.rs`, lines 640–645) exposes a
`config` object to the LLM:

```rust
properties.insert(
    "config".to_owned(),
    JsonSchema::new(JsonSchemaType::Object).with_description(
        "Optional provider config: provider, base_url/base_urls, query_params, ..."
    ),
);
```

The sandbox code (`crates/sandbox/src/web_search.rs`, lines 135–212) then uses
this `config` to allow the LLM to:

- **Switch the search provider** (`config.provider` → DuckDuckGo, Google, SearxNG)
- **Set base URLs** (`config.base_url`, `config.base_urls`)
- **Inject API keys** (`config.api_key`, `config.engine_id`)
- **Override query parameters** (`config.query_params`)
- **Configure SearxNG settings** (`config.engines`, `config.categories`,
  `config.safesearch`)

**Security impact:** An LLM could redirect search traffic to an attacker-controlled
endpoint, inject API keys, or disable safesearch filtering. Even without
malicious intent, the LLM may hallucinate provider names or URLs, causing
failures.

#### `web_fetch` — `max_body_chars` and `output_format`

While less severe, `max_body_chars` and `output_format` are borderline. The
`output_format` parameter is somewhat reasonable for the LLM to control (e.g.,
requesting metadata-only for large binary files). However, `max_body_chars` is
more of a system tuning parameter. The LLM could set it to `0` or an extremely
large value.

### How picobot Handles This

Picobot's `web_search` schema exposes **only user-facing parameters** to the LLM:

```rust
schema: json!({
    "type": "object",
    "required": ["query"],
    "properties": {
        "query": { "type": "string", "minLength": 1, "maxLength": 400 },
        "count": { "type": "integer", "minimum": 1, "maximum": 10, "default": 5 },
        "freshness": { "type": "string", "enum": ["day", "week", "month", "year"] }
    },
    "additionalProperties": false
})
```

All provider configuration (provider kind, API keys, base URLs, SearxNG
settings) is loaded at **construction time** from the configuration file and
stored in the tool struct. The LLM cannot influence it at all.

### Fix Plan

**Step 1:** Remove `config` from the `WebSearchTool` schema in
`crates/tools/src/lib.rs`. The LLM should only see `query`, `count`, and
`freshness`.

**Step 2:** Remove `config` from the `WebSearchArgs` struct and from the JSON
payload passed to `invoke_wasm_tool`. The search provider configuration should
come exclusively from environment variables (the existing
`search_provider_from_env()` path in `web_search.rs`).

**Step 3:** Remove `search_provider_from_arguments()` function in
`crates/sandbox/src/web_search.rs` or simplify it to only read from env. Delete
the `config` override logic (lines 136–212). Make sure that setting the search provider, url, parameters etc is available to be set via config params in agent.toml

**Step 4:** Evaluate `output_format` and `max_body_chars` on `web_fetch`:
- `output_format` is reasonable for the LLM to control — keep it.
- `max_body_chars` should have a clamped range (not exposed raw). Add
  validation to prevent 0 or excessively large values, or remove from the
  schema and let it default.

**Step 5:** Audit all other tool schemas for similar leaks. Current status:
- `file_read`, `file_write`, `file_edit`, `file_delete`, `file_list`,
  `file_search`: ✅ Only expose user-facing params
- `shell_exec`: ✅ Only `command`
- `vault_copyto`: ✅ Only `source_path`, `destination_path`
- `web_fetch`: ⚠️ `max_body_chars` could be clamped
- `web_search`: ❌ `config` leaks infrastructure details

---

## Issue 4: Responses API Ordinal Assignment Bug - RESOLVED

### Symptoms

When using the Responses API provider with multiple tool calls in a single
response, tool calls get incorrect ordinal assignments, causing them to
overwrite each other in the runtime accumulator.

### Root Cause

In `crates/provider/src/responses.rs`, the
`ResponsesToolCallAccumulator::register_output_item()` method (lines 694–713)
has a bug in ordinal computation:

```rust
fn register_output_item(&mut self, output_index: u32, call_id: Option<String>,
                        name: Option<String>) {
    let current_len = self.output_index_to_call.len();    // captured BEFORE insert
    let entry = self.output_index_to_call.entry(output_index).or_default();
    // ...
    if entry.ordinal.is_none() {
        let next = current_len.saturating_sub(1) as u32;  // ← BUG
        entry.ordinal = Some(next);
    }
}
```

For the **first** tool call:
- `current_len = 0` (map is empty)
- `or_default()` inserts entry, `entry.ordinal.is_none()` → true
- `next = 0.saturating_sub(1) = 0` ✅ correct

For the **second** tool call:
- `current_len = 1` (one entry exists)
- `or_default()` inserts new entry, `entry.ordinal.is_none()` → true
- `next = 1.saturating_sub(1) = 0` ❌ **wrong — should be 1**

Both tool calls get ordinal 0, so the runtime merges them into a single tool
call.

The same bug exists in `append_arguments()` (lines 715–734) with the same
`current_len.saturating_sub(1)` pattern.

### How Anthropic Accumulator Does It (Correct)

The `AnthropicToolCallAccumulator` in `crates/provider/src/anthropic.rs` (lines
668–741) uses a simple monotonic counter:

```rust
pub(crate) fn start_tool(&mut self, block_index: usize, ...) -> ToolCallDelta {
    let ordinal = self.next_ordinal;
    self.next_ordinal += 1;              // ← simple, correct
    self.block_to_ordinal.insert(block_index, ordinal);
    // ...
}
```

### Fix Plan

**Step 1:** Replace `current_len.saturating_sub(1)` with a monotonic
`next_ordinal` counter in `ResponsesToolCallAccumulator`, matching the
Anthropic pattern:

```rust
#[derive(Debug, Default)]
pub(crate) struct ResponsesToolCallAccumulator {
    output_index_to_call: BTreeMap<u32, ToolCallState>,
    next_ordinal: u32,   // ← add this
}

fn register_output_item(&mut self, output_index: u32, ...) {
    let entry = self.output_index_to_call.entry(output_index).or_default();
    // ...
    if entry.ordinal.is_none() {
        entry.ordinal = Some(self.next_ordinal);
        self.next_ordinal += 1;
    }
}
```

**Step 2:** Apply the same fix in `append_arguments()` for the fallback ordinal
assignment path.

**Step 3:** Add unit tests for multi-tool-call scenarios to verify ordinals are
0, 1, 2, etc.

---

## Issue 5: `response.completed` Event Emits All Tool Calls With index: 0 - RESOLVED

### Symptoms

When the Responses API streaming path handles the `response.completed` event,
multiple tool calls in the response all get emitted with `index: 0`, causing the
runtime accumulator to overwrite earlier tool calls.

### Root Cause

In `crates/provider/src/responses.rs` (lines 793–803), the
`response.completed` handler creates `ToolCallDelta` items in a loop but
hardcodes `index: 0`:

```rust
"response.completed" => {
    // ...
    for tool_call in &normalized.response.tool_calls {
        items.push(StreamItem::ToolCallDelta(ToolCallDelta {
            index: 0,                          // ← hardcoded, should be loop index
            id: Some(tool_call.id.clone()),
            name: Some(tool_call.name.clone()),
            arguments: Some(serde_json::to_string(&tool_call.arguments).unwrap_or_default()),
        }));
    }
    // ...
}
```

### Fix Plan

**Step 1:** Use `enumerate()` to assign sequential indices:

```rust
for (idx, tool_call) in normalized.response.tool_calls.iter().enumerate() {
    items.push(StreamItem::ToolCallDelta(ToolCallDelta {
        index: idx,
        // ...
    }));
}
```

**Step 2:** Consider whether `response.completed` should emit tool call deltas
at all when incremental deltas were already emitted during streaming. The
runtime would double-count tool calls if both the incremental deltas AND the
completed event feed into the same accumulator. If the streaming loop already
saw `function_call_arguments.delta` events and then also processes
`response.completed`, the tool calls would appear twice. This needs careful
evaluation — potentially the `response.completed` handler should skip
re-emitting tool call deltas that were already streamed.

---

## Issue 6: JsonSchema Lacks Constraint Support (enum, min/max, etc.)

### Symptoms

Tool schemas sent to LLMs lack validation constraints like `enum`, `minLength`,
`maxLength`, `minimum`, `maximum`, and `default`. Without these, LLMs may
generate invalid parameter values (e.g., `freshness: "today"` instead of
`freshness: "day"`), which then fail validation or produce errors.

### Root Cause

The `JsonSchema` struct in `crates/types/src/tool.rs` only supports:
- `type`
- `properties`
- `required`
- `additionalProperties`
- `description`
- `items`

It does **not** support:
- `enum` (for constrained string/integer values)
- `minLength` / `maxLength` (for string length constraints)
- `minimum` / `maximum` (for numeric range constraints)
- `default` (for default values)
- `pattern` (for regex patterns)
- `format` (e.g., `uri`, `email`)

### How picobot Does It

Picobot uses raw `serde_json::Value` for schemas, which naturally supports all
JSON Schema features:

```rust
"freshness": { "type": "string", "enum": ["day", "week", "month", "year"] }
"query": { "type": "string", "minLength": 1, "maxLength": 400 }
"count": { "type": "integer", "minimum": 1, "maximum": 10, "default": 5 }
"url": { "type": "string", "minLength": 8 }
"output_format": { "type": "string", "enum": ["auto", "raw", "text", "metadata_only"] }
```

These constraints serve as hints to the LLM, reducing the chance of invalid
values. They also enable proper jsonschema validation on the runtime side.

### Fix Plan

**Step 1:** Add optional constraint fields to `JsonSchema`:

```rust
pub struct JsonSchema {
    // existing fields...
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "enum")]
    pub enum_values: Option<Vec<serde_json::Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "minLength")]
    pub min_length: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "maxLength")]
    pub max_length: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maximum: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,
}
```

**Step 2:** Add builder methods:
```rust
pub fn with_enum(mut self, values: Vec<&str>) -> Self { ... }
pub fn with_min_length(mut self, min: u64) -> Self { ... }
pub fn with_range(mut self, min: i64, max: i64) -> Self { ... }
```

**Step 3:** Update tool schemas to use these constraints:
- `web_search.freshness`: Add `enum: ["day", "week", "month", "year"]`
- `web_search.query`: Add `minLength: 1`
- `web_search.count`: Add `minimum: 1, maximum: 10`
- `web_fetch.output_format`: Add `enum: ["auto", "raw", "text", "metadata_only"]`
- `web_fetch.url`: Add `minLength: 1`

**Step 4:** Consider whether to switch `JsonSchema` to `serde_json::Value`
entirely (like picobot) for full JSON Schema flexibility without needing to
model every keyword. This would be a larger change but provides future-proof
schema support. The tradeoff is losing type-safety on the schema construction
side.

---

## Summary Table

| # | Issue | Root Cause | Severity | Fix Scope |
|---|-------|-----------|----------|-----------|
| 1 | Streaming args malformed | Double accumulation (provider emits full, runtime re-appends) | **Critical** | 3 provider files |
| 2 | Tool validation failures | Global `additionalProperties: false` + no per-provider schema adaptation | **High** | types, 4 providers, runtime |
| 3 | Config leaked to LLM | `web_search` schema exposes provider/URL/API-key config | **High** (security) | tools/lib.rs, sandbox/web_search.rs |
| 4 | Responses ordinal bug | `saturating_sub(1)` gives all tool calls ordinal 0 | **High** | responses.rs |
| 5 | response.completed index: 0 | Hardcoded `index: 0` for all tool calls in completed event | **High** | responses.rs |
| 6 | Missing schema constraints | No `enum`/`min`/`max` support in `JsonSchema` | **Medium** | types/tool.rs, tools/lib.rs |

### Recommended Fix Order

1. **Issue 1** (streaming double accumulation) — most user-visible, causes all
   streaming tool calls to fail
2. **Issue 3** (config leak) — security concern, easy to fix
3. **Issue 4 + 5** (Responses ordinal bugs) — can be fixed together, affects
   multi-tool-call scenarios
4. **Issue 2** (validation failures) — broad impact but partially mitigated by
   fixing Issue 1
5. **Issue 6** (schema constraints) — quality improvement, reduces validation
   failures
