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

**File:** `runtime/src/scrubbing.rs`

Before tool output is returned to the LLM or stored in memory, it passes through `scrub_tool_output`:

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

Detected high-entropy strings are redacted as `[REDACTED:high-entropy]`.

## Tracing and Instrumentation

The runtime is instrumented with `tracing` spans throughout:

- **Turn-level spans** — turn number, state transitions
- **Provider call spans** — model, token usage, latency
- **Tool execution spans** — tool name, safety tier, execution time
- **Budget spans** — context utilization percentage, cost accumulation

These spans are emitted via `tracing-subscriber` and can be upgraded to OpenTelemetry export without changing instrumentation code.

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
