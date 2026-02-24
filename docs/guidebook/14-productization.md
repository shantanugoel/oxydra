# Chapter 14: Productization

## Overview

With the core runtime, isolation model, and channel infrastructure in place, remaining product surfaces promote backlog concerns into first-class, typed architecture. This chapter covers model catalog governance, explicit session lifecycle controls, scheduled execution, skill loading, persona governance, and MCP integration.

These capabilities share a common principle: they are **governed execution surfaces**, not unconstrained extension points. Each one is typed, auditable, and policy-controlled.

## Model Catalog Governance

### Current State

The model catalog is currently a pinned JSON snapshot (`models.json`) embedded at compile time. It contains 8 models across OpenAI and Anthropic with typed metadata: `ModelId`, `ProviderId`, capability flags (`supports_tools`, `supports_streaming`), token limits, and pricing.

### Evolution

The catalog evolves from a lightweight pinned list into a fully typed schema with operator-driven governance:

1. **Snapshot generation command** — an operator-invokable command (through TUI or runner control paths) that fetches upstream catalog data (e.g., from models.dev), validates it against the typed schema, and produces a deterministic artifact
2. **Versioned artifacts** — generated snapshots are committed into the repository with a version stamp, not auto-applied
3. **Validation at startup** — runtime rejects unknown or invalid model metadata during config validation; unrecognized model IDs in configuration fail fast
4. **No self-mutation** — the runtime/LLM process cannot modify catalog files in-place; catalog updates are always operator-driven

### Schema

The catalog schema captures:

```rust
pub struct ModelDescriptor {
    pub id: ModelId,
    pub provider: ProviderId,
    pub display_name: String,
    pub supports_tools: bool,
    pub supports_streaming: bool,
    pub supports_reasoning_traces: bool,
    pub supports_json_mode: bool,
    pub max_input_tokens: u32,
    pub max_output_tokens: u32,
    pub input_price_per_million: f64,
    pub output_price_per_million: f64,
    pub deprecation_state: DeprecationState,
}
```

## Explicit Session Lifecycle

### Current State

Sessions are currently created lazily on first message storage and identified by `session_id`. There is no explicit "new session" control — sessions continue indefinitely until the process restarts.

### New Session Control

Users get an explicit "new session" command that:

1. Generates a new canonical `session_id`
2. Preserves the user's workspace, identity, and channel bindings
3. Starts a clean conversational slate — no prior messages in the context
4. Keeps previous sessions intact for recall and audit

The command is available through all channels (TUI, external adapters) and produces a deterministic, auditable session boundary.

### Design Constraints

- No implicit session rollover heuristics (e.g., "new session after 30 minutes of inactivity") — rollover is always explicit
- Previous sessions remain queryable through memory retrieval
- Session identity is canonical across channels: the `(user_id, channel_id, channel_session_id) → session_id` mapping (Chapter 12) generates the new session ID

## Scheduler System

### Current State (Implemented — Phase 19)

The scheduler system is fully implemented and provides one-off and periodic task execution through the same runtime policy envelope used for interactive turns.

### Architecture

```
┌──────────────────┐     ┌──────────────────┐     ┌──────────────────┐
│  SchedulerStore  │────►│ SchedulerExecutor│────►│   AgentRuntime   │
│  (durable defs)  │     │ (due-job runner) │     │  (policy rails)  │
└──────────────────┘     └──────────────────┘     └──────────────────┘
        │                         │                         │
        ▼                         ▼                         ▼
   libSQL tables           tokio interval loop        run_scheduled_turn()
   (schedules,             polls due_schedules()      via ScheduledTurnRunner
    schedule_runs)         dispatches concurrently     trait (gateway impl)
```

