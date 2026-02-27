# Chapter 5: Agent Runtime

## Overview

The agent runtime is the central nervous system of Oxydra. It implements a deterministic, turn-based execution loop that sends context to an LLM provider, processes responses, executes tool calls, and iterates until the model produces a final response or a budget limit is reached. The runtime contains zero planning heuristics — all cognitive progression is delegated to the LLM.

## AgentRuntime Struct

```rust
pub struct AgentRuntime {
    provider: Box<dyn Provider>,
    tool_registry: ToolRegistry,
    limits: RuntimeLimits,
    stream_buffer_size: usize,
    memory: Option<Arc<dyn Memory>>,
    memory_retrieval: Option<Arc<dyn MemoryRetrieval>>,
}
```

- `provider` — the active LLM provider (wrapped in `ReliableProvider` for retries)
- `tool_registry` — all available tools with policy enforcement
- `limits` — turn count, cost, timeout budgets
- `memory` — persistence layer for conversation history
- `memory_retrieval` — hybrid retrieval interface for RAG and summarization

## Turn-Based Loop

The core loop lives in `run_session_internal`. Here is the exact execution flow:

```
┌─────────────────────────────────────┐
│           Entry Point               │
│  receive user message + session_id  │
└──────────────┬──────────────────────┘
               │
               ▼
┌─────────────────────────────────────┐
│         Persist User Message        │
│  store in memory with sequence num  │
└──────────────┬──────────────────────┘
               │
               ▼
        ┌──────────────┐
        │  turn < max? │──── No ──► BudgetExceeded
        │  cancelled?  │──── Yes ─► Cancelled
        └──────┬───────┘
               │ Yes (continue)
               ▼
┌─────────────────────────────────────┐
│     Maybe Trigger Summarization     │
│  if token usage > trigger_ratio     │
│  condense old turns into summary    │
└──────────────┬──────────────────────┘
               │
               ▼
┌─────────────────────────────────────┐
│     Prepare Provider Context        │
│  inject system prompt               │
│  inject rolling summary             │
│  inject retrieved memory snippets   │
│  truncate history to fit budget     │
└──────────────┬──────────────────────┘
               │
               ▼
┌─────────────────────────────────────┐
│     Request Provider Response       │
│  try stream(), fallback to          │
│  complete() on failure              │
│  accumulate deltas + tool calls     │
└──────────────┬──────────────────────┘
               │
               ▼
┌─────────────────────────────────────┐
│       Enforce Cost Budget           │
│  update accumulated_cost            │
│  check against max_cost             │
└──────────────┬──────────────────────┘
               │
               ▼
┌─────────────────────────────────────┐
│     Persist Assistant Message       │
│  store response in memory           │
└──────────────┬──────────────────────┘
               │
               ▼
        ┌──────────────┐
        │  tool calls? │──── No ──► Return Response (Yielding)
        └──────┬───────┘
               │ Yes
               ▼
┌─────────────────────────────────────┐
│       Execute Tool Calls            │
│  ReadOnly: parallel (join_all)      │
│  Others: sequential                 │
│  validate → execute → scrub         │
└──────────────┬──────────────────────┘
               │
               ▼
┌─────────────────────────────────────┐
│     Persist Tool Results            │
│  store each result in memory        │
│  append to context                  │
└──────────────┬──────────────────────┘
               │
               └────────► loop back to turn check
```

## State Machine

```rust
pub enum TurnState {
    Streaming,       // Receiving response from provider
    ToolExecution,   // Processing and executing tool calls
    Yielding,        // Terminal — final response produced
    Cancelled,       // Terminal — execution stopped via CancellationToken
}
```

State transitions are explicit and logged via `tracing` spans. The loop only transitions to `Yielding` when the provider produces a response with no tool calls, or to `Cancelled` when the cancellation token fires.

## Tool Dispatch

### Parallel vs Sequential

The runtime categorizes tool calls by `SafetyTier` and dispatches them accordingly:

1. **Contiguous `ReadOnly` tools** are batched and executed concurrently via `futures::future::join_all`
2. When a `SideEffecting` or `Privileged` tool is encountered:
   - Any pending `ReadOnly` batch is flushed first
   - The non-readonly tool executes alone
3. This continues through all tool calls in order

This preserves correctness (side-effecting tools see the effects of prior operations) while maximizing throughput for safe read operations.

### Self-Correction

When a tool call fails — whether from JSON validation, argument type mismatch, or execution error — the runtime does not crash:

1. The error is caught as `RuntimeError::Tool`
2. It is formatted as a `MessageRole::Tool` message:
   ```
   Tool execution failed: invalid type for 'count', expected integer got string
   ```
3. The error message is persisted and appended to context
4. The loop continues to the next turn, giving the LLM a chance to retry

## Budget Enforcement

### Turn Budget

`max_turns` (default 8) limits the number of loop iterations. When exceeded, the runtime returns `RuntimeError::BudgetExceeded`.

