# Chapter 14: Productization

> **Status:** Mixed
> **Implemented:** Model catalog governance (models.dev-aligned, snapshot commands, caps overrides), session lifecycle controls, scheduler system, skill system (prompt-injected markdown skills, browser automation built-in)
> **Remaining:** MCP support, persona governance
> **Last verified against code:** 2026-03-03

## Overview

With the core runtime, isolation model, and channel infrastructure in place, remaining product surfaces promote backlog concerns into first-class, typed architecture. This chapter covers model catalog governance, explicit session lifecycle controls, scheduled execution, skill loading, persona governance, and MCP integration.

These capabilities share a common principle: they are **governed execution surfaces**, not unconstrained extension points. Each one is typed, auditable, and policy-controlled.

## Model Catalog Governance

### Current State

The model catalog is built on a three-tier resolution strategy: cached catalog (fetched from models.dev), auto-fetch on first use, and a compiled-in pinned snapshot (`pinned_model_catalog.json`) as fallback. The pinned snapshot contains 93 models across OpenAI, Anthropic, and Google with full models.dev-aligned metadata: capability flags (`tool_call`, `reasoning`, `structured_output`), token limits, pricing, modalities, and more. An Oxydra-specific `oxydra_caps_overrides.json` overlay provides additional capabilities not expressed by models.dev (e.g. streaming support, deprecation status).

### Evolution

The catalog evolves from a lightweight pinned list into a fully typed schema with operator-driven governance:

1. **Snapshot generation command** — an operator-invokable command (through TUI or runner control paths) that fetches upstream catalog data (e.g., from models.dev), validates it against the typed schema, and produces a deterministic artifact
2. **Versioned artifacts** — generated snapshots are committed into the repository with a version stamp, not auto-applied
3. **Validation at startup** — runtime rejects unknown or invalid model metadata during config validation; unrecognized model IDs in configuration fail fast
4. **No self-mutation** — the runtime/LLM process cannot modify catalog files in-place; catalog updates are always operator-driven
5. **Web catalog API** — the web configurator exposes `GET /api/v1/catalog` (browsable model data), `GET /api/v1/catalog/status` (source info), and `POST /api/v1/catalog/refresh` (trigger fetch). The model picker UI uses the catalog for searchable, provider-grouped model selection with capability badges.

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

### Current State (Implemented)

Session lifecycle is now explicit and multi-session aware across TUI and external adapters:

1. **Protocol controls:** `CreateSession`, `ListSessions`, and `SwitchSession` frame flows are implemented end-to-end.
2. **Handshake semantics:** `Hello { create_new_session, session_id }` supports deterministic reconnect/join and explicit new-session creation.
3. **Bounded session/concurrency policy:** Gateway config enforces per-user limits (`max_sessions_per_user` default 50, `max_concurrent_turns_per_user` default 10) with bounded FIFO queueing for top-level turns.
4. **Idle lifecycle management:** sessions with no subscribers and idle beyond `session_idle_ttl_hours` (default 48) are archived and evicted from in-memory runtime context.

### Behavioral Constraints

- New session boundaries are explicit (`/new`, `CreateSession`) rather than heuristic rollovers
- Previous sessions remain durable in the session store for listing/audit and resumption
- Session identity remains canonical per channel context mapping (`(channel_id, channel_context_id) → session_id`)

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

> **Status:** Implemented (Phase A/B/C complete). See Chapter 15 Phase 20 for full scope.

### Purpose

Skills extend the agent's capabilities through markdown documents that are injected into the system prompt at session start. They teach the LLM domain-specific workflows using existing tools — no new Rust code or tool schemas required. The browser automation skill is the first built-in; users can author their own skills or override built-ins.

### Skill File Format

Each skill lives in a folder containing a `SKILL.md` file and an optional `references/` subdirectory:

```
config/skills/BrowserAutomation/
├── SKILL.md                    # Frontmatter + markdown body (injected into prompt)
└── references/
    └── pinchtab-api.md         # Lazy-loaded supplementary reference (not injected)
```

`SKILL.md` begins with YAML frontmatter followed by markdown content:

```markdown
---
name: browser-automation
description: Control a headless Chrome browser via Pinchtab's REST API
activation: auto
requires:
  - shell_exec
env:
  - PINCHTAB_URL
priority: 50
---

## Browser Automation (Pinchtab)

You can control a headless Chrome browser via Pinchtab at `{{PINCHTAB_URL}}`...
```

**Frontmatter fields** (types in `types/src/skill.rs` — `SkillMetadata`):

