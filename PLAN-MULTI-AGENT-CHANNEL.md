# Plan: Multi-Agent Orchestration + External Channels + Concurrent Sessions

## Table of Contents

1. [Executive Summary](#1-executive-summary)
2. [Current State Assessment](#2-current-state-assessment)
3. [Unified Session Model](#3-unified-session-model)
4. [Concurrent Session Architecture](#4-concurrent-session-architecture)
5. [External Channel: Telegram](#5-external-channel-telegram)
6. [Multi-Agent Orchestration](#6-multi-agent-orchestration)
7. [Gateway Evolution](#7-gateway-evolution)
8. [Configuration Design](#8-configuration-design)
9. [Database Migrations](#9-database-migrations)
10. [Protocol Evolution](#10-protocol-evolution)
11. [Implementation Phases](#11-implementation-phases)
12. [Testing Strategy](#12-testing-strategy)
13. [UX Design](#13-ux-design)
14. [Risk Analysis](#14-risk-analysis)

---

## 1. Executive Summary

This plan covers three deeply intertwined capabilities that must be designed together to avoid rewrites:

1. **Concurrent Sessions** — Multiple simultaneous conversations with the same or different agents, via the same or different channels. Like opening multiple Claude Code windows or doing `/new` in ChatGPT.
2. **External Channel (Telegram)** — First non-TUI channel adapter, not feature-flagged, enabled/disabled via config (default disabled).
3. **Multi-Agent Orchestration** — Parent agents delegating to specialized subagents with bounded context handoffs.

### Why These Are Interdependent

- A Telegram user sending `/new` must create a new session the same way a second TUI instance does — they share the same session lifecycle model.
- Multi-agent subagent spawning creates child sessions that must coexist with interactive sessions without blocking.
- The gateway's current single-session-per-user model (`HashMap<String, UserSessionState>`) must be replaced with a multi-session model before either Telegram or multi-agent can work correctly.
- Lane-based queueing (needed for multi-agent) and concurrent sessions (needed for Telegram `/new`) both require replacing the current `active_turn: Mutex<Option<ActiveTurnState>>` with per-session concurrency.

### Design Principles (drawn from OpenClaw, Claude Code, ChatGPT)

- **Session = conversation thread** — each session has its own message history, independent turns, and lifecycle.
- **User = identity** — a user can have many sessions across channels. Workspace, memory namespace, and long-term notes are shared per-user.
- **Channel = transport** — channels deliver messages, they don't own sessions. A session can be accessed from multiple channels/connections.
- **Agent = runtime configuration** — each agent has a system prompt, model, tool set. Multi-agent means multiple runtime configurations, not multiple processes.

---

## 2. Current State Assessment

### What We Have

| Component | Current Design | Limitation |
|-----------|---------------|------------|
| **Gateway sessions** | `HashMap<String, Arc<UserSessionState>>` keyed by `user_id` | **One session per user** — second TUI connection to same user gets the same session |
| **Turn execution** | `active_turn: Mutex<Option<ActiveTurnState>>` | **One turn at a time per user** — second request rejected with "active turn already running" |
| **Context storage** | `RuntimeGatewayTurnRunner.contexts: Mutex<HashMap<String, Context>>` keyed by `runtime_session_id` | Supports multiple contexts in theory, but gateway maps user→single session |
| **Session identity** | `default_runtime_session_id()` → `"runtime-{user_id}"` | No explicit session creation/listing/switching |
| **Channel support** | TUI only (WebSocket client → gateway) | No external channels |
| **Agent configuration** | Single `AgentConfig` per `oxydra-vm` process | Single agent per process |

### What Already Works In Our Favor

1. **`RuntimeGatewayTurnRunner.contexts`** already supports multiple `runtime_session_id` → `Context` mappings — the turn runner is session-aware, the gateway just doesn't expose it.
2. **`Channel` trait** is designed for multiple adapters — `ChannelRegistry` already supports register/lookup/remove by ID.
3. **`CancellationToken`** hierarchy (parent→child) is already used and maps perfectly to agent→subagent lifecycle.
4. **`SchedulerExecutor`** already creates independent sessions (`scheduled:{schedule_id}`) — this pattern directly extends to multi-session.
5. **`GatewayServerFrame`** protocol is extensible (serde tagged enum) — new frames can be added without breaking existing clients.
6. **Memory layer** is session-scoped — `conversation_events` are already keyed by `session_id`, so multiple sessions "just work" at the storage level.

---

## 3. Unified Session Model

### Core Design: Session as First-Class Entity

Replace the current `user_id → single session` mapping with a proper session registry:

```
┌──────────────────────────────────────────────────────┐
│                    GatewayServer                     │
│                                                      │
│  users: HashMap<UserId, UserState>                   │
│    │                                                 │
│    └── UserState                                     │
│         ├── user_id: String                          │
│         ├── sessions: HashMap<SessionId, SessionState>│
│         └── active_session_id: SessionId (default)   │
│                                                      │
│  SessionState                                        │
│    ├── session_id: String                            │
│    ├── user_id: String                               │
│    ├── channel_origin: String (tui/telegram/...)     │
│    ├── created_at: Instant                           │
│    ├── active_turn: Mutex<Option<ActiveTurnState>>   │
│    ├── events: broadcast::Sender<GatewayServerFrame> │
│    ├── latest_terminal_frame: Mutex<Option<...>>     │
│    └── subscribers: AtomicU32 (connection count)     │
└──────────────────────────────────────────────────────┘
```

### Session Identity Scheme

```
Session ID format: "session-{user_id}-{uuid7}"

Examples:
  session-alice-01944a8b-7c3e-7f1a-9c4e-3b2a1d5e8f7c   (TUI session 1)
  session-alice-01944a8b-8d2f-7a3b-bc5f-4c3b2e6f9a8d   (TUI session 2)
  session-alice-tg-12345-01944a8b-9e3a-7b4c-cd6a-5d4c3f7aab9e  (Telegram chat)
  scheduled:sched-01944a8b...                           (Scheduled, existing format)
  subagent:parent-session-id:subagent-name:uuid         (Subagent, new)
```

The format includes the user ID for debugging/logging, but session lookup is always by the full session ID. UUID v7 is used for time-ordering.

### Session Lifecycle

```
             ┌──────────┐
             │  Create   │  ← Hello (new), /new command, subagent spawn
             └─────┬─────┘
                   │
             ┌─────▼─────┐
             │   Active   │  ← Turns execute, messages accumulate
             └─────┬─────┘
                   │
          ┌────────┼────────┐
          │        │        │
    ┌─────▼─────┐  │  ┌────▼─────┐
    │   Idle    │  │  │ Archived │  ← Explicit close, TTL expiry
    └───────────┘  │  └──────────┘
                   │
             ┌─────▼─────┐
             │  Resumed   │  ← Reconnect with session_id, /resume
             └───────────┘
```

**Key rules:**
- Sessions are never implicitly rolled over (no "new session after N minutes of inactivity")
- Creating a new session does not close the previous one — previous sessions can still be resumed
- A session with zero subscribers (no connected clients) remains live indefinitely (until TTL or explicit close)
- Archived sessions retain their conversation history in memory (queryable for audit/recall)

### Connection vs. Session Separation

This is the critical conceptual split:

- **Connection** = a WebSocket (TUI) or a Telegram chat. A transport pipe. One connection can only interact with one session at a time.
- **Session** = a conversation thread. Multiple connections can observe the same session (e.g., two TUI windows pointed at the same session).

```
Connection 1 (TUI window A)  ──┐
                                ├──► Session "research-task"
Connection 2 (TUI window B)  ──┘

Connection 3 (TUI window C)  ────► Session "code-review"

Connection 4 (Telegram chat)  ────► Session "telegram-default"
```

---

## 4. Concurrent Session Architecture

### Multiple TUI Instances (Like Claude Code)

**How it works today (broken):** Two `oxydra-tui` processes with `--user alice` connect to the same gateway. The second connection joins the existing `UserSessionState` and both share the same session. If one sends a turn, the other sees it streaming. But the second cannot send its own turn (rejected: "active turn already running").

**How it should work:**

1. Each `oxydra-tui` invocation gets a **new session by default** (like opening a new Claude Code window).
2. The TUI can optionally specify `--session <id>` to resume/observe an existing session.
3. The `Hello` frame carries a `session_id` field (optional). If omitted, the gateway creates a new session. If provided, the gateway joins that session.

**Changes to `GatewayClientHello`:**

```rust
pub struct GatewayClientHello {
    pub request_id: String,
    pub protocol_version: u16,
    pub user_id: String,
    // CHANGED: renamed for clarity; specifying a session joins it
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    // NEW: explicitly request a new session (overrides session_id)
    #[serde(default)]
    pub create_new_session: bool,
}
```

**Session resolution logic in gateway:**

```
Hello arrives with (user_id, session_id, create_new_session):

  if create_new_session:
    → generate new session_id, create SessionState, return HelloAck with new ID

  if session_id is Some(id):
    → look up existing session for this user
    → if found: join it (subscribe to its broadcast), return HelloAck
    → if not found: return error "session not found"

  if session_id is None:
    → look up user's default/most-recent session
    → if found: join it
    → if not found: create a new session (first connection for this user)
```

### `/new` Command (Like ChatGPT)

Available in all channels (TUI, Telegram):

1. User types `/new` (or sends the command through the channel).
2. The channel adapter intercepts it before it reaches the LLM.
3. A `NewSession` client frame is sent to the gateway.
4. Gateway creates a new `SessionState`, sets it as the connection's active session.
5. Gateway sends `SessionSwitched { new_session_id }` frame back.
6. TUI updates its status bar; Telegram sends "Started a new conversation" message.

**Why `/new` and not a gateway-level concept:** The user should be able to start a fresh conversation at any time. This is the simplest UX — a single command that works everywhere. No need to disconnect/reconnect.

### Session Listing and Switching

**TUI:** Add `--session <id>` CLI flag and `/sessions` command to list recent sessions:

```
/sessions          → List recent sessions with IDs and creation dates
/switch <id>       → Switch this connection to a different session
/new               → Create and switch to a new session
/new <name>        → Create a named session
```

**Telegram:**

```
/new               → Start a new conversation
/sessions          → List recent sessions
/switch <id>       → Switch to a previous session
```

### Per-Session Turn Isolation

Each `SessionState` has its own `active_turn: Mutex<Option<ActiveTurnState>>`. This means:

- Session A can have a turn running while Session B accepts a new turn — they're fully independent.
- Two connections to the same Session A still see the "active turn already running" error if one sends while the other is running — this is correct because they share a conversation context.

### Resource Limits

To prevent abuse:

```toml
[gateway]
max_sessions_per_user = 10        # default
max_concurrent_turns_per_user = 3  # default (across all sessions)
session_idle_ttl_hours = 24        # archive after 24h of inactivity
```

`max_concurrent_turns_per_user` is a global cap — even though sessions have independent turn locks, the gateway enforces a total concurrency limit per user to control costs.

---

## 5. External Channel: Telegram

### Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Feature flag? | **No** — compiled in, config-enabled | Per your requirement. Keeps the build simple. |
| Default state | **Disabled** | Safe default. Requires explicit `[channels.telegram] enabled = true`. |
| Library | **`teloxide` 0.13+** | Best Rust Telegram framework; provides long-polling, FSM, and axum webhook support |
| Transport | **Long polling** initially, webhook later | Long polling is simpler to set up (no public URL needed), perfect for personal/single-user agent |
| Adapter location | **`channels` crate** | Follows existing architecture — adapters in `channels`, used by `gateway` |

### Telegram Adapter Architecture

```
┌──────────────────────┐
│  TelegramAdapter     │  (in channels crate)
│                      │
│  ├── bot: Bot        │  (teloxide Bot instance)
│  ├── config: TgConf  │  (token, allowed_ids, etc.)
│  ├── inbound_tx      │  (mpsc → gateway)
│  └── outbound_rx     │  (mpsc ← gateway)
│                      │
│  Responsibilities:   │
│  1. Receive Telegram │
│     Update → convert │
│     to ChannelInbound│
│  2. Receive Channel  │
│     OutboundEvent →  │
│     send_message()   │
│  3. Handle /new,     │
│     /sessions, etc.  │
│  4. Rate limiting    │
└──────────────────────┘
```

### Message Flow: Telegram → Agent

```
Telegram API
    │
    ▼ (long polling / teloxide update listener)
TelegramAdapter.listen()
    │
    ▼ (convert telegram::Message → ChannelInboundEvent)
ChannelInboundEvent {
    channel_id: "telegram",
    connection_id: "{chat_id}",   // Telegram chat ID = connection
    frame: GatewayClientFrame::SendTurn { ... }
}
    │
    ▼ (via mpsc channel to gateway)
GatewayServer.handle_channel_inbound()
    │
    ├── Validate sender (allowed_sender_ids)
    ├── Resolve session (chat_id → session mapping)
    ├── Start turn (reuses existing RuntimeGatewayTurnRunner)
    │
    ▼ (streaming response)
GatewayServerFrame::AssistantDelta { ... }
    │
    ▼ (gateway routes to TelegramAdapter)
TelegramAdapter.send(ChannelOutboundEvent)
    │
    ▼ (accumulate deltas, send complete message)
bot.send_message(chat_id, full_response)
```

### Telegram Session Mapping

Each Telegram chat ID maps to a session:

```
(user_id, "telegram", chat_id) → session_id
```

**Default behavior:** Each Telegram chat gets one session. The user can use `/new` to start a fresh session within the same chat. `/switch` lets them switch between sessions.

### Telegram Response Handling

Telegram has a 4096-character message limit. For long responses:

1. Accumulate all `AssistantDelta` frames until `TurnCompleted`.
2. If the final message exceeds 4096 chars, split into multiple messages at paragraph boundaries.
3. Use Telegram's Markdown V2 formatting for code blocks.
4. For very long code output, send as a document/file attachment.

### Sender Authentication

```toml
[channels.telegram]
enabled = true
bot_token_env = "TELEGRAM_BOT_TOKEN"  # env var name holding the token
allowed_sender_ids = ["12345678"]     # Telegram user IDs (not chat IDs)
```

**Default-deny:** Messages from sender IDs not in the allowlist are silently dropped with an audit log entry. No error response is sent to unauthorized senders (prevents enumeration).

### Telegram-Specific Commands

| Command | Action |
|---------|--------|
| `/new` | Create a new session in this chat |
| `/sessions` | List recent sessions |
| `/switch <id>` | Switch to a different session |
| `/cancel` | Cancel the active turn |
| `/status` | Show agent status and current session |

These are intercepted by the adapter before they reach the LLM. Regular messages are forwarded as turns.

---

## 6. Multi-Agent Orchestration

### Design Approach: Start Simple, Extend Later

The guidebook describes an ambitious state graph router. We start with the minimum viable orchestration:

1. **Agent definitions** — multiple `AgentConfig` sections in config, each with a name, system prompt, model, tool subset.
2. **Subagent spawning** — the parent agent's LLM calls a `delegate_to_agent` tool, which creates a child session with the target agent's config.
3. **Bounded handoffs** — `SubagentBrief` provides goal + key facts, not full conversation transcript.
4. **Result return** — subagent output is returned to parent as a tool result.

State graphs and declarative routing come later.

### Agent Registry

```toml
# Default agent (used when no agent is specified)
[agents.default]
system_prompt_file = "SYSTEM.md"
# inherits global selection.provider, selection.model

# Additional agent definitions
[agents.researcher]
system_prompt = "You are a research specialist..."
selection.provider = "anthropic"
selection.model = "claude-sonnet-4-20250514"
tools = ["web_search", "web_fetch", "file_read", "file_write", "memory_search", "memory_save"]
max_turns = 15
max_cost = 1.0

[agents.coder]
system_prompt = "You are a coding specialist..."
selection.provider = "anthropic"
selection.model = "claude-sonnet-4-20250514"
tools = ["file_read", "file_write", "file_edit", "file_list", "file_search", "shell_exec"]
max_turns = 20
max_cost = 2.0
```

### Agent Registry Types

```rust
/// Defined in types/src/config.rs
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDefinition {
    /// System prompt (inline or file reference)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt_file: Option<String>,
    /// Override provider/model for this agent
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<ProviderSelection>,
    /// Allowlist of tool names (None = all tools)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    /// Override runtime limits
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost: Option<f64>,
}
```

### SubagentBrief

```rust
/// Defined in types crate
pub struct SubagentBrief {
    /// Target agent name from the registry
    pub agent_name: String,
    /// What the subagent should accomplish
    pub goal: String,
    /// Key facts from parent context
    pub key_facts: Vec<String>,
    /// Maximum turns for this delegation
    pub max_turns: u32,
    /// Maximum cost budget
    pub max_cost: f64,
    /// How the subagent should format its result
    pub expected_output: OutputFormat,
}

pub enum OutputFormat {
    /// Free-form text response
    Text,
    /// Structured JSON conforming to a schema
    Json { schema_hint: String },
    /// Just the final file paths modified
    FileSummary,
}
```

### Delegation Tool

A single LLM-callable tool named `delegate_to_agent`:

```rust
/// Delegates a task to a specialized agent.
///
/// The agent will execute independently with its own context and return
/// a result. Available agents: {agent_list_from_config}
#[tool]
pub async fn delegate_to_agent(
    /// Name of the agent to delegate to
    agent_name: String,
    /// Clear, self-contained description of what the agent should do
    goal: String,
    /// Key facts the agent needs to know (context from this conversation)
    key_facts: Vec<String>,
    /// Maximum turns the agent can take (default: agent's configured max)
    max_turns: Option<u32>,
) -> String {
    // Implementation spawns a subagent session
}
```

### Subagent Execution Flow

```
Parent Agent (session-alice-abc)
    │
    ├── LLM calls delegate_to_agent("researcher", "Find papers on X", [...])
    │
    ▼
Gateway creates subagent session:
    session_id: "subagent:session-alice-abc:researcher:def456"
    parent_session_id: "session-alice-abc"
    agent_config: agents.researcher
    │
    ├── Constructs child AgentRuntime with researcher's config
    ├── Creates child CancellationToken (linked to parent)
    ├── Sets budget from SubagentBrief (capped by parent's remaining budget)
    │
    ▼
Subagent executes independently:
    │
    ├── Own system prompt (researcher persona)
    ├── Own tool registry (subset)
    ├── Own conversation context (starts with goal + key_facts)
    ├── Same workspace (/shared, /tmp, /vault)
    ├── Same memory namespace (user_id scoped)
    │
    ▼
On completion/failure/budget-exhaustion:
    │
    ├── Result returned to parent as tool result message
    ├── Subagent session archived
    └── Parent continues its turn
```

### Budget Cascading

```
Parent budget remaining: $5.00
  │
  ├── Subagent requested: max_cost = $2.00
  │   ├── Actual: min(requested, parent_remaining) = $2.00
  │   └── Parent remaining after allocation: $3.00
  │
  ├── Subagent completes, used $1.50
  │   └── $0.50 returned to parent → parent remaining: $3.50
  │
  └── Parent continues with $3.50
```

### Progress Streaming During Delegation

While a subagent is executing, the parent session's subscribers see progress:

```
GatewayServerFrame::TurnProgress {
    kind: RuntimeProgressKind::SubagentExecution {
        agent_name: "researcher",
        subagent_session_id: "subagent:...",
    },
    message: "[delegated] researcher: Searching for papers... (turn 3/15)"
}
```

This keeps the user informed during potentially long delegations.

---

## 7. Gateway Evolution

### New Gateway Architecture

The gateway is the component that changes the most. Here is the evolved design:

```
┌─────────────────────────────────────────────────────────────────┐
│                        GatewayServer                            │
│                                                                 │
│  users: RwLock<HashMap<UserId, Arc<UserState>>>                │
│                                                                 │
│  ┌─────────────────────────────────────────────────────────┐   │
│  │ UserState                                                │   │
│  │  ├── user_id: String                                     │   │
│  │  ├── sessions: RwLock<HashMap<SessionId, Arc<SessionState>>>│ │
│  │  ├── concurrent_turns: AtomicU32                         │   │
│  │  └── max_concurrent_turns: u32                           │   │
│  └─────────────────────────────────────────────────────────┘   │
│                                                                 │
│  ┌─────────────────────────────────────────────────────────┐   │
│  │ SessionState                                             │   │
│  │  ├── session_id: String                                  │   │
│  │  ├── user_id: String                                     │   │
│  │  ├── parent_session_id: Option<String>  (subagent link)  │   │
│  │  ├── agent_name: String  ("default" or named agent)      │   │
│  │  ├── created_at: Instant                                 │   │
│  │  ├── active_turn: Mutex<Option<ActiveTurnState>>         │   │
│  │  ├── events: broadcast::Sender<GatewayServerFrame>       │   │
│  │  ├── latest_terminal_frame: Mutex<Option<...>>           │   │
│  │  └── subscriber_count: AtomicU32                         │   │
│  └─────────────────────────────────────────────────────────┘   │
│                                                                 │
│  ┌─────────────────────────────────────────────────────────┐   │
│  │ ConnectionState (per WebSocket / per Telegram chat)      │   │
│  │  ├── connection_id: String                               │   │
│  │  ├── user_id: String                                     │   │
│  │  ├── channel_id: String                                  │   │
│  │  ├── active_session_id: String  (which session this      │   │
│  │  │                               connection is viewing)  │   │
│  │  └── subscription: broadcast::Receiver<...>              │   │
│  └─────────────────────────────────────────────────────────┘   │
│                                                                 │
│  channel_adapters: ChannelRegistry                              │
│  turn_runners: HashMap<String, Arc<dyn GatewayTurnRunner>>      │
│     └── keyed by agent_name                                     │
└─────────────────────────────────────────────────────────────────┘
```

### Connection Lifecycle

```
1. WebSocket connects (TUI) or Telegram message arrives
2. Hello/auth → identify user_id and channel_id
3. Session resolution:
   a. Explicit session_id provided → join that session
   b. create_new_session flag → generate new session
   c. Neither → find or create default session for this (user, channel, connection)
4. Connection subscribes to session's broadcast channel
5. Turns, deltas, progress flow through the session's broadcast
6. /new → unsubscribe from current session, create new, subscribe to new
7. Disconnect → decrement subscriber_count; session remains alive
```

### Multi-Agent Turn Runner Registry

Today: one `RuntimeGatewayTurnRunner` for the whole gateway.

New: a registry of turn runners, one per agent definition:

```rust
pub struct MultiAgentGateway {
    // Base turn runner (for "default" agent)
    default_turn_runner: Arc<dyn GatewayTurnRunner>,
    // Named agent turn runners
    agent_runners: HashMap<String, Arc<dyn GatewayTurnRunner>>,
}
```

Each `RuntimeGatewayTurnRunner` holds its own `Arc<AgentRuntime>` with the agent-specific provider, tools, limits, and system prompt.

### Channel Adapter Integration

The gateway gains a `register_channel` method and a channel listener loop:

```rust
impl GatewayServer {
    /// Register a channel adapter and start listening for inbound events.
    pub async fn register_channel(&self, channel_id: String, adapter: Arc<dyn Channel>) {
        // 1. Register in channel registry
        // 2. Spawn a tokio task that calls adapter.listen()
        // 3. For each ChannelInboundEvent:
        //    a. Validate sender
        //    b. Resolve session
        //    c. Route to turn execution
    }
}
```

This keeps the gateway as the single routing point — all channels (TUI via WebSocket, Telegram via adapter) converge at the gateway.

---

## 8. Configuration Design

### Full Configuration Schema

```toml
# ============================================================
# agent.toml — Full configuration with multi-agent and channels
# ============================================================

config_version = "1.0.0"

# ── Provider Selection (default agent) ──────────────────────
[selection]
provider = "anthropic"
model = "claude-sonnet-4-20250514"

# ── Provider Registry (unchanged) ───────────────────────────
[providers.registry.anthropic]
provider_type = "anthropic"
api_key_env = "ANTHROPIC_API_KEY"

[providers.registry.openai]
provider_type = "openai"
api_key_env = "OPENAI_API_KEY"

# ── Runtime (default limits) ────────────────────────────────
[runtime]
turn_timeout_secs = 120
max_turns = 15
max_cost = 5.0

# ── Gateway ─────────────────────────────────────────────────
[gateway]
max_sessions_per_user = 10
max_concurrent_turns_per_user = 3
session_idle_ttl_hours = 24

# ── Channels ────────────────────────────────────────────────
[channels.telegram]
enabled = false                            # default: disabled
bot_token_env = "TELEGRAM_BOT_TOKEN"       # env var name
polling_timeout_secs = 30                  # long-polling timeout
allowed_sender_ids = []                    # Telegram user IDs
max_message_length = 4096                  # split longer responses
rejection_mode = "silent"                  # "silent" or "respond"

# ── Agent Definitions ──────────────────────────────────────
# The "default" agent uses the top-level selection/runtime.
# Named agents override specific fields.

[agents.researcher]
system_prompt = "You are a research specialist. Search the web, read papers, and synthesize findings."
selection.provider = "anthropic"
selection.model = "claude-sonnet-4-20250514"
tools = ["web_search", "web_fetch", "file_read", "file_write", "memory_search", "memory_save"]
max_turns = 15
max_cost = 1.5

[agents.coder]
system_prompt = "You are a coding specialist. Write, review, and test code."
selection.provider = "anthropic"
selection.model = "claude-sonnet-4-20250514"
tools = ["file_read", "file_write", "file_edit", "file_list", "file_search", "file_delete", "shell_exec"]
max_turns = 25
max_cost = 3.0

# ── Scheduler (unchanged) ──────────────────────────────────
[scheduler]
enabled = true

# ── Memory (unchanged) ─────────────────────────────────────
[memory]
enabled = true
```

### Configuration Types

```rust
// New in types/src/config.rs

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GatewayConfig {
    #[serde(default = "default_max_sessions_per_user")]
    pub max_sessions_per_user: usize,        // default: 10
    #[serde(default = "default_max_concurrent_turns_per_user")]
    pub max_concurrent_turns_per_user: usize, // default: 3
    #[serde(default = "default_session_idle_ttl_hours")]
    pub session_idle_ttl_hours: u64,          // default: 24
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ChannelsConfig {
    #[serde(default)]
    pub telegram: TelegramChannelConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TelegramChannelConfig {
    #[serde(default)]
    pub enabled: bool,                        // default: false
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_token_env: Option<String>,        // env var name
    #[serde(default = "default_telegram_polling_timeout")]
    pub polling_timeout_secs: u64,            // default: 30
    #[serde(default)]
    pub allowed_sender_ids: Vec<String>,      // Telegram user IDs
    #[serde(default = "default_telegram_max_message_length")]
    pub max_message_length: usize,            // default: 4096
    #[serde(default = "default_telegram_rejection_mode")]
    pub rejection_mode: RejectionMode,        // default: Silent
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectionMode {
    Silent,
    Respond,
}

// AgentConfig gains:
pub struct AgentConfig {
    // ... existing fields ...
    #[serde(default)]
    pub gateway: GatewayConfig,
    #[serde(default)]
    pub channels: ChannelsConfig,
    #[serde(default)]
    pub agents: BTreeMap<String, AgentDefinition>,
}
```

---

## 9. Database Migrations

### New Tables

```sql
-- Migration: 0020_create_session_registry.sql
-- Provides a gateway-level session registry for listing/searching sessions.
-- The existing `sessions` table in memory is for conversation event storage;
-- this table tracks gateway-level session metadata.

CREATE TABLE IF NOT EXISTS gateway_sessions (
    session_id      TEXT PRIMARY KEY,
    user_id         TEXT NOT NULL,
    agent_name      TEXT NOT NULL DEFAULT 'default',
    channel_origin  TEXT NOT NULL DEFAULT 'tui',
    display_name    TEXT,                    -- optional user-given name
    parent_session_id TEXT,                  -- NULL for root sessions
    created_at      TEXT NOT NULL DEFAULT (datetime('now')),
    last_active_at  TEXT NOT NULL DEFAULT (datetime('now')),
    archived        INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_gateway_sessions_user
    ON gateway_sessions(user_id, archived, last_active_at DESC);

CREATE INDEX IF NOT EXISTS idx_gateway_sessions_parent
    ON gateway_sessions(parent_session_id)
    WHERE parent_session_id IS NOT NULL;
```

```sql
-- Migration: 0021_create_channel_session_mappings.sql
-- Maps (user_id, channel_id, channel_context_id) → session_id.
-- For TUI: channel_context_id = connection_id.
-- For Telegram: channel_context_id = chat_id.

CREATE TABLE IF NOT EXISTS channel_session_mappings (
    user_id             TEXT NOT NULL,
    channel_id          TEXT NOT NULL,
    channel_context_id  TEXT NOT NULL,
    session_id          TEXT NOT NULL,
    created_at          TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (user_id, channel_id, channel_context_id)
);
```

```sql
-- Migration: 0022_create_sender_audit_log.sql
-- Audit log for rejected sender attempts on external channels.

CREATE TABLE IF NOT EXISTS sender_audit_log (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    channel_id      TEXT NOT NULL,
    sender_id       TEXT NOT NULL,
    rejected_at     TEXT NOT NULL DEFAULT (datetime('now')),
    reason          TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_sender_audit_channel
    ON sender_audit_log(channel_id, rejected_at DESC);
```

---

## 10. Protocol Evolution

### New Client Frames

```rust
pub enum GatewayClientFrame {
    Hello(GatewayClientHello),       // MODIFIED: add session_id, create_new_session
    SendTurn(GatewaySendTurn),       // unchanged
    CancelActiveTurn(...),           // unchanged
    HealthCheck(...),                // unchanged

    // NEW: Session management commands
    NewSession(GatewayNewSession),
    ListSessions(GatewayListSessions),
    SwitchSession(GatewaySwitchSession),
}

pub struct GatewayNewSession {
    pub request_id: String,
    pub display_name: Option<String>,    // optional label
    pub agent_name: Option<String>,      // default → "default"
}

pub struct GatewayListSessions {
    pub request_id: String,
    pub include_archived: bool,
}

pub struct GatewaySwitchSession {
    pub request_id: String,
    pub session_id: String,
}
```

### New Server Frames

```rust
pub enum GatewayServerFrame {
    // ... existing frames ...

    // NEW: Session lifecycle responses
    SessionCreated(GatewaySessionCreated),
    SessionList(GatewaySessionList),
    SessionSwitched(GatewaySessionSwitched),

    // NEW: Subagent progress (extends existing TurnProgress)
    SubagentProgress(GatewaySubagentProgress),
}

pub struct GatewaySessionCreated {
    pub request_id: String,
    pub session: GatewaySession,         // the new session
}

pub struct GatewaySessionList {
    pub request_id: String,
    pub sessions: Vec<SessionSummary>,
}

pub struct SessionSummary {
    pub session_id: String,
    pub agent_name: String,
    pub display_name: Option<String>,
    pub channel_origin: String,
    pub created_at: String,
    pub last_active_at: String,
    pub archived: bool,
    pub has_active_turn: bool,
}

pub struct GatewaySessionSwitched {
    pub request_id: String,
    pub previous_session: GatewaySession,
    pub new_session: GatewaySession,
}

pub struct GatewaySubagentProgress {
    pub request_id: String,
    pub session: GatewaySession,          // parent session
    pub turn: GatewayTurnStatus,          // parent turn
    pub agent_name: String,
    pub subagent_session_id: String,
    pub message: String,
}
```

### Protocol Version

Bump `GATEWAY_PROTOCOL_VERSION` from `1` to `2`. The gateway accepts both v1 and v2 clients:

- v1 clients: behave exactly as today (single session per user, no session commands)
- v2 clients: get multi-session support

The TUI sends v2 in its `Hello` frame. Old TUI binaries connecting to a new gateway continue to work because v1 still creates a default session.

---

## 11. Implementation Phases

### Phase A: Gateway Session Foundation (No External Changes Yet)

**Goal:** Replace single-session-per-user with multi-session model. Existing TUI continues to work identically.

**Crates touched:** `types`, `gateway`, `memory`

1. Add `GatewayConfig` to `AgentConfig` with defaults.
2. Add `gateway_sessions` migration (0020).
3. Replace `HashMap<String, Arc<UserSessionState>>` with `HashMap<String, Arc<UserState>>` where `UserState` contains `HashMap<String, Arc<SessionState>>`.
4. Update `resolve_session()` to handle multi-session Hello logic.
5. Move `active_turn` from user-level to session-level (it's already conceptually there, just formalize).
6. Add `concurrent_turns` counter on `UserState` with `max_concurrent_turns_per_user` enforcement.
7. Implement session creation, including `gateway_sessions` table persistence.
8. Existing TUI works unchanged — v1 Hello creates a default session, same behavior.

**Tests:**
- Multiple sessions for same user can run turns concurrently
- v1 Hello backward compatibility
- Session limit enforcement
- Concurrent turn limit enforcement

### Phase B: Session Lifecycle Commands

**Goal:** `/new`, `/sessions`, `/switch` support in the gateway protocol.

**Crates touched:** `types`, `gateway`, `tui`

1. Add `NewSession`, `ListSessions`, `SwitchSession` client frames.
2. Add `SessionCreated`, `SessionList`, `SessionSwitched` server frames.
3. Implement gateway handlers for each.
4. Add `channel_session_mappings` migration (0021).
5. Update `GatewayClientHello` with `session_id` and `create_new_session` fields (backward-compatible via `serde(default)`).
6. Bump `GATEWAY_PROTOCOL_VERSION` to 2 (accept both 1 and 2).
7. TUI: add `--session <id>` CLI flag.
8. TUI: intercept `/new`, `/sessions`, `/switch` before sending as turns; convert to protocol frames.
9. TUI: update status bar to show session ID.

**Tests:**
- `/new` creates a new session and switches to it
- `/sessions` lists all user sessions
- `/switch` changes the connection's active session
- Two TUI instances with different session IDs run independently
- Session persistence across gateway restart (via `gateway_sessions` table)

### Phase C: Telegram Channel Adapter

**Goal:** Working Telegram bot that can receive messages, route them through the gateway, and send responses.

**Crates touched:** `types`, `channels`, `gateway`, `runner`

1. Add `TelegramChannelConfig` to `AgentConfig`.
2. Add `teloxide` dependency to `channels` crate (not feature-flagged).
3. Implement `TelegramAdapter` in `channels/src/telegram.rs`:
   - `Channel` trait implementation
   - Long-polling update listener
   - Telegram → `ChannelInboundEvent` conversion
   - `ChannelOutboundEvent` → Telegram `send_message` conversion
   - Message splitting for long responses
   - `/new`, `/sessions`, `/switch`, `/cancel`, `/status` command interception
4. Add `sender_audit_log` migration (0022).
5. Implement sender authentication in gateway:
   - `ChannelSenderAuthPolicy` with per-channel allowlists
   - Rejection audit logging
6. Gateway: add `register_channel()` and channel listener loop.
7. Gateway: implement channel session mapping — `(user_id, telegram, chat_id) → session_id`.
8. Runner bootstrap: conditionally construct and register Telegram adapter when `channels.telegram.enabled = true`.

**Tests:**
- Telegram adapter converts messages correctly (unit tests with mock bot)
- Sender authentication rejects unauthorized users
- Audit logging captures rejections
- Long message splitting
- Session mapping persistence
- `/new` in Telegram creates a new session

### Phase D: Multi-Agent Orchestration

**Goal:** Parent agent can delegate to named subagents and receive results.

**Crates touched:** `types`, `tools`, `runtime`, `gateway`, `runner`

1. Add `AgentDefinition` config type and `agents` map to `AgentConfig`.
2. Add `SubagentBrief` and `OutputFormat` types.
3. Implement `delegate_to_agent` tool:
   - Validates agent name against registry
   - Constructs `SubagentBrief` from tool arguments
   - Creates subagent session (session_id with `subagent:` prefix)
   - Builds child `AgentRuntime` with agent-specific config
   - Creates child `CancellationToken`
   - Executes subagent turn loop
   - Returns result to parent as tool output
4. Gateway: add `SubagentProgress` frame for progress streaming during delegation.
5. Runner bootstrap: build agent-specific turn runners from config.
6. Budget cascading: subagent budget deducted from parent remaining.
7. Register `delegate_to_agent` tool when `agents` config has entries (beyond "default").

**Tests:**
- Subagent spawning with correct config inheritance
- Budget cascading prevents exceeding parent budget
- Parent cancellation cascades to child
- Subagent timeout enforcement
- Progress streaming during delegation
- Tool subset enforcement for subagents

### Phase E: Polish and Integration

**Goal:** End-to-end reliability, cross-feature interactions, documentation.

1. Session TTL enforcement (background cleanup task).
2. Telegram reconnection and error recovery.
3. Subagent concurrent execution (fan-out pattern for future).
4. System prompt augmentation with available agent descriptions.
5. Update guidebook chapters 9, 11, 12, 14, 15.
6. Integration tests covering:
   - TUI + Telegram simultaneous use
   - Multi-agent delegation from Telegram session
   - Session switching mid-conversation
   - Scheduled tasks alongside interactive sessions

---

## 12. Testing Strategy

### Unit Tests

| Area | Tests |
|------|-------|
| **Session model** | Session create, join, list, switch, archive; concurrent turn limits; session TTL |
| **Protocol** | v1/v2 Hello compatibility; new frame serialization round-trips |
| **Telegram adapter** | Message conversion, command interception, message splitting, rate limiting |
| **Sender auth** | Allowlist validation, rejection audit, silent vs. respond modes |
| **Agent registry** | Config parsing, tool subset validation, missing agent errors |
| **SubagentBrief** | Serialization, budget validation, output format |
| **Delegation tool** | Argument parsing, agent resolution, budget cascading |

### Integration Tests

| Scenario | Verification |
|----------|-------------|
| **Two TUI sessions** | Both can run turns concurrently; messages don't cross-contaminate |
| **Telegram e2e** | Message received → turn executed → response sent back (mock Telegram API) |
| **Session persistence** | Create session, restart gateway, session still queryable |
| **Subagent delegation** | Parent delegates, child executes, result returned as tool output |
| **Budget enforcement** | Subagent cannot exceed parent's remaining budget |
| **Cancellation cascade** | Parent cancelled → child cancelled |
| **Concurrent turn limit** | Third turn rejected when `max_concurrent_turns_per_user = 2` |

### Mock Strategy

- **Mock Telegram API:** A small axum server that mimics `api.telegram.org` for adapter tests. Alternatively, mock the `teloxide::Bot` at the trait boundary.
- **Mock Provider:** Existing `MockProvider` used for agent runtime tests.
- **Mock Memory:** In-memory libSQL for session persistence tests.

---

## 13. UX Design

### TUI Experience

```
┌─ Oxydra ─────────────────────────────────────────────────────────┐
│                                                                   │
│  you: Analyze the performance of our API endpoints                │
│                                                                   │
│  assistant: I'll analyze the API performance data...              │
│  [delegated → researcher] Searching for performance metrics...    │
│  [delegated → researcher] Found 3 relevant datasets...           │
│  The analysis shows the following...                              │
│                                                                   │
├─ Prompt (session: research-abc | agent: default) ────────────────┤
│  █                                                                │
├──────────────────────────────────────────────────────────────────┤
│  🟢 Connected │ session-alice-abc123 │ idle │ [/new] [/sessions]  │
└──────────────────────────────────────────────────────────────────┘
```

**Status bar changes:**
- Session ID shortened for display (last 8 chars or display name)
- `/new` and `/sessions` hints shown when idle
- Delegation progress shown inline with `[delegated → agent_name]` prefix

### Telegram Experience

```
User: Analyze the performance of our API endpoints

Bot: I'll analyze the API performance data...

[⏳ Delegating to researcher agent...]

Bot: The analysis shows the following:
1. GET /users: avg 45ms, p99 120ms
2. POST /orders: avg 180ms, p99 450ms
...

User: /new

Bot: ✅ Started a new conversation (session-abc123)

User: /sessions

Bot: Your sessions:
• session-abc123 (current) — just now
• session-xyz789 — 2h ago — "API Performance Analysis"
• session-def456 — 1d ago — "Database Migration"
```

### Command UX Consistency

| Command | TUI Behavior | Telegram Behavior |
|---------|-------------|-------------------|
| `/new` | Creates new session, clears chat area, updates status bar | Sends confirmation message, subsequent messages go to new session |
| `/sessions` | Shows session list in chat area (system message) | Sends formatted session list |
| `/switch <id>` | Switches session, loads recent history if available | Sends confirmation, subsequent messages go to switched session |
| `/cancel` | Cancels active turn (same as Ctrl+C) | Cancels active turn, sends confirmation |
| `/status` | Shows in status bar (already there) | Sends formatted status message |

---

## 14. Risk Analysis

### Technical Risks

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| **Protocol version migration** | Medium | Medium | v1/v2 backward compat; staged rollout (Phase A is invisible to clients) |
| **Telegram rate limits** | Low | Low | Accumulate deltas before sending; respect Telegram's 30 msg/sec limit |
| **Memory contention** | Medium | Medium | Per-session isolation; `concurrent_turns` limit prevents runaway |
| **Gateway complexity** | High | High | Strong session/connection separation; comprehensive integration tests |
| **Subagent infinite loops** | Low | High | Budget cascading; max_turns enforcement; parent cancellation cascade |
| **Session leak** | Medium | Medium | TTL-based cleanup; subscriber counting; periodic GC task |

### Crate Boundary Integrity

All changes respect the existing crate dependency graph:

```
types ← provider, tools, tools-macros, memory, runtime, channels, gateway, tui, runner
                    ↑                      ↑        ↑          ↑
                    tools ← runtime    runtime ← gateway ← runner
                                                     ↑
                                              channels ← runner
                                                     ↑
                                                   tui (standalone binary)
```

**No new crate-level dependencies introduced.** The `channels` crate gains the `teloxide` dependency. The `gateway` crate gains an indirect dependency on channels (via the runner, which constructs and passes adapters). The runtime crate does not depend on gateway — the `ScheduledTurnRunner` and `GatewayTurnRunner` traits maintain the boundary.

### What We're NOT Doing (Explicitly Deferred)

1. **State graph routing** — Declarative graph definitions for complex workflows (Chapter 11). Too complex for v1; start with simple delegation.
2. **Cross-agent communication** — Agents talking to each other without a parent coordinator. Covered by delegating through the parent.
3. **Webhook mode for Telegram** — Long polling first; webhook requires public URL and SSL. Can be added later without changing the adapter interface.
4. **Slack/Discord/WhatsApp adapters** — Same `Channel` trait; just different adapter implementations. Trivially addable after Telegram proves the pattern.
5. **Session migration across channels** — A session started in TUI can be continued in Telegram. Deferred because it requires cross-channel context sync and is a UX challenge.
6. **Hot channel reload** — Adding/removing channels without gateway restart. Requires config watching.

---

## Appendix A: Dependency Summary

### New Crate Dependencies

| Crate | New Dependency | Version | Purpose |
|-------|---------------|---------|---------|
| `channels` | `teloxide` | `0.13` | Telegram bot framework |
| `channels` | `tokio` (full features) | `1` | Async runtime for adapter |
| `channels` | `tracing` | `0.1` | Structured logging |
| `types` | `uuid` | `1` (features: `v7`) | Time-ordered session IDs |

### File Change Summary

| File | Change Type |
|------|------------|
| `crates/types/src/config.rs` | Modified — add `GatewayConfig`, `ChannelsConfig`, `TelegramChannelConfig`, `AgentDefinition` |
| `crates/types/src/channel.rs` | Modified — add session lifecycle frames, modify `GatewayClientHello` |
| `crates/types/src/error.rs` | Modified — add channel auth error variants |
| `crates/types/src/lib.rs` | Modified — re-export new types |
| `crates/channels/src/lib.rs` | Modified — re-export telegram module |
| `crates/channels/src/telegram.rs` | **New** — Telegram adapter |
| `crates/channels/Cargo.toml` | Modified — add teloxide, tokio, tracing deps |
| `crates/gateway/src/lib.rs` | **Major rewrite** — multi-session model, channel integration |
| `crates/gateway/src/turn_runner.rs` | Modified — multi-agent turn runner registry |
| `crates/gateway/src/session.rs` | **New** — SessionState, UserState, session lifecycle |
| `crates/gateway/src/channel_router.rs` | **New** — channel inbound routing, sender auth |
| `crates/memory/migrations/0020_*.sql` | **New** — gateway_sessions table |
| `crates/memory/migrations/0021_*.sql` | **New** — channel_session_mappings table |
| `crates/memory/migrations/0022_*.sql` | **New** — sender_audit_log table |
| `crates/memory/src/lib.rs` | Modified — session registry queries |
| `crates/tools/src/delegation_tools.rs` | **New** — delegate_to_agent tool |
| `crates/runtime/src/lib.rs` | Modified — subagent spawning support |
| `crates/runner/src/bootstrap.rs` | Modified — multi-agent bootstrap, channel registration |
| `crates/runner/src/bin/oxydra-vm.rs` | Modified — channel adapter startup |
| `crates/tui/src/bin/oxydra-tui.rs` | Modified — `--session` flag |
| `crates/tui/src/app.rs` | Modified — `/new`, `/sessions`, `/switch` handling |
| `crates/tui/src/channel_adapter.rs` | Modified — session lifecycle frame handling |
| `crates/tui/src/ui_model.rs` | Modified — session info in view model |
| `crates/tui/src/widgets.rs` | Modified — session info in status bar |

---

## Appendix B: Key Design Decisions Record

### Decision 1: No Feature Flag for Telegram

**Chosen:** Compile always, enable via config.
**Rationale:** Feature flags add CI complexity (testing N feature combinations). The `teloxide` dependency is small. Config-based enable/disable is simpler for users and CI.
**Trade-off:** Slightly larger binary when Telegram is disabled. Acceptable for a personal agent tool.

### Decision 2: Long Polling Over Webhooks (Initial)

**Chosen:** Long polling with `teloxide`.
**Rationale:** No public URL or SSL setup required. Perfect for personal/home-server use. Webhook can be added later via config switch (same adapter, different listener).
**Trade-off:** Slightly higher latency (polling interval). Acceptable for personal assistant use.

### Decision 3: UUID v7 for Session IDs

**Chosen:** UUID v7 (time-ordered) over UUID v4 (random).
**Rationale:** Natural time-ordering makes "most recent sessions" queries trivial (lexicographic sort = temporal sort). Human-readable timestamps embedded in the ID aid debugging.
**Trade-off:** Requires `uuid` crate with `v7` feature. Already used in TUI for connection IDs (`v4`).

### Decision 4: Session per Connection (Not Global Default)

**Chosen:** Each new TUI instance gets a new session by default.
**Rationale:** Matches Claude Code / ChatGPT behavior. Users expect a fresh conversation when opening a new window. Explicit `--session <id>` for joining existing sessions.
**Trade-off:** Users who want "one session" must use `--session`. This is the minority case and easy to support.

### Decision 5: Simple Delegation Tool Over State Graphs

**Chosen:** Single `delegate_to_agent` tool, no declarative state graphs.
**Rationale:** Start simple; state graphs are a v2 feature. The `delegate_to_agent` tool gives the LLM control over when and what to delegate, which is more flexible than static graphs for initial use.
**Trade-off:** No automatic routing or intent classification. The parent LLM must decide when to delegate. This is actually desirable for transparency.

### Decision 6: Keep `runtime_session_id` Field Name

**Chosen:** Do NOT rename `runtime_session_id` to `session_id` in the existing protocol types.
**Rationale:** The field `runtime_session_id` appears in 150 references across 15 files (`GatewaySession`, `GatewaySendTurn`, `GatewayCancelActiveTurn`, `GatewayHelloAck`, `TuiUiState`, `RuntimeGatewayTurnRunner`, etc.). Renaming would cause a massive blast radius with no functional benefit. The new session lifecycle frames (`NewSession`, `ListSessions`, `SwitchSession`) use `session_id` as the user-facing term, but the internal protocol field retains `runtime_session_id` for backward compatibility. The `GatewayClientHello` gains an additional optional `session_id` field (for joining existing sessions) which maps to `runtime_session_id` internally.
**Trade-off:** Slight naming inconsistency between user-facing "session ID" and protocol-level "runtime_session_id". Acceptable; the protocol is internal.

### Decision 7: Subagent Execution In-Process

**Chosen:** Subagents run in the same `oxydra-vm` process, different `AgentRuntime` instances.
**Rationale:** Subagents share the same workspace and memory database. Spawning separate processes would require IPC for file/memory access. In-process is simpler and faster.
**Trade-off:** A subagent panic could crash the parent. Mitigated by `catch_unwind` (already used in gateway turn execution).