- **SchedulerStore** (`memory/src/scheduler_store.rs`) — persists schedule definitions and run history in the memory database with explicit ownership by `user_id`. Implemented as `LibsqlSchedulerStore` using a dedicated libSQL connection obtained via `LibsqlMemory::connect_for_scheduler()`.
- **SchedulerExecutor** (`runtime/src/scheduler_executor.rs`) — evaluates due entries on a configurable polling interval (default 15s) and dispatches them as bounded agent turns through the `ScheduledTurnRunner` trait.
- **Execution** — scheduled runs execute through `RuntimeGatewayTurnRunner.run_scheduled_turn()`, which creates a context and calls `AgentRuntime::run_session` — inheriting all policy controls (timeouts, budgets, tool gates, tracing).

### Schedule Management

Schedules are managed through four tools exposed to the LLM (see Chapter 4 for details):

- `schedule_create` — create a new one-off or periodic schedule with goal, cadence, timezone, and notification policy
- `schedule_search` — search and list schedules with filters (status, name, cadence type, notification policy) and pagination
- `schedule_edit` — modify, pause, or resume a schedule (partial-patch semantics)
- `schedule_delete` — permanently remove a schedule (cascading deletes run history)

Validation happens at creation/edit time: cadence grammar is parsed and validated by the `cron` crate, budget limits are checked against user-level maximums, timezones are validated against the `chrono-tz` IANA database, and the schedule is persisted only after all checks pass.

### Silent Execution

Scheduled tasks support three notification modes:

- **Always notify** — output is always sent to the user's active channel via `GatewayServerFrame::ScheduledNotification`
- **Conditionally notify** — the LLM's response is checked for a `[NOTIFY]` prefix; if present, the message after the marker is routed to the user; otherwise the run is silent
- **Never notify** — the task runs silently, with results stored in memory only

The conditional notification protocol is implemented by augmenting the scheduled turn's prompt with instructions about the `[NOTIFY]` marker convention.

### Cadence

The scheduler supports three cadence types:
- **One-off** (`once`) — execute once at a specific RFC 3339 timestamp
- **Cron** (`cron`) — execute periodically according to a 5-field cron expression with timezone support
- **Interval** (`interval`) — execute at a fixed interval (in seconds, minimum 60s)

Cadence evaluation uses the `cron` crate for cron expressions and `chrono-tz` for timezone-aware scheduling. The `next_run_for_cadence()` function computes the next fire time; `validate_cadence()` ensures minimum intervals are met.

### Configuration

The scheduler is disabled by default. To enable it, add `[scheduler] enabled = true` to the agent config. See Chapter 2 for all configuration options including `max_turns`, `max_cost` (operator-only budgets), `min_interval_secs`, `auto_disable_after_failures`, and `default_timezone`.

### Session Isolation

Each schedule gets its own `runtime_session_id` (format: `scheduled:{schedule_id}`). This ensures:
- Scheduled runs don't pollute the user's interactive session history
- Each schedule accumulates its own conversation context across runs
- Memory tools scoped to `user_id` still work correctly

## Skill System

### Purpose

Skills extend the agent's capabilities through markdown manifests loaded from a managed per-user workspace folder. They are a governed capability boundary — the LLM can create, load, and use skills, but cannot delete them.

### Folder Structure

```
<workspace_root>/<user_id>/skills/
├── code-review.md
├── meeting-notes.md
└── data-analysis.md
```

Each skill file contains:
- A name and description
- System prompt additions or behavioral instructions
- Optional tool declarations or tool usage patterns
- Activation conditions (when this skill should be applied)

### Loading and Access

- **Discovery** — `SkillLoader` scans the managed folder at session start and on explicit reload
- **Registry** — discovered skills are registered in a `SkillRegistry` available to the runtime
- **API-mediated access** — the LLM interacts with skills through a dedicated skill API, not through raw filesystem traversal of the skills folder
- **Immutability policy** — the LLM can create new skill files and read existing ones, but deletion is denied by policy enforcement in the tool layer

### Validation

Skills can be validated in strict schema mode (structured YAML/TOML frontmatter) or permissive mode (freeform markdown). The validation mode is operator-configurable.

