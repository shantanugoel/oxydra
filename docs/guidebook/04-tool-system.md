# Chapter 4: Tool System

## Overview

The tool system bridges the strongly-typed Rust world with the loosely-structured JSON generation of LLMs. It provides automatic schema generation, validation with self-correction, safety classification, and policy enforcement — ensuring that even malformed or malicious tool calls are handled safely.

## The Tool Trait

Defined in `types/src/tool.rs`:

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn schema(&self) -> FunctionDecl;
    async fn execute(&self, args: &str) -> Result<String, ToolError>;
    fn timeout(&self) -> Duration;
    fn safety_tier(&self) -> SafetyTier;
}
```

- `schema()` — returns the JSON Schema declaration that tells the LLM how to call this tool
- `execute()` — receives raw JSON arguments as a string, validates and executes
- `timeout()` — maximum execution duration (enforced by the registry)
- `safety_tier()` — classification that determines execution policy

## Safety Tiers

```rust
pub enum SafetyTier {
    ReadOnly,       // No side effects (file_read, file_search, file_list)
    SideEffecting,  // Modifies state within sandbox (file_write, file_edit, web_fetch)
    Privileged,     // High-risk operations (shell_exec)
}
```

Safety tiers drive two behaviors:
1. **Parallel execution** — `ReadOnly` tools run concurrently; others run sequentially
2. **Approval gates** — `Privileged` tools can be gated behind user confirmation callbacks

## Schema Types

```rust
pub struct FunctionDecl {
    pub name: String,
    pub description: Option<String>,
    pub parameters: JsonSchema,
}

pub enum JsonSchemaType {
    Object { properties: BTreeMap<String, JsonSchema>, required: Vec<String> },
    String { enum_values: Option<Vec<String>>, description: Option<String> },
    Integer { description: Option<String> },
    Number { description: Option<String> },
    Boolean { description: Option<String> },
    Array { items: Box<JsonSchema>, description: Option<String> },
}
```

## The `#[tool]` Procedural Macro

**Crate:** `tools-macros`

The `#[tool]` attribute macro automates `FunctionDecl` generation from Rust function signatures:

```rust
#[tool]
/// Reads the contents of a file at the given path.
async fn file_read(path: String) -> String {
    // implementation
}
```

This generates a hidden function `__tool_function_decl_file_read()` that builds the `FunctionDecl` with:
- Function name as tool name
- Doc comments as the tool description
- Parameter names and types mapped to JSON Schema types

### Type Mapping

| Rust Type | JSON Schema Type |
|-----------|-----------------|
| `String`, `&str` | `String` |
| `bool` | `Boolean` |
| `i8`..`u128`, `isize`, `usize` | `Integer` |
| `f32`, `f64` | `Number` |

### Constraints

- Functions must be `async`
- Generics are not supported
- Methods with `self` receivers are not supported
- Complex patterns in parameters are not supported
- Compile-time errors are generated for violations (verified via `trybuild` tests)

## Core Tools

| Tool Name | Safety Tier | Description |
|-----------|-------------|-------------|
| `file_read` | `ReadOnly` | Reads UTF-8 text from a file |
| `file_search` | `ReadOnly` | Recursively searches for text patterns in files |
| `file_list` | `ReadOnly` | Lists directory entries |
| `file_write` | `SideEffecting` | Creates or overwrites a file with content |
| `file_edit` | `SideEffecting` | Replaces a specific text snippet in a file |
| `file_delete` | `SideEffecting` | Deletes a file or directory |
| `attachment_save` | `SideEffecting` | Saves an inbound user attachment from the current turn into `/shared` or `/tmp` |
| `web_fetch` | `SideEffecting` | Fetches URL content (converts to Markdown/Text) |
| `web_search` | `SideEffecting` | Performs web searches via configured providers |
| `vault_copyto` | `SideEffecting` | Securely copies data from vault to shared/tmp workspace |
| `send_media` | `ReadOnly` | Sends a workspace file as a channel media attachment |
| `delegate_to_agent` | `SideEffecting` | Delegates a goal to a named specialist agent; forwards delegated media outputs when available |
| `shell_exec` | `Privileged` | Executes shell commands (via sidecar or host) |
| `memory_search` | `ReadOnly` | Searches the user's personal memory store (with optional tag filtering) |
| `memory_save` | `SideEffecting` | Saves a new note to the user's memory (with optional tags) |
| `memory_update` | `SideEffecting` | Replaces the content of an existing note by note_id |
| `memory_delete` | `SideEffecting` | Deletes a note from the user's memory by note_id |
| `scratchpad_read` | `ReadOnly` | Reads the session-scoped working scratchpad |
| `scratchpad_write` | `SideEffecting` | Replaces the session scratchpad with a list of active working items |
| `scratchpad_clear` | `SideEffecting` | Clears the session scratchpad to free capacity |
| `schedule_create` | `SideEffecting` | Creates a new one-off or recurring scheduled task |
| `schedule_search` | `ReadOnly` | Searches and lists scheduled tasks with filters and pagination |
| `schedule_edit` | `SideEffecting` | Modifies, pauses, or resumes a scheduled task |
| `schedule_delete` | `SideEffecting` | Permanently deletes a scheduled task |
| `schedule_runs` | `ReadOnly` | Lists recent run history metadata for a scheduled task |
| `schedule_run_output` | `ReadOnly` | Retrieves full output of a specific run with chunked pagination |