### Cost Budget

After each provider response, token usage is converted to cost units and accumulated. If `accumulated_cost` exceeds `max_cost`, the loop terminates.

### Turn Timeout

Every provider call and tool execution is wrapped in `tokio::time::timeout(turn_timeout)`. Timeouts are per-turn, not per-session.

### Cancellation

A `CancellationToken` (from `tokio-util`) is threaded through the entire execution path. Every async wait — provider calls, tool runs, stream reads — uses `tokio::select!` with the cancellation future. When cancelled:
- The current operation is aborted
- State transitions to `Cancelled`
- The loop exits cleanly

## Context Window Management

Before each provider call, the runtime prepares the context to fit within the model's token budget.

### Token Budget Breakdown

```
┌─────────────────────────────────────┐
│  System Prompt                      │  (always included)
├─────────────────────────────────────┤
│  Rolling Summary                    │  (if summarization active)
├─────────────────────────────────────┤
│  Retrieved Memory Snippets          │  (from hybrid search)
├─────────────────────────────────────┤
│  Tool Schema Declarations           │  (all registered tools)
├─────────────────────────────────────┤
│  Conversation History               │  (newest first, truncated)
├─────────────────────────────────────┤
│  Safety Buffer                      │  (reserved headroom)
└─────────────────────────────────────┘
```

Token counting uses `tiktoken-rs` with the `cl100k_base` encoding (GPT-4 compatible). History is filled in reverse-chronological order — newest messages first. When the budget is exhausted, older messages are dropped (they are already represented in the rolling summary if summarization is active).

### Attachment-Aware Budget Management

Messages with inline media attachments (images, audio, etc.) receive special handling during context window management:

- **Latest user message guarantee:** If the most recent user message cannot fit within the token budget (common for large attachments), the turn fails with `RuntimeError::BudgetExceeded` rather than silently dropping it.
- **Token estimation bypass:** When any message in the context contains non-empty attachments, the fast-path token estimation is disabled (media bytes cannot be token-counted), ensuring the full budget fitting algorithm is always used.
- **Attachment stripping for older turns:** Before appending a new user message, attachment bytes on older user messages in the context are cleared to prevent unbounded memory growth across multi-turn conversations.
- **Memory persistence:** Attachment metadata (mime type) is preserved when storing messages to memory, but raw bytes are cleared before persistence to avoid bloating the session store.

## Memory Integration

The runtime treats memory as a write-through layer:

### Storing

Every message (user, assistant, tool result) is assigned a monotonically increasing `sequence` number and stored immediately after it is generated. This ensures conversations survive process restarts.

### Retrieval

Before calling the provider, the runtime extracts the latest user prompt and performs a hybrid search (vector + FTS) against the session's stored chunks. Relevant snippets are injected as a system message:

```
Relevant context from memory:
- [snippet 1]
- [snippet 2]
```

### Session Management

Sessions are lazily created on first message storage. The runtime can restore a session by ID, rebuilding the `Context` from stored `conversation_events`.

## Credential Scrubbing

**File:** `runtime/src/scrubbing.rs`, `runtime/src/lib.rs`, `runtime/src/tool_execution.rs`

Before tool output is returned to the LLM or stored in memory, it is host-path scrubbed and then passed through `scrub_tool_output`.
Media attachments emitted through runtime stream events are also wrapped with a scrubbing sender so `StreamItem::Media.file_path` is rewritten to virtual paths before forwarding.

### Keyword-Based Redaction

A compiled `RegexSet` matches patterns like:
- `api_key: "sk-..."` → `api_key: "[REDACTED]"`
- `Authorization: Bearer ...` → `Authorization: [REDACTED]`
- `password`, `secret`, `token` key-value patterns

### Entropy-Based Detection

For strings that lack keyword labels but look like secrets:

1. String length must be 24-512 characters
2. Must contain a mix of character types (not pure hex, which could be a hash)
3. Shannon entropy must be ≥ 3.8

Detected high-entropy strings are redacted as `[REDACTED]`.

## Tracing and Instrumentation

The runtime is instrumented with `tracing` spans throughout:

- **Turn-level spans** — turn number, state transitions
- **Provider call spans** — model, token usage, latency
- **Tool execution spans** — tool name, safety tier, execution time
- **Budget spans** — context utilization percentage, cost accumulation

These spans are emitted via `tracing-subscriber` and can be upgraded to OpenTelemetry export without changing instrumentation code.

## Scheduled Turn Execution

**File:** `runtime/src/scheduler_executor.rs`

The `SchedulerExecutor` is a background task that polls the `SchedulerStore` for due schedules and dispatches them as agent turns through the `ScheduledTurnRunner` trait.

### ScheduledTurnRunner Trait