| Field | Required | Default | Description |
|---|---|---|---|
| `name` | Yes | — | Unique kebab-case identifier. Used for deduplication across all discovery tiers. |
| `description` | Yes | — | One-line summary for diagnostics and logs. |
| `activation` | No | `auto` | `auto` — inject when all conditions met; `always` — always inject; `manual` — never auto-inject. Maps to `SkillActivation` enum. |
| `requires` | No | `[]` | Tool names that must be **ready** (not just registered) for this skill to activate. Currently recognized: `"shell_exec"`, `"browser"`. |
| `env` | No | `[]` | (`env_vars` in Rust) Environment variable names that must be set. Values are substituted into `{{VAR}}` placeholders in the skill body. **Only use for non-sensitive values** (URLs, feature flags) — secrets must be referenced as `$VAR` shell env vars in the skill body. |
| `priority` | No | `100` | Ordering when multiple skills are active (lower = earlier in prompt). |

Skills are capped at **3,000 estimated tokens** (`content.len() / 4`). Skills exceeding the cap are rejected at load time with a warning.

### Types

Defined in `types/src/skill.rs`:

```rust
pub enum SkillActivation { Auto, Manual, Always }

pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    pub activation: SkillActivation,
    pub requires: Vec<String>,   // tool names (env field alias: "env")
    pub env_vars: Vec<String>,   // env var names
    pub priority: i32,
}

pub struct Skill {
    pub metadata: SkillMetadata,
    pub content: String,         // Markdown body after frontmatter
    pub source_path: PathBuf,
}

pub struct RenderedSkill {
    pub name: String,
    pub content: String,         // `{{VAR}}` placeholders replaced
    pub priority: i32,
}
```

### Discovery and Precedence

The skill loader (`runner/src/skills.rs`) scans four tiers, deduplicating by `name` (highest-precedence tier wins — no merging):

| Tier | Location | Priority |
|---|---|---|
| Embedded built-ins | Compiled into `oxydra-vm` binary via `rust-embed` | Lowest |
| System/config | `<config_dir>/skills/` (repo `config/skills/` in the binary) | Low |
| User | `~/.config/oxydra/skills/` | Medium |
| Workspace | `<workspace>/.oxydra/skills/` | Highest |

Both folder-based skills (`<Folder>/SKILL.md`) and bare `.md` files directly in the `skills/` directory are supported. The `name` field in the frontmatter — not the filename or folder name — determines identity for deduplication.

**Embedded built-ins** are compiled into the `oxydra-vm` binary at build time via `rust-embed` (`BuiltinSkills` struct pointing at `config/skills/`). This means the browser automation skill ships inside the binary and works across all deployment modes without needing file copying at startup.

### Activation Evaluation

At session start, `evaluate_activation()` filters the discovered skills:

- **`Always`** — always included, no conditions checked.
- **`Manual`** — never auto-included.
- **`Auto`** — included if:
  1. All tools listed in `requires` are **ready** according to `ToolAvailability` (not just registered — this prevents the browser skill from activating when the shell sidecar is unavailable).
  2. All variable names in `env_vars` are present in the environment map passed to the loader.

The browser automation skill requires `shell_exec` to be ready **and** `PINCHTAB_URL` to be set. The runner only sets `PINCHTAB_URL` when Pinchtab's health check passes at startup — so if Pinchtab failed to start, the skill will not activate.

### Rendering and Injection

After activation evaluation:

1. **Render** — `render_skill()` replaces `{{VAR}}` placeholders with values from the environment map.
2. **Format** — `format_skills_prompt()` concatenates active skill bodies under an `## Active Skills` heading.
3. **Inject** — The formatted string is appended to the system prompt constructed by `build_system_prompt()` in `runner/src/bootstrap.rs`.

```rust
// In build_system_prompt():
let skills_section = load_and_render_skills(
    &paths.config_dir,
    user_skills_dir.as_deref(),
    &paths.workspace_dir,
    availability,
    &env_map,
);
format!("...{shell_note}{scheduler_note}{memory_note}{skills_section}{specialists_note}")
```

Activation is evaluated once at session start. Skills are stable for the lifetime of the session.

### Lazy-Loaded References

For skills that need extensive reference material (full API docs, parameter tables), the skill body stays under the token cap by containing only a concise summary plus a pointer:

```markdown
For the full API reference:
`cat /shared/.oxydra/skills/BrowserAutomation/references/pinchtab-api.md`
```