Most tools (except `BashTool`) execute through the `WasmToolRunner` backend (`WasmWasiToolRunner` when available, otherwise `HostWasmToolRunner`), which enforces capability-based mount policies per tool class (see Chapter 7: Security Model).

### BashTool Backends

The `BashTool` operates in three modes determined by the bootstrap environment:

1. **Host** — direct execution via `std::process::Command` (local development)
2. **Session** — forwarded to a `ShellSession` in the shell-daemon sidecar (production)
3. **Disabled** — rejects all execution if no safe execution environment is available

### Memory Tools

**File:** `tools/src/memory_tools.rs`

Four LLM-callable tools expose the persistent memory system directly to the agent. They allow the LLM to proactively save, search, update, and delete user-scoped notes — turning passive memory retrieval into an active knowledge management capability.

#### Per-User Scoping

All memory tools operate within a per-user namespace using the session ID pattern `memory:{user_id}`. This ensures:
- Each user has an isolated note store
- Notes persist across conversation sessions
- The LLM cannot access another user's memory

#### Per-Turn Context (`ToolExecutionContext`)

All four tools receive a `ToolExecutionContext` on each invocation, which carries the current `user_id` and `session_id`. The runtime sets this context at the start of each turn. Unlike the previous shared-mutable approach, each tool invocation receives an immutable snapshot of the context, eliminating race conditions under concurrent turns. If the user context is not set (e.g., during startup before a handshake), tool execution returns a descriptive error rather than silently failing.

#### Tool Details

| Tool | Parameters | Returns |
|------|-----------|---------|
| `memory_search` | `query` (required), `top_k` (optional, default 5, max 20), `include_conversation` (optional, default false), `tags` (optional, array of strings for tag filtering) | JSON array of `{text, source, score, note_id, tags, created_at, updated_at}` results |
| `memory_save` | `content` (required), `tags` (optional, array of strings) | `{note_id, message}` with the generated `note-{uuid}` identifier |
| `memory_update` | `note_id` (required), `content` (required), `tags` (optional, array of strings) | `{note_id, message}` confirming the update |
| `memory_delete` | `note_id` (required) | `{message}` confirming deletion |

- `memory_search` uses the hybrid retrieval pipeline (native libSQL vector index + FTS5) with configurable `vector_weight` and `fts_weight` parameters. When `tags` are provided, results are filtered at the database level to only include notes containing every requested tag.
- `memory_save` generates a UUID-based `note_id` (format: `note-{uuid}`) and stores the content through `MemoryRetrieval::store_note`; it is intended for durable facts/preferences/decisions and corrected working procedures. Tags are normalized (trimmed, lowercased, deduplicated, capped at 16) and stored as chunk metadata. Backend-owned timestamps (`created_at`, `updated_at`) are set automatically.
- `memory_update` performs a delete-then-save under the same `note_id`, preserving the identifier while replacing content when previously saved information or procedures are corrected. Tags can be replaced during update.
- `memory_delete` removes all chunks and events associated with the `note_id`.

#### Registration

Memory tools are registered during bootstrap via `register_memory_tools()` when a memory backend is configured. The function takes a `ToolRegistry`, `Arc<dyn MemoryRetrieval>`, and the retrieval weight parameters, then creates and registers all four tools.

### Scratchpad Tools

**File:** `tools/src/scratchpad_tools.rs`

Three LLM-callable tools expose a session-scoped working memory scratchpad, enabling the agent to track active sub-goals during long multi-step execution without polluting persistent cross-session memory.

#### Semantics

- Scratchpad data is **session-scoped** — it does not persist across sessions unless the agent explicitly saves items to durable memory via `memory_save`.
- Each write **replaces** the entire scratchpad (not append-only), keeping state fresh.
- Hard limits are enforced: maximum 32 items, each up to 240 characters.

#### Tool Details

| Tool | Parameters | Returns |
|------|-----------|---------|
| `scratchpad_read` | (none) | `{session_id, items, updated_at, item_count}` |
| `scratchpad_write` | `items` (required, array of 1–32 strings, each ≤240 chars) | `{updated, item_count, message}` |
| `scratchpad_clear` | (none) | `{cleared, message}` |

#### Registration

Scratchpad tools are registered during bootstrap via `register_scratchpad_tools()` when a memory backend is configured, alongside memory tools.

### Scheduler Tools

**File:** `tools/src/scheduler_tools.rs`

Six LLM-callable tools expose the scheduler system, enabling the LLM to create, search, edit, delete scheduled tasks, and inspect their run history. These tools are registered during bootstrap via `register_scheduler_tools()` when the scheduler is enabled and a memory backend is available.

#### Per-User Scoping