## Persona Governance

### SOUL.md and SYSTEM.md

Agent identity and behavioral policy are externalized into two operator-visible files:

- **`SOUL.md`** — identity and persona envelope (name, personality traits, communication style, tone)
- **`SYSTEM.md`** — system-behavior directives (operational rules, safety guidelines, domain expertise boundaries)

### Loading

These files are loaded at session start from the user's workspace directory. The loading process:

1. Locate files in `<workspace_root>/<user_id>/SOUL.md` and `<workspace_root>/<user_id>/SYSTEM.md`
2. Parse and validate content
3. Inject into the system prompt at the beginning of each provider context preparation
4. Surface effective precedence in diagnostics (which file was loaded, from which path)

### Immutability

Persona files are non-editable by the LLM:
- Write/edit/delete operations targeting `SOUL.md` or `SYSTEM.md` are rejected by the security policy layer
- Modifications happen through human/operator workflows only
- Changes take effect on the next session start (not mid-session)

This ensures the agent cannot modify its own behavioral boundaries or identity, even if instructed to do so by a user or through prompt injection.

## MCP (Model Context Protocol) Integration

### Purpose

MCP support allows Oxydra to discover and call external tool servers alongside its native tools. This bridges the ecosystem of MCP-compatible tool providers (IDE integrations, database connectors, API wrappers) into the agent's tool registry.

### Architecture

MCP integration uses an adapter pattern:

```rust
pub struct McpToolAdapter {
    /// MCP client connection (stdio or HTTP+SSE transport)
    client: McpClient,
    /// Tool declarations fetched from the MCP server
    tools: Vec<McpToolDeclaration>,
}

impl Tool for McpToolAdapter {
    fn schema(&self) -> FunctionDecl { /* map MCP tool schema to internal format */ }
    async fn execute(&self, args: &str) -> Result<String, ToolError> { /* forward to MCP server */ }
    fn timeout(&self) -> Duration { /* MCP-specific timeout */ }
    fn safety_tier(&self) -> SafetyTier { /* configured per MCP server */ }
}
```

The `Tool` trait was designed from the start to be MCP-compatible — the `McpToolAdapter` implements the same trait as native tools, so MCP tools are registered in the same `ToolRegistry` and participate in the same validation, timeout, and policy enforcement.

### Discovery

MCP tool servers are discovered via configuration:

```toml
[[mcp.servers]]
name = "github"
transport = "stdio"
command = "mcp-server-github"
safety_tier = "SideEffecting"

[[mcp.servers]]
name = "database"
transport = "http"
url = "http://localhost:8080"
safety_tier = "Privileged"
```

At runtime startup, the MCP client connects to each configured server, fetches available tool declarations, and registers them as `McpToolAdapter` instances in the tool registry.

### Safety

MCP tools inherit the same policy enforcement as native tools:
- Safety tier is configured per MCP server (not per individual tool within the server)
- Timeout enforcement applies to MCP tool calls
- Output scrubbing (credential redaction) runs on MCP tool results
- HITL approval gates apply based on the configured safety tier

MCP is enabled only after operational confidence in the native tool governance model is established.

## Implementation Sequence

These productization features are implemented in dependency order:

1. **Model catalog governance** (Phase 13) — ✅ Complete: typed schema, snapshot generation, startup validation, provider registry, capability overrides
2. **MCP support** (Phase 17) — adapter implementation, discovery, safety integration
3. **Session lifecycle** (Phase 18) — explicit new-session command, deterministic identity
4. **Scheduler** (Phase 19) — ✅ Complete: durable store, polling executor, four LLM tools, conditional notification, cron/interval/once cadences, auto-disable on failures
5. **Skill system** (Phase 20) — loader, registry, API-mediated access, deletion prevention
6. **Persona governance** (Phase 21) — SOUL.md/SYSTEM.md loading, immutability enforcement

Each phase has an explicit verification gate enforced by automated tests before closure.