The reference file is extracted from the embedded binary to the shared workspace at startup by `extract_builtin_references()`. The LLM reads it on demand via `shell_exec` or `file_read` — only when needed, keeping the system prompt lean.

```rust
// In oxydra-vm startup:
skills::extract_builtin_references(&shared_dir);
// Writes: /shared/.oxydra/skills/BrowserAutomation/references/pinchtab-api.md
```

### Token Budget Guidance

| Section | Approx. tokens |
|---|---|
| Base prompt (identity, filesystem) | ~300 |
| Memory section (when enabled) | ~800 |
| Scheduler section (when enabled) | ~150 |
| Browser Automation skill | ~1,036 |

Skill content benefits from **prompt caching** on providers that support it (Anthropic, OpenAI). Stable system prompt prefixes are processed once and served from cache on subsequent turns, keeping the per-turn cost low.

### Browser Automation Skill

The built-in `browser-automation` skill (`config/skills/BrowserAutomation/SKILL.md`) is the first production skill. It teaches the agent to drive headless Chrome through Pinchtab's REST API using `curl` commands in the sandboxed shell.

**Why skill-based instead of a dedicated tool:**

| Concern | Dedicated tool | Skill (shell + curl) |
|---|---|---|
| API drift | Every Pinchtab change needs Rust code + tests | Update the markdown file |
| Provider schema | JSON schema quirks per provider | No schema — uses existing `shell_exec` |
| Maintenance | ~2,000 lines of Rust | ~200 lines of markdown |
| Flexibility | New capabilities need new Rust variants | LLM uses any endpoint immediately |

**Core workflow the skill teaches:**

1. `POST /navigate` → creates a tab, returns `tabId`
2. `GET /tabs/{id}/snapshot?filter=interactive&maxTokens=2000` → accessibility tree with clickable refs
3. `POST /tabs/{id}/action` with `{"kind":"click","ref":"e5"}` etc.
4. Re-snapshot with `diff=true` for ~90% token savings
5. Repeat until done; use `send_media` to deliver screenshots/PDFs

**File integration** is explicitly documented in the skill body: screenshots saved to `/shared/`, PDFs, downloads, uploads — all via Pinchtab endpoints, with `send_media` to deliver files to the user.

**If blocked** by CAPTCHAs, 2FA, or login walls, the skill instructs the agent to call `request_human_assistance`.

See `config/skills/BrowserAutomation/SKILL.md` for the full injected content and `config/skills/BrowserAutomation/references/pinchtab-api.md` for the lazy-loaded full API reference.

### Logging

Skill discovery and activation log at `info` level:

```
skill discovery complete  discovered=2 names="always-active, browser-automation"
skills activated          active_count=1 names="browser-automation"
# Or, when conditions are not met:
skill not activated: required tool(s) not ready  skill=browser-automation missing_tools=shell_exec
skill not activated: required env var(s) not set  skill=browser-automation missing_env=PINCHTAB_URL
```

### Overriding and Disabling Built-ins

Users place a skill with the same `name` value at a higher-precedence location to override a built-in. To disable a built-in, set `activation: manual` in the override — `manual` skills are never auto-injected. See the [README Customizing section](../../README.md#skills-custom-workflows-and-overrides) for path details.

### Implementation Sequence (Phases A–C)

1. **Phase A** — `Skill`/`SkillMetadata`/`RenderedSkill` types; `runner/src/skills.rs` loader (discover, evaluate, render, format); `build_system_prompt()` integration; 36 unit tests.
2. **Phase B** — Pinchtab infrastructure in shell-vm: port allocation, `BRIDGE_TOKEN` generation, env forwarding, health check polling, shell policy overlay, embedded reference extraction.
3. **Phase C** — `config/skills/BrowserAutomation/SKILL.md` written; `rust-embed` embedded built-ins so the skill ships inside the binary; additional integration tests validating end-to-end activation and prompt injection.

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
3. **Session lifecycle** (Phase 18) — ✅ Complete: explicit new-session command, deterministic identity
4. **Scheduler** (Phase 19) — ✅ Complete: durable store, polling executor, four LLM tools, conditional notification, cron/interval/once cadences, auto-disable on failures
5. **Skill system** (Phase 20) — ✅ Complete: markdown-based skills, embedded built-ins, browser automation skill, three-tier discovery with workspace override, activation evaluation against tool readiness and env vars, prompt injection
6. **Persona governance** (Phase 21) — SOUL.md/SYSTEM.md loading, immutability enforcement

Each phase has an explicit verification gate enforced by automated tests before closure.