All scheduler tools operate within a per-user namespace using the `user_id` from `ToolExecutionContext`. Ownership checks prevent users from accessing or modifying another user's schedules. At creation time, `channel_id` and `channel_context_id` from `ToolExecutionContext` are captured so notifications route back to the originating channel.

#### Tool Details

| Tool | Key Parameters | Returns |
|------|---------------|---------|
| `schedule_create` | `goal` (required), `cadence_type` (required: cron/once/interval), `cadence_value` (required), `name` (optional), `timezone` (optional), `notification` (optional: always/conditional/never) | Created schedule with `schedule_id`, `next_run_at`, `status` |
| `schedule_search` | `name`, `status`, `cadence_type`, `notification`, `limit` (default 20, max 50), `offset` (for pagination) | Paginated results with `schedules`, `total`, `remaining`, `hint` |
| `schedule_edit` | `schedule_id` (required), plus optional: `name`, `goal`, `cadence_type`+`cadence_value`, `timezone`, `notification`, `status` (active/paused) | Updated schedule definition |
| `schedule_delete` | `schedule_id` (required) | Confirmation message |
| `schedule_runs` | `schedule_id` (required), `limit` (optional, 1–20, default 5) | Run metadata list (timestamps, status, summary) — no full output |
| `schedule_run_output` | `run_id` (required), `offset` (optional), `max_chars` (optional, 200–8000, default 4000) | Chunked full run output with `remaining` count for pagination |

#### Run History Tools

`schedule_runs` returns lightweight metadata per run (timestamps, status, output summary, cost) to keep tool output within truncation limits. `schedule_run_output` retrieves the full persisted output of a specific run with chunking support — callers can page through long outputs by incrementing `offset`, ensuring no JSON truncation.

#### Key Design Decisions

- **`cadence_type` + `cadence_value` as separate parameters** — Forces structured input that our code validates, preventing the LLM from hallucinating hybrid formats.
- **`goal` as the full prompt** — The tool description instructs the LLM to write goals as complete, self-contained instructions, since scheduled turns run without conversational history.
- **No `max_turns` or `max_cost` in the schema** — These are operator-configured only via `SchedulerConfig`. The LLM cannot override execution budgets.
- **Pagination in `schedule_search`** — The `remaining` count and `hint` string guide the LLM's pagination behavior naturally.
- **`schedule_edit` for pause/resume** — Single tool with partial-patch semantics handles pause, resume, cadence change, goal update, and notification policy change.

#### Cadence Validation

At creation and edit time, the cadence is validated:
- **Cron expressions** are parsed with the `cron` crate (5-field standard, internally prepended with `0` for seconds). Minimum interval between firings is checked against `min_interval_secs`.
- **One-off timestamps** must be in the future (RFC 3339 format).
- **Intervals** must meet the minimum `min_interval_secs` (default 60 seconds).
- **Timezones** are validated against the `chrono-tz` IANA timezone database.

#### Registration

Scheduler tools are registered during bootstrap via `register_scheduler_tools()` when `config.scheduler.enabled` is `true` and a memory backend is available. The function takes a `ToolRegistry`, `Arc<dyn SchedulerStore>`, and `&SchedulerConfig`, then creates and registers all six tools.

## Tool Registry

**File:** `tools/src/registry.rs`

The `ToolRegistry` manages tool lifecycle and enforces execution policies.

### Registration

Tools are registered at startup via `bootstrap_runtime_tools`, which reads the `RunnerBootstrapEnvelope` to determine which tools are available based on the current sandbox tier and sidecar status.

### Execution Pipeline (`execute_with_policy`)

Every tool call passes through this pipeline:

```
1. Tool Lookup      → Find tool by name in registry
2. Safety Gate      → Check SafetyTier against caller-provided approval policy
3. Security Policy  → WorkspaceSecurityPolicy validates arguments:
                      - Filesystem: path canonicalization + root boundary check
                      - Shell: command allowlist + syntax restriction
4. Timeout Wrap     → tokio::time::timeout(tool.timeout())
5. Execute          → tool.execute(args)
6. Output Truncate  → Cap output at max_output_bytes (default 16KB)
7. Return           → Truncated result or formatted error
```

### Output Truncation

If tool output exceeds `max_output_bytes` (16KB default), it is truncated and a suffix is appended:

```
[output truncated — original size: 142,857 bytes]
```

This prevents context window exhaustion from verbose tool output.

## Validation and Self-Correction

When an LLM generates malformed tool call arguments, the system does not crash. Instead:

1. The runtime's `ToolCallAccumulator` reconstructs the full argument string from streaming deltas
2. `serde_json::from_str` attempts to parse the arguments
3. If parsing fails, `jsonschema` validates against the tool's declared schema
4. The validation error is formatted as a `MessageRole::Tool` result:
   ```
   Tool execution failed: missing required field 'path' in arguments
   ```
5. This error message is appended to the conversation context
6. The provider is re-invoked, giving the LLM a chance to self-correct

This transforms what would be a fatal runtime crash into a recoverable iteration step. The model sees its mistake and typically corrects it on the next turn.
