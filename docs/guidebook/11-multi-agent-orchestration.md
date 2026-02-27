# Chapter 11: Multi-Agent Orchestration

## Overview

Multi-agent orchestration extends Oxydra from a single-agent runtime into a network of specialized subagents coordinated by a master router. Instead of overloading one LLM context with coding, research, and communication tasks simultaneously, the system can delegate subtasks to purpose-built agents with their own system prompts, tool registries, security capability sets, and model selections.

This chapter describes the planned design for subagent spawning, delegation, state graphs, and advanced gateway routing. The foundation is already in place: the `AgentRuntime` supports per-turn cancellation/timeout/budget guards, the gateway manages per-user sessions, and the `Channel` trait provides the routing abstraction. Multi-agent orchestration builds on top of these surfaces.

## Subagent Delegation Model

Status (update): Partial implementation complete — Agent definitions, agent-specific model/provider routing, and a runtime-backed delegation executor are implemented. The `delegate_to_agent` tool is registered at bootstrap with config-aware specialist schema, top-level sessions route by `agent_name`, and delegation resolves explicit vs inherited selection (`default` always root). Remaining work: state graph engine, lane-based queueing, and more conservative tool allowlisting per-agent.



### SubagentBrief

Cross-agent delegation uses a structured contract rather than raw transcript copying. This reduces token waste and makes delegation explicit and auditable.

```rust
pub struct SubagentBrief {
    /// What the subagent should accomplish
    pub goal: String,
    /// Key facts extracted from the parent context
    pub key_facts: Vec<String>,
    /// Which tools the subagent is allowed to use
    pub available_tools: Vec<String>,
    /// Filesystem path the subagent operates within
    pub workspace_path: PathBuf,
    /// How the subagent should format its result
    pub expected_output_format: OutputFormat,
    /// Maximum turns before forced termination
    pub max_turns: u32,
    /// Maximum token cost budget
    pub token_budget: f64,
}
```

The parent agent constructs a `SubagentBrief` from its current context, selecting only the relevant facts and tools. The subagent receives this brief as its initial context — it never sees the parent's full conversation history.

### Lifecycle Management

Each subagent receives its own:
- `CancellationToken` (child of the parent's token, so parent cancellation cascades)
- Turn timeout and max-turn budget (from the brief, not global defaults)
- Token cost budget (deducted from the parent's remaining budget)
- Isolated tool registry (subset of the parent's tools, determined by the brief)

Subagent execution uses `tokio::select!` over completion, timeout, cancellation, and budget exhaustion — the same pattern the base runtime already uses for individual turns, extended to an entire agent lifecycle.

### Result Handoff

When a subagent completes, its output is returned to the parent as a structured result:
- Success: the output formatted per the `expected_output_format`
- Failure: an error description plus partial results if any were produced
- Budget exhaustion: partial results with an explicit "budget exceeded" signal

The parent agent receives this as a tool result message and can decide whether to retry with a new subagent, adjust the brief, or present the result to the user.

## State Graph Routing

Complex multi-step workflows are modeled as deterministic state graphs rather than ad-hoc LLM reasoning chains. This provides predictable progression, explicit guard conditions, and clear audit trails.

### Graph Structure

```
┌─────────────┐     guard: intent       ┌──────────────┐
│   Intake     │ ─── classified ────────►│  Delegation   │
│  (Router)    │                         │  (Spawner)    │
└──────┬──────┘                          └──────┬───────┘
       │                                        │
       │ direct answer                          │ subagent result
       ▼                                        ▼
┌─────────────┐                          ┌──────────────┐
│   Respond    │◄────────────────────────│  Synthesize   │
│  (Terminal)  │                         │  (Merger)     │
└─────────────┘                          └──────────────┘
```

Each node in the graph represents a processing phase with:
- An entry action (e.g., spawn a subagent, prepare context)
- Guard conditions for outgoing transitions (e.g., "subagent succeeded", "confidence > threshold")
- An exit action (e.g., append results to parent context)

State transitions are evaluated deterministically — the runtime checks guard conditions in priority order and takes the first matching transition.

### Design Constraints

- Graphs are defined declaratively (configuration, not code) so operators can inspect and modify routing without recompilation
- The gateway tracks graph state per session, persisting it across reconnections
- Each graph node can specify a different model (e.g., use a smaller/cheaper model for classification, a larger model for synthesis)
- Cycle detection is enforced at graph construction time — infinite loops in workflow definitions are rejected before execution

## Advanced Gateway Routing

### Lane-Based Queueing

The gateway currently uses mutex-based rejection (a new turn is rejected if one is already active). Multi-agent orchestration upgrades this to lane-based queueing:

- Messages for the same user are queued sequentially in a dedicated processing lane (identified by `user_id` hash)
- Messages for different users execute fully in parallel
- Background tasks (scheduled runs, subagent completions) are routed to a separate lane so they don't block real-time conversational turns
- Each lane has bounded depth — backpressure is applied when the queue exceeds capacity

### Multi-Agent Session Tracking

The gateway maintains a session tree per user:

```
User Session
├── Main Agent (root)
│   ├── Research Subagent (spawned for query)
│   │   └── completed, result merged
│   └── Code Subagent (spawned for implementation)
│       └── active, turn 3/10
└── Background Agent (scheduled task)
    └── completed silently
```

Each node in the tree is a subagent with its own `runtime_session_id`, brief, and lifecycle state. The gateway tracks the tree structure so progress can be streamed to the user with trace-linked status updates.

### Streaming During Delegation

While a subagent is executing, the gateway keeps the user's connection alive and streams status updates:
- "Delegating research to specialized agent..."
- Subagent stream deltas (if the parent opts to forward them)
- Final synthesis when all subagents complete

This prevents the user from seeing long periods of silence during complex multi-step workflows.

## Trace Continuity

OpenTelemetry trace context is propagated through subagent handoffs:
- The parent agent's trace ID is included in the `SubagentBrief`
- The subagent creates a child span linked to the parent's trace
- One user request can be traced end-to-end across parent and child agents in Jaeger or any OTel-compatible backend

This is critical for debugging multi-agent workflows and understanding where time and tokens are spent.

## Implementation Approach

Multi-agent orchestration builds on existing infrastructure:

1. **`SubagentBrief` type** — defined in `types`, consumed by `runtime` and `gateway`
2. **Subagent spawning** — `AgentRuntime` gets a `spawn_subagent()` method that constructs a child runtime with the brief's constraints
3. **State graph engine** — a new module in `gateway` that evaluates declarative graph definitions
4. **Lane-based queue** — replaces the mutex-based rejection in `gateway` session management
5. **Session tree tracking** — extends `UserSessionState` to maintain parent-child relationships

The `AgentRuntime` and `CancellationToken` patterns are reused without modification. The gateway's WebSocket protocol gains new frame types for delegation status and subagent progress.

## Design Boundaries

- The master router is an LLM-powered agent, not a hardcoded classifier. It uses the same `AgentRuntime` as any other agent, with a system prompt optimized for intent classification and delegation
- Subagents share the user's workspace and memory namespace but get isolated conversation contexts
- Subagent tool registries are always subsets of the parent's tools — a subagent cannot escalate its own capabilities
- Budget cascading is strict: the sum of all subagent budgets cannot exceed the parent's remaining budget
- The OpenAI Responses API (`/v1/responses` with `previous_response_id` chaining) is particularly well-suited for subagent delegation — spawning a subagent with a fresh response chain provides clean context isolation without duplicating memory payloads. This provider is planned but not yet implemented (see Chapter 3 and Chapter 15, Gap 5)
