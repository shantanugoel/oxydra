# Final Plan: External Channels + Multi-Agent + Session Lifecycle + Auth/Identity

**Date:** 2026-02-25
**Status:** Consolidated plan, codebase review, and auth/onboarding analysis.
**Guidebook phases covered:** 14 (External Channels + Identity), 15 (Multi-Agent), 18 (Session Lifecycle)
**Backward compatibility:** NOT required. All clients (TUI, adapters) are updated together. No v1 support.

---

## Table of Contents

1. [Design Decisions & Arbitrations](#1-design-decisions--arbitrations)
2. [Auth, Identity & Onboarding Model](#2-auth-identity--onboarding-model)
3. [Codebase Ground Truth](#3-codebase-ground-truth)
4. [Target Architecture](#4-target-architecture)
5. [Implementation Steps](#5-implementation-steps)
6. [Database Migrations](#6-database-migrations)
7. [Protocol Evolution](#7-protocol-evolution)
8. [Configuration Schema](#8-configuration-schema)
9. [Testing Strategy](#9-testing-strategy)
10. [Risk Analysis & Mitigations](#10-risk-analysis--mitigations)

---

## 1. Design Decisions & Arbitrations

BACKWARD COMPATIBILITY IS NOT REQUIRED FOR ANY OF THE CHANGES BELOW!

Issues raised across the three plans, resolved with rationale grounded in the actual codebase.

### D0: Channels config is per-user; adapters run inside the VM

**Rationale:** `agent.toml` (→ `AgentConfig`) is guest-side config discovered from CWD and copied identically into every user's VM. Channels config is per-user and contains:
- Bot tokens (secrets that differ per user)
- Sender identity bindings (access control that differs per user)
- Per-user enable/disable toggles

These belong in `RunnerUserConfig` (each user's `config.toml`, already per-user). The runner includes the channels config in the `RunnerBootstrapEnvelope` sent to the VM, and forwards bot token env vars (same as it forwards `ANTHROPIC_API_KEY` today).

**Channel adapters run inside the VM** (same process as the gateway), not in the runner:
- Gateway is in the same process — direct function calls, no WebSocket overhead
- Adapter lifecycle matches VM lifecycle automatically — no separate management
- Each VM handles only its own user's bot — no multi-user routing complexity
- Follows the same pattern as provider, memory, scheduler — everything runs in the VM
- Bot tokens are same trust level as LLM API keys, which already enter the VM

**What remains outside the VM (host-side, in runner):**
- `RunnerUserConfig` with `channels` section — config source of truth
- `RunnerBootstrapEnvelope` carries channels config into the VM
- Bot token env var forwarding (runner reads `bot_token_env`, forwards the value)
- Everything else about channels (auth, adapters, session mapping, audit) runs inside the VM

### D1: Default session behavior

**Conflict:** Original plan said both "new session per TUI window" AND "resume most recent if session_id absent."

**Resolution:** Server behavior is deterministic. The _client_ decides what it wants:

- Send `create_new_session: true` in Hello for a fresh session
- Send `session_id: "<id>"` to join an existing one
- If neither is set, gateway creates a new session (no ambiguous resume heuristic)

**TUI default:** New TUI launch without `--session` sends `create_new_session: true`. Reconnection sends the `session_id` received from prior HelloAck.

### D2: Delegation tool crate boundary

**Conflict:** Plan places delegation in `tools` crate, but delegation needs runtime orchestration. `tools` cannot depend on `runtime` (dependency cycle).

**Resolution:** Same pattern as scheduler tools:
- Define `DelegationExecutor` trait in `types` crate (boundary-safe)
- Implement it in `runtime` crate
- `delegate_to_agent` tool in `tools` crate calls the trait via `Arc<dyn DelegationExecutor>` injected at registration time
- Runner wires the concrete implementation during bootstrap

**Actual dependency chain (unchanged):**
```
types ← tools (uses DelegationExecutor trait)
types ← runtime (implements DelegationExecutor)
runner wires tools + runtime together at bootstrap
```

### D3: Protocol version

**Resolution:** Bump `GATEWAY_PROTOCOL_VERSION` to `2`. No v1 support needed — all clients (TUI, adapters) are updated together.
- Remove the old `!= GATEWAY_PROTOCOL_VERSION` check
- Gateway rejects anything other than `2`
- All new frame types and `RuntimeProgressKind` variants are always emitted

### D4: Session persistence

**Conflict:** Gateway currently has zero persistence; plan assumes DB access.

**Resolution:**
- Define `SessionStore` trait in `types` crate (boundary-safe)
- Implement `LibsqlSessionStore` in `memory` crate (reuses existing libSQL connection infrastructure)
- Inject into gateway via constructor at bootstrap time (runner wires it)
- Update `memory/src/schema.rs` MIGRATIONS list AND `REQUIRED_TABLES`/`REQUIRED_INDEXES` arrays
- In-memory session state is authoritative for active sessions; DB is for listing/resumption after restart

### D5: Subagent progress transport

**Conflict:** Plan uses both `TurnProgress` extension AND separate `SubagentProgress` frame.

**Resolution:** Single path only. Extend existing `RuntimeProgressKind` with a new variant:
```rust
RuntimeProgressKind::SubagentExecution {
    agent_name: String,
    subagent_session_id: String,
}
```
Reuse existing `GatewayServerFrame::TurnProgress`. No new frame type.

### D6: Feature flag for Telegram (conflict between plan and guidebook)

**Conflict:** Original plan says no feature flag. Guidebook Ch.12 says feature-flagged.

**Resolution:** Feature-flag in `channels` crate. Rationale:
- `frankenstein` + `reqwest` pulls in dependencies (though most are already in Oxydra's tree)
- Feature flags are standard Rust practice for optional transports
- CI tests without the flag stay fast; feature-flag CI job tests with it
- Guidebook is the canonical source
- But add it to default features

```toml
# channels/Cargo.toml
[features]
telegram = ["dep:frankenstein", "dep:tokio"]
```

### D7: Tool execution context race

**Conflict:** `AgentRuntime` stores `ToolExecutionContext` in shared `Arc<Mutex<>>`. `set_tool_execution_context()` is called per-turn in `RuntimeGatewayTurnRunner`. With concurrent sessions/turns, context can be overwritten.

**Actual code path:**
```
RuntimeGatewayTurnRunner.run_turn() → runtime.set_tool_execution_context(ctx)
  ↓ (later, in tool_execution.rs)
let context = self.tool_execution_context.lock().await.clone();
```

If two turns call `set_tool_execution_context` back-to-back before either reaches the tool execution lock, the first turn uses the second turn's context. This IS a real race.

**Resolution:** Pass `ToolExecutionContext` through the `run_session_internal` call chain instead of storing it globally. The context becomes a parameter, not shared state:
- Add `tool_execution_context: ToolExecutionContext` parameter to `run_session_internal`
- Thread it through `execute_tool_call` → `execute_with_context`
- Remove the `Arc<Mutex<ToolExecutionContext>>` field from `AgentRuntime`
- Remove `set_tool_execution_context()` method
- `RuntimeGatewayTurnRunner.run_turn()` constructs context and passes it directly

### D8: Session lifetime and memory pressure

**Resolution:**
- Active session state (broadcast channels, turn locks) lives in memory
- Conversation history lives in DB (already the case via `conversation_events`)
- Gateway maintains a bounded in-memory session map; sessions are evicted from memory after `session_idle_ttl` with no subscribers
- Evicted sessions can be resumed by reloading from DB
- Gateway-level session registry (DB) tracks all sessions for listing/searching

### D9: `runtime_session_id` → `session_id` rename

**Resolution:** Since no backward compatibility is needed, rename `runtime_session_id` to `session_id` throughout the codebase. The field appears in ~150 references across ~15 files — this is a mechanical rename but worthwhile for clarity. The wire protocol uses `session_id` and internal code should match.

### D10: Internal Gateway API for channel adapters

**Problem:** The plan says channel adapters "call gateway methods directly," but `GatewayServer` only exposes WebSocket-facing methods (`handle_socket`, `handle_client_frame`). In-process adapters (Telegram, future Discord/Slack) need a programmatic API.

**Resolution:** Extract an internal API from `GatewayServer` during Step 3 (multi-session gateway core). The WebSocket handler becomes a thin wrapper around this API:

```rust
impl GatewayServer {
    // Internal API for in-process channel adapters
    pub async fn create_or_get_session(
        &self, user_id: &str, agent_name: &str, channel_origin: &str,
    ) -> Result<Arc<SessionState>, GatewayError>;

    pub async fn submit_turn(
        &self, session: &Arc<SessionState>, prompt: String,
    ) -> Result<broadcast::Receiver<GatewayServerFrame>, GatewayError>;

    pub async fn cancel_session_turn(
        &self, session: &Arc<SessionState>,
    ) -> Result<(), GatewayError>;

    pub async fn subscribe_events(
        &self, session: &Arc<SessionState>,
    ) -> broadcast::Receiver<GatewayServerFrame>;

    pub async fn list_user_sessions(
        &self, user_id: &str, include_archived: bool,
    ) -> Result<Vec<SessionRecord>, GatewayError>;
}
```

Both the WebSocket handler and the Telegram adapter call these same methods. This ensures identical behavior regardless of transport.

### D11: Subagent semaphore accounting

**Problem:** Step 11 originally said "Subagent turns also acquire parent user's permit." This creates a deadlock risk: with `max_concurrent_turns_per_user = 3` and max delegation depth = 3, a depth-3 chain consumes all permits, blocking any other concurrent session.

**Resolution:** Subagents run under the parent turn's permit. They are logically part of the parent's turn — the parent tool call synchronously awaits the result. The semaphore counts **top-level user-initiated turns only** (gateway `start_turn()` calls), not internal delegation executions. The delegation depth limit (max 3) and budget cascading already prevent fan-out abuse.

### D12: Scheduled tasks vs session model

**Problem:** Scheduled tasks create ad-hoc session IDs (`scheduled:{schedule_id}`) that are not tracked in the gateway's session map, not persisted in `gateway_sessions`, don't appear in `/sessions`, and bypass the concurrency semaphore.

**Resolution:** Scheduled tasks remain outside the gateway session model for now. They are system-initiated, not user-initiated, and should not compete with user permits. Document this explicitly:
- Scheduled task sessions are invisible to `/sessions`
- They do not count toward `max_sessions_per_user` or `max_concurrent_turns_per_user`
- Their conversation history is persisted via `conversation_events` (existing behavior)
- This may be revisited when scheduled tasks gain user-facing visibility

### D13: Channel trait evolution

**Problem:** Guidebook Chapter 9 defines a `Channel` trait with `send()`, `listen()`, `health_check()`. Chapter 12 says adapters implement this trait. But the plan's Telegram adapter doesn't implement this trait — it calls the gateway's internal API directly.

**Resolution:** The existing `Channel` trait is for **WebSocket-based client adapters** (TUI). In-process adapters (Telegram, future Discord/Slack) use the internal gateway API (D10) directly and do not implement `Channel`. The `Channel` trait may be deprecated or renamed to `WebSocketChannelClient` in a future cleanup pass. Document this distinction in the guidebook update (Step 12) so future contributors don't try to force-fit in-process adapters into the WebSocket client trait.

### D14: Thread/topic-based session mapping for external channels

**Problem:** The plan maps one Telegram chat to one session. Within a single chat, only one turn can be active, blocking the user from working on multiple tasks concurrently. This is the primary concurrency UX issue for external channels.

**Resolution:** Use platform-native threading constructs as separate sessions:

- **Telegram Forum Topics:** `channel_context_id = "{chat_id}:{message_thread_id}"` for forum groups, `channel_context_id = "{chat_id}"` for regular chats/DMs
- **Discord:** `channel_context_id = "{guild_id}:{channel_id}:{thread_id?}"`
- **WhatsApp:** Each community group thread gets its own `channel_context_id`

Each unique `(channel_id, channel_context_id)` maps to one session. This means:
- Each topic/thread = separate session = separate `active_turn` = true concurrency
- User creates Telegram topics like "Research", "Coding", "General" — each runs independently
- Within a single topic, "turn already active" still applies (natural: one thread, one conversation)
- Topic/thread names auto-populate `session.display_name` when available
- No protocol or gateway changes needed — purely in how the adapter derives `channel_context_id`

For Telegram, the adapter detects whether a group has forum mode enabled and adjusts the `channel_context_id` derivation accordingly. In DMs (no topics), the single-session behavior remains.

### D15: Edit-message streaming for external channels

**Problem:** The plan accumulates all `AssistantDelta` fragments and sends one final message on `TurnCompleted`. This means 2+ minutes of silence during complex turns — poor UX compared to the TUI's live streaming.

**Resolution:** External channel adapters use platform edit-message APIs to stream responses live:

1. Turn starts → adapter sends a placeholder message ("⏳ Working...")
2. `TurnProgress` arrives → edit message to show status ("🔍 Searching the web...", "🤖 Delegating to researcher...")
3. `AssistantDelta` tokens arrive → accumulate text, edit message every ~1.5 seconds with accumulated content
4. `TurnCompleted` → final edit with complete response (progress status lines removed)

**Telegram specifics:**
- `editMessageText()` rate limit is ~30 edits/minute per chat (1 edit/1.5s is safe)
- 4096 char limit per message; when approaching limit, stop editing current message, send a new continuation message, continue editing the new one
- Progress events shown as a status line at the top during processing, removed in final edit
- Throttle mechanism: `Instant::elapsed()` check before each edit, skip if < 1.5s since last edit

**Platform adaptation pattern:**
- Each adapter implements a `ResponseStreamer` that handles the platform-specific edit semantics
- Common logic: throttle interval, text accumulation, progress formatting
- Platform differences: edit API call, char limits, formatting rules

---

## 2. Auth, Identity & Onboarding Model

This is the missing piece from all three prior plans. The core question: _When a message arrives on Telegram/Discord/WhatsApp, how do we know who this sender is in Oxydra's world?_

### The Problem

| Channel | Identity Source | Trust Level |
|---------|----------------|-------------|
| TUI | `--user alice` CLI flag on local machine | High (local access implies authorization) |
| Telegram | Telegram user_id `12345678` | Must be verified |
| Discord | Discord user_id `123456789012345678` | Must be verified |
| WhatsApp | Phone number `+1234567890` | Must be verified |

On TUI, the user IS the operator — they have local machine access. On external channels, the sender is a remote platform identity that must be _bound_ to an Oxydra user before any processing happens.

### Solution: Pre-Configured Sender Binding

The operator (who has local access to `agent.toml`) configures which platform identities map to which Oxydra users and what role they have. This is the same trust model as:
- SSH `authorized_keys` (operator configures which keys are allowed)
- Tailscale ACLs (admin maps identities to permissions)
- IRC bot owner masks (configured in bot config)

```toml
# users/alice/config.toml (RunnerUserConfig — per-user, host-side)

[channels.telegram]
enabled = true
bot_token_env = "ALICE_TELEGRAM_BOT_TOKEN"

# Authorized senders — only these platform IDs can interact with alice's agent
[[channels.telegram.senders]]
platform_ids = ["12345678"]       # Telegram user ID (this IS alice on Telegram)

[[channels.telegram.senders]]
platform_ids = ["87654321", "11223344"]  # Bob has two Telegram accounts
display_name = "Bob"
```

Note: `user_id` is not in the sender binding — it's implicit from which user's config file this is in. The runner already knows the user_id from `global.toml`'s `[users]` map. All authorized senders are treated identically as the user.

### Sender Authorization Model

Binary decision: a sender is either **authorized** or **rejected**.

- **Authorized senders** (listed in the user's `channels.*.senders`): Messages are processed as normal user turns. The agent sees them as `MessageRole::User`, identical to TUI input.
- **Unknown senders** (not in the list): Rejected silently. Audit log entry created. No response sent (prevents enumeration).

All authorized senders are treated as the user — there is no role hierarchy or permission differentiation. If alice authorizes Bob's Telegram ID, Bob's messages are processed exactly as if alice typed them in the TUI.

### Group Channel Scenario (Discord)

When the bot is added to a Discord server/channel:

```toml
# users/alice/config.toml

[channels.discord]
enabled = true
bot_token_env = "ALICE_DISCORD_BOT_TOKEN"

# Which Discord channels (guild+channel IDs) the bot should listen to
allowed_channel_ids = ["1234567890123456"]

[[channels.discord.senders]]
platform_ids = ["111222333444555666"]  # Discord user ID (this is alice)

[[channels.discord.senders]]
platform_ids = ["666555444333222111"]  # Charlie — authorized to interact
display_name = "Charlie"
```

In a group channel:
- Messages from alice (111222333444555666) → processed as user turns
- Messages from Charlie (666555444333222111) → processed as user turns (same as alice)
- Messages from Dave (unknown) → silently dropped, audit logged

### Why Not Dynamic Onboarding?

For the initial implementation, we deliberately avoid invite-code or OAuth flows because:
1. They add attack surface (invite code leakage, phishing)
2. They require state management for pending invites
3. They're unnecessary for the primary use case (personal agent)
4. Pre-configured binding is zero-trust: only the operator with file system access can authorize senders

Dynamic onboarding (invite codes, admin commands) can be added later as an enhancement _on top of_ the static binding model.

### Config-Level Identity Types

```rust
// In types/src/runner.rs (per-user config, passed to VM via bootstrap envelope)

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SenderBinding {
    /// Platform-specific sender identifiers (Telegram user_id, Discord user_id, etc.)
    /// A single person may have multiple platform IDs.
    pub platform_ids: Vec<String>,
    /// Optional human-readable name for logging/audit
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}
// All authorized senders are treated as the user. No role differentiation.
```

### Runtime Identity Propagation

Channel adapters run inside the VM alongside the gateway. Sender auth happens before any message reaches the gateway's turn processing.

```
Platform message (Telegram user_id: 12345678)
    │
    ▼ (via frankenstein long-polling, inside the VM)
    │
Channel Adapter (in-process, same VM as gateway):
    ├── Extract sender platform_id from Telegram update
    ├── Look up platform_id in authorized sender set
    │     │
    │     ├── NOT FOUND → audit log + silent drop, done
    │     │
    │     └── FOUND → authorized, proceed
    │
    ▼
Adapter calls gateway directly (no WebSocket — in-process):
    ├── Resolve session for this chat (via session map)
    ├── Start turn with message text
    │
    ▼
Gateway: processes turn normally
    │
    ▼
Gateway response (AssistantDelta, TurnCompleted, etc.)
    │
    ▼
Adapter: accumulate deltas → format → send back via Telegram API
```

The gateway doesn't need to know about Telegram at all. The adapter is an internal component that calls gateway methods directly.

---

## 3. Codebase Ground Truth

Current state verified against actual code, not plan assumptions.

### Actual Types (as-is)

| Type | Location | Current State |
|------|----------|---------------|
| `GatewaySession` | `types/src/channel.rs` | `{ user_id, session_id }` ✅ renamed from `runtime_session_id` |
| `GatewayClientHello` | `types/src/channel.rs` | `{ request_id, protocol_version, user_id, session_id?, create_new_session }` ✅ updated |
| `GatewayClientFrame` | `types/src/channel.rs` | `Hello, SendTurn, CancelActiveTurn, HealthCheck` |
| `GatewayServerFrame` | `types/src/channel.rs` | `HelloAck, TurnStarted, AssistantDelta, TurnCompleted, TurnCancelled, Error, HealthStatus, TurnProgress, ScheduledNotification` |
| `RuntimeProgressKind` | `types/src/model.rs` | `ProviderCall, ToolExecution { tool_names }, RollingSummary` |
| `GATEWAY_PROTOCOL_VERSION` | `types/src/channel.rs` | `2` ✅ bumped from 1 |
| `AgentConfig` | `types/src/config.rs` | No `gateway`, `channels`, or `agents` fields. Channels config will go in `RunnerUserConfig` (host-side, per-user). Agent definitions will go here. |
| `ToolExecutionContext` | `types/src/tool.rs` | `{ user_id?, session_id? }` ✅ now threaded as parameter, not shared state |

### Actual Gateway State (as-is, post Steps 1-4)

```rust
// gateway/src/lib.rs
pub struct GatewayServer {
    turn_runner: Arc<dyn GatewayTurnRunner>,
    startup_status: Option<StartupStatusReport>,
    users: RwLock<HashMap<String, Arc<UserState>>>,        // keyed by user_id
    next_connection_id: AtomicU64,
    session_store: Option<Arc<dyn SessionStore>>,           // ✅ persistence (Step 4)
    max_concurrent_turns: u32,                              // ✅ concurrency limit (Step 3)
}

// gateway/src/session.rs
struct UserState {
    user_id: String,
    sessions: RwLock<HashMap<String, Arc<SessionState>>>,  // ✅ multi-session (Step 3)
    concurrent_turns: AtomicU32,                            // ✅ concurrency tracking (Step 3)
}

struct SessionState {
    session_id: String,
    user_id: String,
    agent_name: String,                                     // ✅ "default" or named (Step 3)
    parent_session_id: Option<String>,                      // ✅ for subagent sessions (Step 3)
    created_at: Instant,
    events: broadcast::Sender<GatewayServerFrame>,
    active_turn: Mutex<Option<ActiveTurnState>>,
    latest_terminal_frame: Mutex<Option<GatewayServerFrame>>,
}
```

### Actual Dependency Graph

```
types ← provider, tools, tools-macros, memory, runtime, channels, gateway, tui, runner
channels ← types (only; will gain gateway + memory for in-process adapter integration)
memory ← types
provider ← types
tools ← types
runtime ← types, tools
gateway ← types, runtime, tools, uuid (dev: libsql, memory)
tui ← types (standalone binary)
runner ← types, provider, tools, runtime, memory, gateway
```

Key: `channels` does NOT depend on `gateway`. `tools` does NOT depend on `runtime`. `gateway` does NOT depend on `memory` at compile time (dev-dependency only for tests; session store injected at runtime via trait).

### Actual Migrations

Last migration: `0020_create_gateway_sessions.sql`
Required tables: 11 (`memory_migrations`, `sessions`, `conversation_events`, `session_state`, `files`, `chunks`, `chunks_vec`, `chunks_fts`, `schedules`, `schedule_runs`, `gateway_sessions`)
Required indexes: 13
Required triggers: 3

### Actual UUID Usage

All crates currently use `uuid` v4. Session IDs would benefit from v7 (time-ordered) for natural sort order.

---

## 4. Target Architecture

### 4.1 Session Model

```
┌──────────────────────────────────────────────────────────────────┐
│                        GatewayServer                             │
│                                                                  │
│  users: RwLock<HashMap<UserId, Arc<UserState>>>                 │
│  session_store: Arc<dyn SessionStore>                            │
│                                                                  │
│  ┌──────────────────────────────────────────────────────────┐   │
│  │ UserState                                                 │   │
│  │  ├── user_id: String                                      │   │
│  │  ├── sessions: RwLock<HashMap<SessionId, Arc<SessionState>>>│  │
│  │  └── concurrent_turns: AtomicU32                          │   │
│  └──────────────────────────────────────────────────────────┘   │
│                                                                  │
│  ┌──────────────────────────────────────────────────────────┐   │
│  │ SessionState                                              │   │
│  │  ├── session_id: String                                   │   │
│  │  ├── user_id: String                                      │   │
│  │  ├── agent_name: String ("default" or named)              │   │
│  │  ├── parent_session_id: Option<String>                    │   │
│  │  ├── created_at: Instant                                  │   │
│  │  ├── events: broadcast::Sender<GatewayServerFrame>        │   │
│  │  ├── active_turn: Mutex<Option<ActiveTurnState>>          │   │
│  │  └── latest_terminal_frame: Mutex<Option<...>>            │   │
│  └──────────────────────────────────────────────────────────┘   │
│                                                                  │
│  ┌──────────────────────────────────────────────────────────┐   │
│  │ ConnectionState (per WS / per Telegram chat binding)      │   │
│  │  ├── connection_id: u64                                   │   │
│  │  ├── user_id: String                                      │   │
│  │  ├── channel_id: String                                   │   │
│  │  └── active_session_id: String                            │   │
│  └──────────────────────────────────────────────────────────┘   │
└──────────────────────────────────────────────────────────────────┘
```

### 4.2 Identity & Auth Flow

```
                          HOST SIDE (Runner process)
┌─────────────────────────────────────────────────────────────────┐
│  Runner:                                                        │
│  ├── Reads RunnerUserConfig (channels config)                   │
│  ├── Includes channels config in RunnerBootstrapEnvelope        │
│  ├── Forwards bot token env vars to VM                          │
│  └── That's it — no adapter logic, no WebSocket management      │
└─────────────────────────────────────────────────────────────────┘
                              │ Bootstrap envelope + env vars
                              ▼
                          GUEST SIDE (oxydra-vm)
┌─────────────────────────────────────────────────────────────────┐
│                                                                 │
│  GatewayServer                                                  │
│  ├── UserState("alice")                                         │
│  │   ├── Session from TUI (WebSocket connection)                │
│  │   ├── Session from Telegram adapter (in-process)             │
│  │   └── (sessions are independent)                             │
│  └── Multi-session, protocol v2, session persistence            │
│                                                                 │
│  TelegramAdapter (in-process, spawned at startup)               │
│  ├── bot: frankenstein::client_reqwest::Bot (alice's bot token from env) │
│  ├── authorized_senders: HashSet<platform_id>                   │
│  ├── session_map: (chat_id, topic_id?) → session_id             │
│  ├── Calls gateway internal API directly (D10, no WebSocket)    │
│  └── ResponseStreamer: edit-message streaming (D15)              │
│                                                                 │
│  Message flow:                                                  │
│  Telegram API → frankenstein poll → auth check                  │
│    → derive channel_context_id (D14: chat_id or chat_id:topic)  │
│    → gateway.submit_turn()                                      │
│                                                                 │
│  Response flow:                                                 │
│  gateway.subscribe_events() → ResponseStreamer                  │
│    → send placeholder → throttled edits → final edit            │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

### 4.3 Multi-Agent Delegation

```
Parent AgentRuntime (session-alice-abc)
    │
    ├── LLM calls delegate_to_agent(agent_name, goal, key_facts)
    │
    ▼
DelegationExecutor trait (in types, impl in runtime):
    │
    ├── Validate agent_name exists in AgentDefinition registry
    ├── Create child CancellationToken (linked to parent)
    ├── Reserve budget from parent's remaining allocation
    ├── Construct child AgentRuntime with agent-specific config
    │    ├── Different system prompt
    │    ├── Different model (optional)
    │    └── Subset of tools
    │
    ├── Create subagent session:
    │    session_id = "subagent:{parent_session_id}:{agent_name}:{uuid}"
    │
    ├── Execute child runtime.run_session()
    │    └── Progress events forwarded to parent via RuntimeProgressKind::SubagentExecution
    │
    └── Return result to parent as tool output string
```

---

## 5. Implementation Steps

Each step is self-contained, testable, and does not require rewriting any prior step.

### Step 1: Fix Tool Execution Context Race ✅ DONE

**Goal:** Eliminate the shared mutable `ToolExecutionContext` before enabling concurrent sessions.

**Crates:** `types`, `runtime`, `gateway`

**Changes applied:**

1. **`runtime/src/lib.rs`:**
   - Removed the `tool_execution_context: Arc<Mutex<ToolExecutionContext>>` field from `AgentRuntime`
   - Removed the `set_tool_execution_context()` method
   - Added `tool_context: &ToolExecutionContext` parameter to `run_session_internal()`
   - `run_session()` passes `ToolExecutionContext::default()`
   - `run_session_for_session()` constructs context from session_id
   - `run_session_for_session_with_stream_events()` accepts `tool_context` parameter from caller

2. **`runtime/src/tool_execution.rs`:**
   - `execute_tool_call()` accepts `tool_context: &ToolExecutionContext` parameter and passes it to `execute_with_context()`
   - `execute_tool_and_format()` accepts and threads `tool_context` parameter

3. **`gateway/src/turn_runner.rs`:**
   - `RuntimeGatewayTurnRunner::run_turn()` constructs `ToolExecutionContext { user_id, session_id }` locally and passes it to runtime

**Verification:** ✅ All tests pass, clippy clean

---

### Step 2: Protocol Update for Multi-Session + Channels ✅ DONE

**Goal:** Update protocol version, add new Hello fields for session control, rename `runtime_session_id` → `session_id` throughout.

**Crates:** `types`, `gateway`, `tui`, `runner`

**Changes applied:**

1. **`types/src/channel.rs`:**
   - `GATEWAY_PROTOCOL_VERSION` changed from `1` to `2`
   - `GatewaySession.runtime_session_id` → `session_id`
   - `GatewayClientHello.runtime_session_id` → `session_id`, added `create_new_session: bool` with `#[serde(default)]`
   - `GatewaySendTurn.runtime_session_id` → `session_id`
   - `GatewayCancelActiveTurn.runtime_session_id` → `session_id`

2. **`gateway/src/lib.rs`:**
   - `UserSessionState.runtime_session_id` → `session_id`
   - `default_runtime_session_id()` → `default_session_id()`
   - All internal references updated

3. **`gateway/src/turn_runner.rs`:**
   - `GatewayTurnRunner::run_turn()` parameter `runtime_session_id` → `session_id`
   - `ScheduledTurnRunner::run_scheduled_turn()` parameter renamed

4. **`tui/src/channel_adapter.rs`:**
   - `TuiUiState.runtime_session_id` → `session_id`
   - `build_hello_frame()` uses `session_id` field with `create_new_session: false`
   - All frame handlers updated

5. **`runner/src/lib.rs`:**
   - `RunnerTuiConnection.runtime_session_id` → `session_id`
   - `probe_gateway_health()` sends v2 Hello with `create_new_session: false`

6. **`runtime/src/lib.rs`:**
   - `ScheduledTurnRunner` trait parameter renamed

7. **All test files updated across all crates**

**Verification:** ✅ All tests pass, clippy clean, zero remaining `runtime_session_id` references

---

### Step 3: Multi-Session Gateway Core ✅ DONE

**Goal:** Replace single-session-per-user with multi-session model.

**Crates:** `gateway`

**Changes applied:**

1. **`gateway/src/session.rs` (new file):**
   - `UserState` struct with `sessions: RwLock<HashMap<String, Arc<SessionState>>>` and `concurrent_turns: AtomicU32`
   - `SessionState` struct (evolved from `UserSessionState`) with `session_id`, `user_id`, `agent_name`, `parent_session_id`, `created_at`, `events`, `active_turn`, `latest_terminal_frame`
   - `ActiveTurnState` moved here from `lib.rs`
   - `SessionState` made `pub` for use by in-process channel adapters (D10)

2. **`gateway/src/lib.rs` — major refactor:**
   - `GatewayServer.sessions` replaced with `GatewayServer.users: RwLock<HashMap<String, Arc<UserState>>>` (multi-session per user)
   - Added `session_store: Option<Arc<dyn SessionStore>>` and `max_concurrent_turns: u32` fields
   - New constructors: `with_session_store()`, `with_options()` for full configuration
   - **Internal API (D10) extracted:**
     - `create_or_get_session()` — create new UUID v7 session or join existing, with session store resumption fallback
     - `submit_turn()` — start a turn on a session
     - `cancel_session_turn()` — cancel active turn on a session
     - `subscribe_events()` — subscribe to session broadcast
     - `list_user_sessions()` — list sessions from store or in-memory fallback
   - `resolve_session()` updated: `create_new_session: true` → UUID v7 session; `session_id: Some(id)` → join existing; neither → backward-compat deterministic default session
   - `start_turn()` now enforces per-user concurrent turn limit via `UserState.concurrent_turns`
   - `start_turn()` decrements concurrent turns on completion and touches session store
   - `SchedulerNotifier` updated to iterate all sessions under a user (nested `users → sessions`)
   - `EVENT_BUFFER_CAPACITY` increased from 256 to 1024
   - Per-connection active session tracking via `active_session` variable in `handle_socket`

3. **Backward compatibility maintained:**
   - TUI with `create_new_session: false` and no `session_id` still gets "runtime-{user_id}" deterministic session
   - Existing tests continue to pass without modification

**Verification:** ✅ All existing tests pass; new tests added:
- `create_new_session_creates_fresh_session_with_uuid`: UUID v7 sessions generated
- `two_sessions_for_same_user_run_turns_concurrently`: concurrent multi-session turns
- `session_id_in_hello_joins_existing_session`: session join by ID
- `concurrent_turn_limit_enforced`: max_concurrent_turns rejection
- `default_session_backward_compat`: backward compat for old TUI behavior

---

### Step 4: Session Persistence ✅ DONE

**Goal:** Sessions survive gateway restart. Session listing works from DB.

**Crates:** `types`, `memory`, `gateway`, `runner`

**Changes applied:**

1. **`types/src/session.rs` (new file):**
   - `SessionStore` trait with `create_session`, `get_session`, `list_sessions`, `touch_session`, `archive_session`
   - `SessionRecord` struct with `session_id`, `user_id`, `agent_name`, `display_name`, `channel_origin`, `parent_session_id`, `created_at`, `last_active_at`, `archived`
   - Re-exported from `types/src/lib.rs`

2. **`memory/migrations/0020_create_gateway_sessions.sql` (new file):**
   - `gateway_sessions` table with all required columns
   - `idx_gateway_sessions_user_active` index (user_id, archived, last_active_at DESC)
   - `idx_gateway_sessions_parent` partial index on parent_session_id

3. **`memory/src/schema.rs`:**
   - Migration 0020 added to MIGRATIONS array
   - `"gateway_sessions"` added to REQUIRED_TABLES
   - Both indexes added to REQUIRED_INDEXES

4. **`memory/src/session_store.rs` (new file):**
   - `LibsqlSessionStore` implementing `SessionStore` trait using libSQL
   - Full CRUD operations with `ON CONFLICT DO NOTHING` for idempotent create
   - `datetime('now')` for timestamps
   - Comprehensive test suite (8 tests):
     - `create_and_get_session`, `get_nonexistent_session_returns_none`
     - `list_sessions_excludes_archived_by_default`, `touch_session_updates_last_active_at`
     - `archive_session_marks_as_archived`, `create_session_with_parent`
     - `list_sessions_filters_by_user`, `duplicate_create_is_idempotent`

5. **`gateway/src/lib.rs`:**
   - `GatewayServer.session_store: Option<Arc<dyn SessionStore>>` field
   - Session creation persisted to store in `create_or_get_session()`
   - Session resumed from store when not found in memory
   - `touch_session()` called on turn completion (async, fire-and-forget)
   - `list_user_sessions()` reads from store when available

6. **`runner/src/bootstrap.rs`:**
   - `VmBootstrapRuntime.session_store: Option<Arc<dyn types::SessionStore>>` field
   - Session store built from memory backend's `connect_for_scheduler()` (reuses connection infrastructure)

7. **`runner/src/bin/oxydra-vm.rs`:**
   - Gateway constructed with `with_session_store()` when session store is available
   - Falls back to `with_startup_status()` when no memory backend

**Verification:** ✅ All tests pass; new tests added:
- `session_persisted_to_store_on_creation`: WebSocket Hello → verify DB record
- `touch_session_called_on_turn_completion`: turn completion → last_active_at updated
- `internal_api_create_and_list_sessions`: internal API creates, store lists
- `internal_api_get_existing_session_by_id`: get-by-ID returns same session

---

### Step 5: Session Lifecycle UX ✅ DONE

**Goal:** `/new`, `/sessions`, `/switch` commands available in TUI and through protocol.

**Crates:** `types`, `gateway`, `tui`, `memory`

**Changes applied:**

1. **`types/src/channel.rs` — new v2 client frames:**
   - `GatewayCreateSession { request_id, display_name?, agent_name? }` — request creation of a new session
   - `GatewayListSessions { request_id, include_archived, include_subagent_sessions }` — request session listing
   - `GatewaySwitchSession { request_id, session_id }` — switch connection to a different session
   - Added all three as variants to `GatewayClientFrame` enum

2. **`types/src/channel.rs` — new v2 server frames:**
   - `GatewaySessionCreated { request_id, session, display_name?, agent_name }` — confirmation of new session
   - `GatewaySessionList { request_id, sessions: Vec<GatewaySessionSummary> }` — listing response
   - `GatewaySessionSummary { session_id, agent_name, display_name?, channel_origin, created_at, last_active_at, archived }` — summary of a session in a listing
   - `GatewaySessionSwitched { request_id, session, active_turn? }` — confirmation of session switch
   - Added all three as variants to `GatewayServerFrame` enum
   - Re-exported all new types from `types/src/lib.rs`

3. **`types/src/session.rs` — new trait method:**
   - Added `update_display_name(session_id, display_name)` to `SessionStore` trait

4. **`memory/src/session_store.rs` — trait implementation:**
   - Implemented `update_display_name()` in `LibsqlSessionStore` using SQL UPDATE
   - Added test: `update_display_name_sets_name`

5. **`gateway/src/lib.rs` — new frame handlers in `handle_client_frame()`:**
   - `CreateSession`: creates new session via `create_or_get_session()`, persists display name, switches connection's active session and broadcast subscription, returns `SessionCreated`
   - `ListSessions`: reads from `list_user_sessions()`, filters out subagent sessions (parent_session_id IS NOT NULL) unless `include_subagent_sessions` is true, returns `SessionList`
   - `SwitchSession`: looks up session by ID via `create_or_get_session()`, switches broadcast subscription, returns `SessionSwitched` with current `active_turn` status
   - Changed `_updates` parameter to `updates` (now used for broadcast re-subscription on session switch/create)

6. **`tui/src/channel_adapter.rs` — session command methods:**
   - Added `create_session()`, `list_sessions()`, `switch_session()` methods that enqueue appropriate `GatewayClientFrame` variants
   - Added `with_session_id()` constructor for `--session` CLI flag support
   - Updated `build_hello_frame()`: reconnection uses stored session_id; first connect with `--session` joins that session; default behavior sends `create_new_session: true`
   - Updated `apply_gateway_frame()` to handle `SessionCreated`, `SessionList`, `SessionSwitched` server frames

7. **`tui/src/app.rs` — slash command interception:**
   - Added `try_handle_slash_command()` method that intercepts `/new [name]`, `/sessions`, `/switch <id>` before sending as turns
   - `/new [name]` → sends `CreateSession` with optional display name
   - `/sessions` → sends `ListSessions`
   - `/switch <id>` → sends `SwitchSession`; returns error if no session_id argument provided
   - Unknown `/` commands are treated as normal prompts
   - Added `with_session_id()` constructor for `TuiApp`

8. **`tui/src/bin/oxydra-tui.rs` — `--session` CLI flag:**
   - Added `--session <id>` CLI argument (optional)
   - When provided: creates `TuiApp::with_session_id()` (joins existing session)
   - When omitted: creates `TuiApp::new()` (creates new session)

9. **`tui/src/ui_model.rs` — view model updates:**
   - `apply_server_frame()` handles `SessionCreated` (shows "New session created" system message), `SessionList` (shows formatted session listing with shortened IDs), `SessionSwitched` (shows switch confirmation and resumes stream if active turn)
   - `push_message()` made `pub` for use by `app.rs` error display

10. **`tui/src/widgets.rs` — status bar updates:**
    - Session ID displayed shortened (first 8 chars) for readability
    - Idle key hints updated to `[Ctrl+C to exit | /new /sessions /switch]`
    - Existing widget tests updated for new hint text and shortened IDs

**Verification:** ✅ All tests pass, clippy clean
- New gateway tests:
  - `create_session_frame_creates_and_switches_to_new_session`: CreateSession creates, persists, and switches connection
  - `list_sessions_returns_user_sessions`: ListSessions returns all user sessions
  - `switch_session_changes_active_session`: SwitchSession changes active session and allows turns
  - `switch_to_nonexistent_session_returns_error`: invalid session_id returns error
  - `list_sessions_filters_subagent_sessions`: subagent sessions filterable
  - `two_tui_windows_with_separate_sessions_work_independently`: independent session isolation
- New memory test: `update_display_name_sets_name`
- All existing tests continue to pass (556 total, 0 failures)
- TUI integration tests pass with v2 protocol

---

### Step 6: Auth & Identity Pipeline ✅ DONE

**Goal:** Sender authentication for external channels. Runs inside the VM alongside the gateway.

**Crates:** `types`, `channels`, `runner`

**Changes applied:**

1. **`types/src/runner.rs`:**
   - Added `SenderBinding` type with `platform_ids: Vec<String>` and optional `display_name`
   - Added `ChannelsConfig` with `telegram: Option<TelegramChannelConfig>` (extensible for future channels)
   - Added `TelegramChannelConfig` with `enabled`, `bot_token_env`, `polling_timeout_secs`, `senders`, `max_message_length` fields with sensible defaults
   - Added `channels: ChannelsConfig` field to `RunnerUserConfig` with `#[serde(default)]`
   - Added `channels: Option<ChannelsConfig>` field to `RunnerBootstrapEnvelope` with `#[serde(default, skip_serializing_if = "Option::is_none")]`
   - Added `ChannelsConfig::bot_token_env_refs()` helper to collect all bot token env var names from enabled channels
   - Added `ChannelsConfig::is_empty()` helper to check if any channel is configured

2. **`types/src/lib.rs`:**
   - Re-exported `ChannelsConfig`, `TelegramChannelConfig`, `SenderBinding` from `runner` module

3. **`runner/src/lib.rs`:**
   - `start_user_for_host()` populates `channels` field in `RunnerBootstrapEnvelope` from `user_config.channels` (None if empty)
   - Added `collect_channel_bot_token_env_vars()` function that resolves bot token env var names from channel config against the runner's environment
   - Bot token env vars are collected alongside existing API key env vars and forwarded to the VM container

4. **`channels/src/sender_auth.rs` (new file):**
   - `SenderAuthPolicy` struct with `authorized: HashSet<String>` for O(1) lookup
   - `from_bindings(&[SenderBinding])` flattens all platform IDs into lookup set
   - `is_authorized(platform_id)`, `authorized_count()`, `is_empty()` methods
   - Implements default-deny: only explicitly listed platform IDs are allowed

5. **`channels/src/audit.rs` (new file):**
   - `AuditEntry` struct with `timestamp`, `channel`, `sender_id`, `reason`, `context` fields
   - `AuditLogger` struct with append-only JSON-lines file output
   - `for_workspace(workspace_root)` constructor targets `<workspace>/.oxydra/sender_audit.log`
   - `log_rejected_sender(entry)` writes entry; failures logged via tracing, never propagated
   - `now_iso8601()` utility for UTC timestamps without external chrono dependency
   - Parent directories created automatically on first write

6. **`channels/src/lib.rs`:**
   - Added `pub mod audit` and `pub mod sender_auth` modules
   - Re-exported `AuditEntry`, `AuditLogger`, `SenderAuthPolicy`

7. **`channels/Cargo.toml`:**
   - Added `serde`, `serde_json`, `tracing` dependencies

8. **All existing test files across crates updated:**
   - Added `channels: None` to all `RunnerBootstrapEnvelope` struct constructions in `types/tests`, `runner/src/tests.rs`, `runner/src/bootstrap.rs`, `runner/src/bin/oxydra-vm.rs`, `runtime/src/tests.rs`, `tools/src/tests.rs`

**Verification:** ✅ All tests pass, clippy clean
- New channels tests (12 total, 9 new):
  - `sender_auth::known_sender_is_authorized`: authorized sender passes check
  - `sender_auth::unknown_sender_is_rejected`: unknown sender rejected
  - `sender_auth::empty_bindings_reject_everyone`: empty policy denies all
  - `sender_auth::multiple_platform_ids_in_one_binding_all_authorize`: multi-ID binding works
  - `sender_auth::multiple_bindings_flatten_into_single_set`: cross-binding flattening
  - `sender_auth::duplicate_platform_ids_deduplicated`: dedup in HashSet
  - `audit::audit_logger_creates_log_file_and_writes_entry`: write + read back
  - `audit::audit_logger_appends_multiple_entries`: append-only behavior
  - `audit::audit_logger_creates_parent_directories`: auto-mkdir
  - `audit::now_iso8601_returns_valid_format`: timestamp format validation
- New types runner_contracts tests (9 new):
  - `channels_config_is_optional_and_defaults_empty`: channels default to empty
  - `channels_config_with_telegram_round_trips_through_toml`: full TOML round-trip
  - `channels_config_defaults_for_telegram_fields`: default values for polling_timeout_secs, max_message_length
  - `empty_senders_list_means_nobody_can_interact`: empty senders = nobody authorized
  - `bot_token_env_refs_only_from_enabled_channels`: disabled channels don't contribute env refs
  - `bootstrap_envelope_with_channels_round_trips`: envelope with channels serializes/deserializes
  - `bootstrap_envelope_without_channels_round_trips`: envelope without channels still works
  - `sender_binding_serde_round_trip`: SenderBinding JSON round-trip
  - `sender_binding_without_display_name`: optional display_name deserialization
- All existing tests continue to pass (zero regressions)

---

### Step 7: Channel Session Mapping

**Goal:** Deterministic mapping from `(channel_id, channel_context_id)` to gateway session. Each Telegram chat/topic maps to a stable session so conversation context is preserved. Platform threading constructs (Telegram forum topics, Discord threads) map to separate sessions for natural concurrency (D14).

**Crates:** `channels`, `memory`

**Changes:**

1. **`memory/migrations/0021_create_channel_session_mappings.sql` (new file):**
   ```sql
   CREATE TABLE IF NOT EXISTS channel_session_mappings (
       channel_id          TEXT NOT NULL,       -- e.g. "telegram"
       channel_context_id  TEXT NOT NULL,       -- e.g. "12345678:42" (chat_id:topic_id)
       session_id          TEXT NOT NULL REFERENCES gateway_sessions(session_id)
                               ON DELETE CASCADE,
       created_at          TEXT NOT NULL DEFAULT (datetime('now')),
       updated_at          TEXT NOT NULL DEFAULT (datetime('now')),
       PRIMARY KEY (channel_id, channel_context_id)
   );
   ```

2. **`memory/src/schema.rs`:**
   - Add migration 0021 to MIGRATIONS
   - Add `"channel_session_mappings"` to REQUIRED_TABLES

3. **`memory/src/session_store.rs` (or `channels` via trait):**
   - Add channel session mapping methods to `SessionStore` trait:
     ```rust
     async fn get_channel_session(
         &self, channel_id: &str, channel_context_id: &str,
     ) -> Result<Option<String>, MemoryError>;

     async fn set_channel_session(
         &self, channel_id: &str, channel_context_id: &str, session_id: &str,
     ) -> Result<(), MemoryError>;
     ```

4. **`channels/src/session_map.rs` (new file):**
   - Thin wrapper around `SessionStore` methods for adapter use
   - Adapter uses it to:
     - Look up existing session for this context: found → use that session
     - Not found → create new session via gateway, save mapping
   - `/new` command → create new session, update mapping
   - `/switch` command → update mapping to different session_id

5. **`channel_context_id` derivation per platform (D14):**
   - **Telegram (forum groups):** `"{chat_id}:{message_thread_id}"` — each topic is a separate session
   - **Telegram (regular chats/DMs):** `"{chat_id}"` — single session per chat
   - **Telegram (non-forum groups):** `"{chat_id}"` — single session for the group
   - **Discord:** `"{guild_id}:{channel_id}:{thread_id?}"` — threads are separate sessions
   - **WhatsApp:** `"{phone_number}:{group_id?}"` — each community group thread is separate
   - Detection: Telegram adapter checks `message.message_thread_id.is_some()` to determine forum mode

6. **Auto-populate `session.display_name`:**
   - When creating a new session from a channel mapping, set `display_name` from the thread/topic name if available
   - Telegram: use forum topic title (available via `getForumTopicInfo`)
   - Discord: use thread name
   - Fallback: `"{channel_name} - {timestamp}"` or first user message truncated to 50 chars

**Verification:**
- New test: first message from a chat creates mapping
- New test: subsequent messages from same chat reuse session
- New test: `/new` on Telegram creates new session and updates mapping
- New test: mapping survives adapter/VM restart (DB-backed)

---

### Step 8: Telegram Channel Adapter

**Goal:** Working Telegram bot that receives messages, authenticates senders, and routes through gateway. Runs inside the VM as an in-process component alongside the gateway.

**Crates:** `channels`, `runner`

**Changes:**

1. **`channels/Cargo.toml`:**
   ```toml
   [features]
   default = []
   telegram = ["dep:frankenstein", "dep:tokio"]

   [dependencies]
   types = { path = "../types" }
   frankenstein = { version = "0.47", optional = true, features = ["client-reqwest"] }
   tokio = { version = "1", optional = true, features = ["rt", "sync", "time"] }
   serde = { version = "1", features = ["derive"] }
   serde_json = "1"
   tracing = "0.1"
   ```

2. **`channels/src/telegram.rs` (new file, behind `#[cfg(feature = "telegram")]`):**
   - `TelegramAdapter` struct:
     ```rust
     pub struct TelegramAdapter {
         bot: frankenstein::client_reqwest::Bot,
         sender_auth: SenderAuthPolicy,
         session_store: Arc<dyn SessionStore>,   // for channel session mappings
         gateway: Arc<GatewayServer>,            // direct reference, in-process (uses internal API from D10)
         user_id: String,
         config: TelegramChannelConfig,
     }
     ```
   - `run()` method — manual long-polling loop using `bot.get_updates()`, runs until cancellation token fires
   - Per incoming `Update`:
     - Extract `sender_id` from `update.content` → `UpdateContent::Message(message)` → `message.from.id`
     - `sender_auth.is_authorized(sender_id)` → false = drop + audit, true = proceed
     - **Derive `channel_context_id` (D14):** check `message.message_thread_id` — if present (forum mode), use `"{chat_id}:{thread_id}"`, otherwise use `"{chat_id}"`
     - Command interception: `/new`, `/sessions`, `/switch`, `/cancel`, `/status`
       - These map to gateway internal API calls (D10): `create_or_get_session()`, `list_user_sessions()`, etc.
     - Normal message: resolve session via `session_store.get_channel_session()`, call `gateway.submit_turn()` directly
   - **Turn-already-active handling:**
     - If `gateway.submit_turn()` returns a "turn already active" error:
       - Reply: "⏳ I'm still working on your previous request. Send /cancel to stop it, or wait for me to finish."
       - Do NOT queue the message (simplicity for v1)
       - Audit log the rejection for observability
   - **Edit-message streaming response (D15):**
     - Subscribe to session's broadcast channel via `gateway.subscribe_events()`
     - On `TurnStarted`: send placeholder message via `bot.send_message()` ("⏳ Working...")
     - On `TurnProgress`: edit placeholder to show status ("🔍 Searching the web...", "🤖 Delegating to researcher...")
     - On `AssistantDelta`: accumulate text, throttled edit every ~1.5 seconds using `bot.edit_message_text()`:
       ```rust
       struct ResponseStreamer {
           bot: frankenstein::client_reqwest::Bot,
           chat_id: i64,
           message_id: i32,               // current message being edited
           accumulated_text: String,
           last_edit: Instant,
           edit_throttle: Duration,           // 1.5 seconds
           progress_status: Option<String>,   // current progress line, shown at top
       }
       ```
     - When accumulated text approaches 4096 chars: stop editing current message, send a new message, continue editing the new one
     - On `TurnCompleted`: final edit with complete response (progress status line removed), plain text only
     - On formatting errors: fallback to sending accumulated text as plain text
   - **Telegram forum topic support:**
     - When replying in a forum topic, include `message_thread_id` in all `send_message()` and `edit_message_text()` calls
     - This ensures replies stay within the correct topic thread
   - Rate limiting: respect Telegram's `retry_after` header on 429 responses
   - Per-chat message queue to avoid out-of-order delivery
   - **Markdown → Telegram HTML conversion:**
     - Implement `markdown_to_telegram_html()` utility
     - Telegram HTML subset: `<b>`, `<i>`, `<code>`, `<pre>`, `<a href="...">`, `<s>`, `<u>`
     - Unsupported markdown constructs (tables, images, headers) → plain text fallback
     - Used in final edit only (interim edits use plain text for speed)

3. **`runner/src/bin/oxydra-vm.rs`:**
   - After gateway + runtime construction:
     - Read `ChannelsConfig` from bootstrap envelope
     - If Telegram enabled: resolve bot token from env, build `SenderAuthPolicy`, construct `TelegramAdapter`
     - Spawn adapter with the VM's `CancellationToken`
   - **Graceful shutdown ordering:**
     - On CancellationToken fire: adapter stops accepting new Telegram updates (long-poll exits)
     - In-flight turns continue until completion (with a 30-second drain timeout)
     - For messages received during drain: reply "I'm restarting, please try again shortly"
     - After drain: adapter task exits, gateway shuts down

4. **`runner/Cargo.toml`:**
   ```toml
   channels = { path = "../channels", optional = true }

   [features]
   telegram = ["channels/telegram"]
   ```

**Verification:**
- Unit test: Telegram Update → sender auth → message construction
- Unit test: long message splitting at paragraph boundaries
- Unit test: command interception (`/new`, `/sessions`, etc.)
- Unit test: `channel_context_id` derivation — forum topic vs regular chat vs DM
- Unit test: `markdown_to_telegram_html()` conversion for supported and unsupported constructs
- Unit test: edit-message throttling (edits spaced ≥ 1.5s)
- Unit test: turn-already-active → user-friendly rejection message
- Integration test with mock Telegram API: full message round-trip with edit-message streaming
- Integration test: unauthorized sender rejected, audit logged
- Integration test: rate limiting with simulated 429
- Integration test: forum topic messages stay within correct thread

---

### Step 9: Agent Definitions & Registry

**Goal:** Multiple agent configurations (system prompt, model, tools) defined in config.

**Crates:** `types`, `runner`

**Changes:**

1. **`types/src/config.rs`:**
   - Add `AgentDefinition`:
     ```rust
     pub struct AgentDefinition {
         pub system_prompt: Option<String>,
         pub system_prompt_file: Option<String>,
         pub selection: Option<ProviderSelection>,
         pub tools: Option<Vec<String>>,  // None = all tools
         pub max_turns: Option<usize>,
         pub max_cost: Option<f64>,
     }
     ```
   - Add to `AgentConfig`:
     ```rust
     #[serde(default)]
     pub agents: BTreeMap<String, AgentDefinition>,
     ```

2. **`runner/src/bootstrap.rs`:**
   - Validate agent definitions at startup:
     - If `selection` is provided, validate provider exists in registry
     - If `tools` is provided, validate tool names exist in registry
     - If `system_prompt_file` is provided, validate file exists
   - Build per-agent `AgentRuntime` instances (or defer to delegation time)
   - Augment default system prompt with available agent descriptions:
     ```
     ## Available Specialist Agents
     - **researcher**: research specialist (tools: web_search, web_fetch, ...)
     - **coder**: coding specialist (tools: file_read, file_write, ...)
     Use the `delegate_to_agent` tool to delegate tasks to these specialists.
     ```

**Verification:**
- Config test: valid agent definitions parse correctly
- Config test: invalid tool names rejected at validation
- Config test: agent definitions are optional (empty map = no named agents)
- Bootstrap test: system prompt augmented with agent descriptions

---

### Step 10: Multi-Agent Delegation

**Goal:** Parent agent can delegate to named subagents and receive results.

**Crates:** `types`, `tools`, `runtime`, `gateway`, `runner`

**Changes:**

1. **`types/src/delegation.rs` (new file):**
   ```rust
   #[async_trait]
   pub trait DelegationExecutor: Send + Sync {
       async fn delegate(
           &self,
           request: DelegationRequest,
           parent_cancellation: &CancellationToken,
           progress_sender: Option<&RuntimeStreamEventSender>,
       ) -> Result<DelegationResult, RuntimeError>;
   }

   pub struct DelegationRequest {
       pub parent_session_id: String,
       pub parent_user_id: String,
       pub agent_name: String,
       pub goal: String,
       pub key_facts: Vec<String>,
       pub max_turns: Option<u32>,
       pub max_cost: Option<f64>,
   }

   pub struct DelegationResult {
       pub output: String,
       pub turns_used: usize,
       pub cost_used: f64,
       pub status: DelegationStatus,
   }

   pub enum DelegationStatus {
       Completed,
       BudgetExhausted { partial_output: String },
       Failed { error: String },
   }
   ```

2. **`tools/src/delegation_tools.rs` (new file):**
   - `delegate_to_agent` tool using `#[tool]` macro
   - Takes `Arc<dyn DelegationExecutor>` as injected dependency (same pattern as scheduler tools)
   - Tool schema describes available agents (populated from config at registration time)
   - `SafetyTier::SideEffecting`

3. **`runtime/src/delegation.rs` (new file):**
   - `RuntimeDelegationExecutor` implementing `DelegationExecutor`:
     - Looks up `AgentDefinition` from config
     - Constructs child `AgentRuntime` with:
       - Agent-specific system prompt
       - Agent-specific model (or inherits parent's)
       - Filtered tool registry (agent's `tools` allowlist)
       - Agent-specific limits (max_turns, max_cost)
     - Creates child `CancellationToken` linked to parent
     - Generates subagent session_id: `"subagent:{parent_session_id}:{agent_name}:{uuid}"`
     - Constructs initial context with goal + key_facts as system message
     - Calls `runtime.run_session_internal()`
     - Returns result to caller
   - Budget cascading:
     - Subagent budget = min(requested, parent_remaining)
     - On completion: unused budget returned to parent
   - Safety caps (configurable):
     - Max delegation depth: 3 (prevents recursive delegation loops)
     - Max subagents per parent turn: 5
     - Ancestor agent name tracking (prevents A → B → A cycles)
   - Progress forwarding:
     - Emits `RuntimeProgressKind::SubagentExecution` events through parent's stream

4. **`runtime/src/lib.rs`:**
   - Add `RuntimeProgressKind::SubagentExecution { agent_name, subagent_session_id }` variant

5. **`tools/src/lib.rs`:**
   - Add `register_delegation_tools()` function (mirrors `register_scheduler_tools()` pattern)

6. **`runner/src/bootstrap.rs`:**
   - When `agents` config has entries:
     - Build `RuntimeDelegationExecutor` with config and provider factory
     - Call `register_delegation_tools(&mut registry, executor, &config.agents)`
   - Available agent names/descriptions passed to tool registration for schema generation

7. **`gateway/src/turn_runner.rs`:**
   - `RuntimeGatewayTurnRunner` gains access to delegation executor (or it's embedded in runtime)
   - Subagent progress events flow through existing `StreamItem::Progress` → `GatewayServerFrame::TurnProgress` path

**Verification:**
- Unit test: `DelegationRequest` serialization/validation
- Runtime test: successful delegation round-trip with MockProvider
- Runtime test: budget cascading (subagent cannot exceed parent budget)
- Runtime test: parent cancellation cascades to child
- Runtime test: delegation depth limit enforced
- Runtime test: cycle prevention (A→B→A rejected)
- Runtime test: progress events forwarded
- Integration test: full turn with delegation through gateway

---

### Step 11: Concurrent Turn Management

**Goal:** Bounded per-user concurrency with fair queueing.

**Crates:** `types`, `gateway`

**Changes:**

1. **`types/src/config.rs`:**
   - Add `GatewayConfig`:
     ```rust
     pub struct GatewayConfig {
         pub max_sessions_per_user: usize,           // default: 10
         pub max_concurrent_turns_per_user: usize,   // default: 3
         pub session_idle_ttl_hours: u64,             // default: 24
     }
     ```
   - Add `gateway: GatewayConfig` to `AgentConfig` with `#[serde(default)]`

2. **`gateway/src/lib.rs`:**
   - `UserState` uses `tokio::sync::Semaphore` for bounded concurrency:
     ```rust
     struct UserState {
         user_id: String,
         sessions: RwLock<HashMap<String, Arc<SessionState>>>,
         turn_semaphore: Arc<Semaphore>,  // permits = max_concurrent_turns_per_user
     }
     ```
   - `start_turn()` acquires semaphore permit before spawning turn task
   - Permit released when turn completes (RAII via `OwnedSemaphorePermit`)
   - If permit unavailable: return error frame "too many concurrent turns"
   - **Subagent turns do NOT acquire semaphore permits (D11):** They run under the parent turn's permit since the parent tool call synchronously awaits the result. The delegation depth limit (max 3) and budget cascading prevent fan-out abuse. The semaphore counts top-level user-initiated turns only.

3. **Session cleanup background task:**
   - Spawn a periodic task (every 5 minutes) that:
     - Checks sessions with no subscribers and idle > TTL
     - Re-checks subscriber count under the write lock before removal (prevents race with incoming messages)
     - Archives them in DB
     - Removes from in-memory map
     - **Notifies turn runner to drop evicted session contexts** (prevents OOM from unbounded `RuntimeGatewayTurnRunner.contexts` HashMap)
   - Uses the gateway's `CancellationToken` for clean shutdown

4. **Turn runner context eviction:**
   - Add `drop_session_context(&self, session_id: &str)` method to `GatewayTurnRunner` trait
   - `RuntimeGatewayTurnRunner` implements it by removing the session's `Context` from its `contexts` HashMap
   - Called by the session cleanup task when archiving/evicting a session

**Verification:**
- Test: concurrent turn limit enforced (top-level turns only)
- Test: subagent turns do NOT consume semaphore permits
- Test: delegation depth 3 with concurrent_turns=3 does not deadlock
- Test: session TTL cleanup archives stale sessions
- Test: session cleanup re-checks subscribers under write lock (no race)
- Test: turn runner context dropped on session eviction
- Test: archived session data remains in DB for listing

---

### Step 12: Integration Polish

**Goal:** End-to-end reliability, cross-feature interactions, docs.

**Changes:**

1. **System prompt updates:**
   - When agents are defined: add section about delegation capabilities

2. **Guidebook updates:**
   - Update Chapter 9 (Gateway and Channels) with multi-session model and internal gateway API (D10)
   - Update Chapter 9 to document `Channel` trait scope (WebSocket clients only) and in-process adapter pattern (D13)
   - Update Chapter 12 (External Channels) with auth/identity model, thread/topic session mapping (D14), edit-message streaming (D15)
   - Update Chapter 14 (Productization) with session lifecycle
   - Update Chapter 15 (Build Plan) with completion status

3. **Integration tests:**
   - TUI + Telegram simultaneous sessions
   - Delegation from Telegram session
   - Session switching mid-conversation
   - Scheduled task alongside interactive session + delegation
   - Telegram forum topics: messages in different topics run concurrently
   - Edit-message streaming: verify placeholder → progress → final edit sequence
   - Session eviction + resumption: evict session, send new message, verify session recreated from DB

---

## 6. Database Migrations

### Migration 0020: gateway_sessions

```sql
-- 0020_create_gateway_sessions.sql
CREATE TABLE IF NOT EXISTS gateway_sessions (
    session_id        TEXT PRIMARY KEY,
    user_id           TEXT NOT NULL,
    agent_name        TEXT NOT NULL DEFAULT 'default',
    display_name      TEXT,
    channel_origin    TEXT NOT NULL DEFAULT 'tui',
    parent_session_id TEXT,
    created_at        TEXT NOT NULL DEFAULT (datetime('now')),
    last_active_at    TEXT NOT NULL DEFAULT (datetime('now')),
    archived          INTEGER NOT NULL DEFAULT 0,
    FOREIGN KEY (parent_session_id) REFERENCES gateway_sessions(session_id)
        ON DELETE SET NULL
);

CREATE INDEX IF NOT EXISTS idx_gateway_sessions_user_active
    ON gateway_sessions(user_id, archived, last_active_at DESC);

CREATE INDEX IF NOT EXISTS idx_gateway_sessions_parent
    ON gateway_sessions(parent_session_id)
    WHERE parent_session_id IS NOT NULL;
```

### Host-side persistence (no DB migration needed)

- **Sender audit log:** Append-only file at `<workspace>/.oxydra/sender_audit.log` (one JSON line per rejected sender).

### Migration 0021: channel_session_mappings

```sql
-- 0021_create_channel_session_mappings.sql
CREATE TABLE IF NOT EXISTS channel_session_mappings (
    channel_id          TEXT NOT NULL,
    channel_context_id  TEXT NOT NULL,
    session_id          TEXT NOT NULL REFERENCES gateway_sessions(session_id)
                            ON DELETE CASCADE,
    created_at          TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at          TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (channel_id, channel_context_id)
);
```

### Schema verification additions

In `memory/src/schema.rs`, add to `REQUIRED_TABLES`:
```rust
"gateway_sessions",
"channel_session_mappings",
```

Add to `REQUIRED_INDEXES`:
```rust
"idx_gateway_sessions_user_active",
"idx_gateway_sessions_parent",
```

---

## 7. Protocol Changes

### Updated Client Frame Types

```rust
// GatewayClientHello — session_id replaces runtime_session_id
pub struct GatewayClientHello {
    pub request_id: String,
    pub protocol_version: u16,       // must be 2
    pub user_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,  // join existing session (renamed from runtime_session_id)
    #[serde(default)]
    pub create_new_session: bool,    // create fresh session
}

// New client frames (added to GatewayClientFrame enum)
CreateSession(GatewayCreateSession),
ListSessions(GatewayListSessions),
SwitchSession(GatewaySwitchSession),
```

### Updated Server Frame Types

```rust
// New server frames (added to GatewayServerFrame enum)
SessionCreated(GatewaySessionCreated),
SessionList(GatewaySessionList),
SessionSwitched(GatewaySessionSwitched),

// RuntimeProgressKind gains:
SubagentExecution {
    agent_name: String,
    subagent_session_id: String,
}
```

### Full Frame Summary

| Client Frame | Description |
|-------------|-------------|
| Hello | Session selection (create new or join existing) |
| SendTurn | Send user message |
| CancelActiveTurn | Cancel running turn |
| HealthCheck | Liveness check |
| CreateSession | Create a new named session |
| ListSessions | List user's sessions |
| SwitchSession | Switch connection to a different session |

| Server Frame | Description |
|-------------|-------------|
| HelloAck | Session confirmed |
| TurnStarted | Turn processing began |
| AssistantDelta | Streaming token |
| TurnCompleted | Turn finished |
| TurnCancelled | Turn was cancelled |
| TurnProgress | Progress update (including SubagentExecution) |
| Error | Error notification |
| HealthStatus | Health response |
| ScheduledNotification | Scheduled task notification |
| SessionCreated | New session created |
| SessionList | Session listing response |
| SessionSwitched | Session switch confirmed |

---

## 8. Configuration Schema

### Per-User Host Config (users/alice/config.toml → RunnerUserConfig)

```toml
# ── Sandbox & Resources (existing) ──────────────────────────
[mounts]
shared = "/home/alice/shared"

[resources]
max_vcpus = 2
max_memory_mib = 4096

[credential_refs]
anthropic = "ANTHROPIC_API_KEY"

[behavior]
shell_enabled = true

# ── Channels (NEW — per-user, host-side) ────────────────────
[channels.telegram]
enabled = true
bot_token_env = "ALICE_TELEGRAM_BOT_TOKEN"
polling_timeout_secs = 30
max_message_length = 4096

# Sender identity bindings
[[channels.telegram.senders]]
platform_ids = ["12345678"]

[[channels.telegram.senders]]
platform_ids = ["87654321"]
display_name = "Bob"
```

### Agent Config (agent.toml → AgentConfig, shared or per-workspace)

```toml
config_version = "1.0.0"

# ── Provider Selection (default agent) ──────────────────────
[selection]
provider = "anthropic"
model = "claude-sonnet-4-20250514"

# ── Provider Registry ───────────────────────────────────────
[providers.registry.anthropic]
provider_type = "anthropic"
api_key_env = "ANTHROPIC_API_KEY"

[providers.registry.openai]
provider_type = "openai"
api_key_env = "OPENAI_API_KEY"

# ── Runtime ─────────────────────────────────────────────────
[runtime]
turn_timeout_secs = 120
max_turns = 15
max_cost = 5.0

# ── Gateway ─────────────────────────────────────────────────
[gateway]
max_sessions_per_user = 10
max_concurrent_turns_per_user = 3
session_idle_ttl_hours = 24

# ── Agent Definitions (optional) ────────────────────────────
[agents.researcher]
system_prompt = "You are a research specialist..."
tools = ["web_search", "web_fetch", "file_read", "file_write", "memory_search", "memory_save"]
max_turns = 15
max_cost = 1.5

[agents.coder]
system_prompt = "You are a coding specialist..."
tools = ["file_read", "file_write", "file_edit", "file_list", "file_search", "file_delete", "shell_exec"]
max_turns = 25
max_cost = 3.0

# ── Memory ──────────────────────────────────────────────────
[memory]
enabled = true

# ── Scheduler ───────────────────────────────────────────────
[scheduler]
enabled = true
```

### What Lives Where

| Config Section | File | Scope | Rationale |
|---------------|------|-------|-----------|
| `channels.*` | `config.toml` (RunnerUserConfig) | Per-user, host-side config; delivered to VM via bootstrap envelope | Bot tokens, sender bindings differ per user |
| `gateway.*` | `agent.toml` (AgentConfig) | Per-workspace, guest-side | Session limits are agent behavior; gateway runs in VM |
| `agents.*` | `agent.toml` (AgentConfig) | Per-workspace, guest-side | Agent definitions are agent behavior |
| `selection`, `providers`, `runtime`, `memory`, `scheduler`, `tools` | `agent.toml` (AgentConfig) | Per-workspace, guest-side | Existing config, unchanged |
| `mounts`, `resources`, `credential_refs`, `behavior` | `config.toml` (RunnerUserConfig) | Per-user, host-side | Existing config, unchanged |

### Defaults

All new config sections use `#[serde(default)]` so existing configs work without modification:
- `RunnerUserConfig.channels`: default = no channels (empty struct, all None)
- `AgentConfig.gateway`: defaults defined in impl, sessions limit = 10, concurrency limit = 3
- `AgentConfig.agents`: empty BTreeMap (no named agents, no delegation tool registered)
- `TelegramChannelConfig.enabled`: `false`
- `TelegramChannelConfig.senders`: empty vec (no senders allowed)

---

## 9. Testing Strategy

### Per-Step Test Requirements

| Step | Required Tests |
|------|---------------|
| 1: Context fix | Concurrent tool execution with different user_ids produces isolated contexts |
| 2: Protocol | v2 Hello accepted; non-v2 rejected; session_id and create_new_session fields work |
| 3: Multi-session | Two sessions run turns concurrently; concurrent turn limit; create_new_session and session_id paths |
| 4: Persistence | Session create → DB verify; restart recovery; touch/archive operations |
| 5: Lifecycle UX | /new creates session; /sessions lists; /switch changes; two TUI windows independent |
| 6: Auth pipeline | Known sender authorized; unknown rejected + audit; channels config optional; empty senders = nobody |
| 7: Channel mapping | First message creates mapping; subsequent reuse; /new updates mapping; file persists across restarts; forum topic → separate session; regular chat → single session |
| 8: Telegram | Update→auth→message construction; message splitting; command interception; channel_context_id derivation; markdown→HTML; edit-message throttling; turn-active rejection; mock API round-trip with streaming edits; 429 handling; reconnect; forum topic threading |
| 9: Agent defs | Config parsing; tool validation; system prompt augmentation |
| 10: Delegation | Round-trip delegation; budget cascading; cancel cascade; depth limit; cycle prevention |
| 11: Concurrency | Semaphore enforcement (top-level only); subagent bypass; no deadlock at depth 3; TTL cleanup with race protection; context eviction |
| 12: Integration | TUI + Telegram; delegation from Telegram; scheduled + interactive |

### Mock Strategy

- **MockProvider:** Already exists in `runtime` — reuse for all agent tests
- **Mock Telegram API:** Small axum server or mock at the `frankenstein::AsyncTelegramApi` trait boundary (only 2 methods to implement)
- **In-memory libSQL:** Already used in memory tests — reuse for session store tests
- **Mock DelegationExecutor:** For isolated delegation tool tests without real runtime

### CI Requirements

```yaml
# Fast path (no external features)
- cargo test --all

# Full path (with Telegram adapter)
- cargo test --all --features channels/telegram

# Quality gates (every step must pass)
- cargo fmt --check
- cargo clippy --all-targets --all-features -D warnings
- cargo test
- cargo deny check
```

---

## 10. Risk Analysis & Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| Gateway complexity increase | Medium | Medium | Gateway gains multi-session + persistence. Adapters are in-process but separate components calling gateway methods |
| Protocol version bump | Low | Low | All clients updated together; single version, no negotiation complexity |
| Telegram rate limits | Medium | Low | Accumulate deltas; respect `retry_after`; per-chat message queue |
| Subagent infinite loops | Low | High | Budget cascading; max depth; cycle prevention; parent cancel cascade |
| Session memory leak | Medium | Medium | TTL-based cleanup; bounded session map; semaphore for turn permits; turn runner context eviction on session archive |
| Tool context race (pre-fix) | **Confirmed** | High | **Fixed in Step 1** before any concurrent session work |
| Config complexity for users | Medium | Medium | All new sections optional with safe defaults; existing configs work unchanged |
| DB migration on running system | Low | Medium | Migrations are additive (new tables only); WAL mode enables concurrent reads during migration |
| Broadcast channel lag losing TurnCompleted | Medium | High | Increased capacity to 1024; Telegram adapter buffers via intermediate mpsc channel |
| Turn runner context HashMap OOM | Medium | Medium | Context eviction on session archive (Step 11); `drop_session_context()` on `GatewayTurnRunner` trait |
| Semaphore deadlock with delegation | **Eliminated** | High | Subagents run under parent's permit (D11); depth limit prevents fan-out |

### What We Are NOT Doing (Explicitly Deferred)

1. **State graph routing** — Declarative graph definitions for complex workflows. Start with simple delegation tool.
2. **Cross-agent communication** — Agents talking to each other without a parent. Delegation through parent only.
3. **Webhook mode for Telegram** — Long polling first. Webhook requires public URL.
4. **Slack/Discord/WhatsApp adapters** — Same adapter pattern. Trivially addable after Telegram proves the pattern. Discord uses the same internal gateway API (D10); WhatsApp may not support edit-message streaming (D15).
5. **Dynamic onboarding** — Invite codes, OAuth flows. Pre-configured binding is sufficient for now.
6. **Session migration across channels** — Starting a TUI session and continuing in Telegram. Sessions are independent per channel binding.
7. **Hot channel reload** — Adding/removing channels requires gateway restart.
8. **Observability/OpenTelemetry** — Phase 16 in guidebook; separate from this plan.
9. **Fire-and-forget (async) delegation** — A delegation mode where the parent tool returns immediately and the subagent runs in the background. The result is delivered to the user via a notification-style event (similar to `ScheduledNotification`). This would allow an orchestrator-style parent to handle multiple requests concurrently without blocking. Requires: async result delivery mechanism, follow-up context injection (worker results into parent history), per-worker cancellation UX. Build on top of the synchronous delegation model (Step 10) once it proves stable.
10. **Parallel delegation** — Multiple `delegate_to_agent` tool calls in one LLM response currently run sequentially (SideEffecting safety tier). Parallel delegation would require a new safety tier or explicit opt-in. Deferred until usage patterns clarify whether this is needed.

### File Change Summary

| File | Change Type | Step |
|------|------------|------|
| `crates/types/src/channel.rs` | Modified — protocol v2, new frames | 2, 5 |
| `crates/types/src/config.rs` | Modified — GatewayConfig, AgentDefinition | 9, 11 |
| `crates/types/src/runner.rs` | Modified — ChannelsConfig, SenderBinding in RunnerUserConfig | 6, 8 |
| `crates/types/src/session.rs` | **New** — SessionStore trait, SessionRecord | 4 |
| `crates/types/src/delegation.rs` | **New** — DelegationExecutor trait, request/result types | 10 |
| `crates/types/src/error.rs` | Modified — add delegation error variants | 10 |
| `crates/types/src/model.rs` | Modified — SubagentExecution progress kind | 10 |
| `crates/types/src/lib.rs` | Modified — re-export new modules | 4, 10 |
| `crates/runtime/src/lib.rs` | Modified — remove shared tool context, add context parameter | 1 |
| `crates/runtime/src/tool_execution.rs` | Modified — accept tool context parameter | 1 |
| `crates/runtime/src/delegation.rs` | **New** — RuntimeDelegationExecutor | 10 |
| `crates/gateway/src/lib.rs` | **Major evolution** — multi-session, protocol v2 | 3, 4, 5, 11 |
| `crates/gateway/src/session.rs` | **New** — UserState, SessionState | 3 |
| `crates/gateway/src/turn_runner.rs` | Modified — pass tool context directly | 1 |
| `crates/channels/Cargo.toml` | Modified — add telegram feature + deps | 8 |
| `crates/channels/src/lib.rs` | Modified — re-export telegram, sender_auth, session_map modules | 6, 7, 8 |
| `crates/channels/src/sender_auth.rs` | **New** — SenderAuthPolicy | 6 |
| `crates/channels/src/audit.rs` | **New** — AuditLogger for rejected senders | 6 |
| `crates/channels/src/session_map.rs` | **New** — ChannelSessionMap (wraps SessionStore) | 7 |
| `crates/channels/src/telegram.rs` | **New** — TelegramAdapter (in-process, calls gateway internal API), ResponseStreamer for edit-message streaming, markdown_to_telegram_html | 8 |
| `crates/memory/src/session_store.rs` | **New** — LibsqlSessionStore | 4, 7 |
| `crates/memory/src/schema.rs` | Modified — add migrations 0020-0021, required tables/indexes | 4, 7 |
| `crates/memory/migrations/0020_*.sql` | **New** — gateway_sessions | 4 |
| `crates/memory/migrations/0021_*.sql` | **New** — channel_session_mappings | 7 |
| `crates/tools/src/delegation_tools.rs` | **New** — delegate_to_agent tool | 10 |
| `crates/tools/src/lib.rs` | Modified — register_delegation_tools() | 10 |
| `crates/runner/src/bootstrap.rs` | Modified — delegation wiring, agent definitions | 9, 10 |
| `crates/runner/src/lib.rs` | Modified — channels config in bootstrap envelope, bot token env forwarding, session store wiring | 4, 6 |
| `crates/runner/src/bin/oxydra-vm.rs` | Modified — read channels from envelope, spawn adapters, session store | 4, 7, 8 |
| `crates/runner/Cargo.toml` | Modified — add channels dep with telegram feature | 8 |
| `crates/tui/src/app.rs` | Modified — /new, /sessions, /switch handling | 5 |
| `crates/tui/src/bin/oxydra-tui.rs` | Modified — --session flag, v2 Hello | 5 |
| `crates/tui/src/ui_model.rs` | Modified — session info display | 5 |
| `crates/tui/src/widgets.rs` | Modified — session info in status bar | 5 |
| `crates/tui/src/channel_adapter.rs` | Modified — new frame handling | 5 |

**Architecture:** Channel adapters run inside the VM alongside the gateway. They call gateway's internal API directly (D10) — no WebSocket. The WebSocket handler is a thin wrapper around the same internal API. The runner's only channel responsibility is including `ChannelsConfig` in the bootstrap envelope and forwarding bot token env vars. The existing `Channel` trait is for WebSocket-based client adapters (TUI) only; in-process adapters use the internal API (D13).

### New Crate Dependencies

| Crate | New Dependency | Version | Reason |
|-------|---------------|---------|--------|
| `channels` | `frankenstein` | `0.47` | Telegram Bot API client — raw 1:1 API wrapper, feature-gated. Uses reqwest 0.13 (same as Oxydra's existing stack). Chosen over teloxide for: reqwest version alignment, minimal dependency footprint (~4 new crates vs ~15-50), full control over update loop matching our in-process adapter architecture, newer API coverage (9.4 vs 9.1), and trivially mockable 2-method trait. |
| `channels` | `tokio` | `1` (rt, sync, time) | Async runtime for adapter (feature-gated, already in Oxydra) |
| `types` or `tui` | `uuid` | `1` (features: `v7`) | Time-ordered session IDs (add `v7` feature to existing `uuid` dep) |