```rust
#[async_trait]
pub trait ScheduledTurnRunner: Send + Sync {
    async fn run_scheduled_turn(
        &self,
        user_id: &str,
        runtime_session_id: &str,
        prompt: String,
        cancellation: CancellationToken,
    ) -> Result<String, RuntimeError>;
}
```

This trait is implemented by `RuntimeGatewayTurnRunner` in the gateway crate, allowing the scheduler executor to trigger agent turns without depending on the gateway crate directly. Each scheduled turn uses a schedule-specific `runtime_session_id` (format: `scheduled:{schedule_id}`) so scheduled runs don't pollute the user's interactive session history.

### SchedulerExecutor

```rust
pub struct SchedulerExecutor {
    store: Arc<dyn SchedulerStore>,
    turn_runner: Arc<dyn ScheduledTurnRunner>,
    notifier: Arc<dyn SchedulerNotifier>,
    config: SchedulerConfig,
    cancellation: CancellationToken,
}
```

The executor runs a `tokio::time::interval` loop (configurable via `poll_interval_secs`, default 15 seconds) that:

1. Queries the store for schedules where `next_run_at <= now` and `status = 'active'`
2. Dispatches due schedules concurrently (bounded by `max_concurrent`)
3. For each schedule: builds the prompt (augmenting with `[NOTIFY]` instruction for conditional notification), runs the turn, records the result (including full output), computes the next run time, and handles notification routing

### Full Output Storage

Each run's complete output is stored in the `output` column of the run record. This enables the `schedule_run_output` tool to retrieve historical run outputs without needing the LLM to re-execute the task.

### Notification Routing

After a scheduled turn completes, the executor determines whether to notify the user based on the schedule's `NotificationPolicy`:

| Policy | Behavior |
|--------|----------|
| `Always` | Response is always routed to the user via `SchedulerNotifier` |
| `Conditional` | Response is checked for `[NOTIFY]` prefix — if present, the message after the marker is routed; otherwise silent |
| `Never` | No notification — results stored in memory only |

The `SchedulerNotifier` trait is async and receives the full `ScheduleDefinition`, enabling origin-aware routing:

```rust
#[async_trait]
pub trait SchedulerNotifier: Send + Sync {
    async fn notify_user(&self, schedule: &ScheduleDefinition, frame: GatewayServerFrame);
}
```

The `GatewayServer` implementation routes notifications to the originating channel:

1. **TUI sessions** (`channel_id == "gateway"`) — publishes to the specific WebSocket session matching the `channel_context_id`
2. **External channels** (e.g. Telegram) — looks up a registered `ProactiveSender` for the channel and invokes `send_notification()`
3. **Legacy fallback** — broadcasts to all sessions for the user when no specific origin is available

### Failure Notifications

When `notify_after_failures` is set in `SchedulerConfig`, the executor sends a notification to the user after that many consecutive failures, regardless of `NotificationPolicy`. This ensures users are alerted to persistent problems even for schedules with `Never` notification policy.

### Automatic Schedule Management

After each run, the executor:
- **Computes next run** — uses `next_run_for_cadence()` to determine the next `next_run_at`
- **One-shot completion** — marks `Once` cadence schedules as `Completed` with `next_run_at = None` after successful execution
- **Failure tracking** — increments `consecutive_failures` on failed runs; resets to 0 on success
- **Auto-disable** — disables schedules that exceed `auto_disable_after_failures` consecutive failures
- **History pruning** — prunes old run records beyond `max_run_history`

### Lifecycle

The executor is spawned as a `tokio::spawn` background task in `oxydra-vm` when `scheduler_store` is present. Its `CancellationToken` is cancelled during graceful shutdown (on `ctrl_c`), ensuring clean termination.

## Progress Events

The runtime emits `StreamItem::Progress` events at key transition points within `run_session_internal`. These are delivered on the same `stream_events` channel as `StreamItem::Text` deltas and flow all the way to connected channels, giving users real-time visibility into multi-step execution.

```rust
pub enum StreamItem {
    Text(String),
    Progress(RuntimeProgressEvent),  // ← emitted by runtime
    // ... other variants
}

pub struct RuntimeProgressEvent {
    pub kind: RuntimeProgressKind,
    pub message: String,   // e.g. "[2/8] Executing tools: file_read"
    pub turn: usize,
    pub max_turns: usize,
}

pub enum RuntimeProgressKind {
    ProviderCall,
    ToolExecution { tool_names: Vec<String> },
    RollingSummary,
}
```

**Emission points:**

| Point | Event |
|-------|-------|
| Before each `request_provider_response()` call | `RuntimeProgressKind::ProviderCall` |
| Before tool dispatch loop | `RuntimeProgressKind::ToolExecution { tool_names }` |

Progress events are not emitted from within provider streams — they originate only in the runtime loop itself, at transitions the provider cannot observe. Channels decide independently how to render them. The TUI shows the `progress.message` string as the input bar title (replacing "Waiting...") while a turn is active.
