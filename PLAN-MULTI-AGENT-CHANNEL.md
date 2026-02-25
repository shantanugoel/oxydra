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
- `teloxide` pulls in a significant dependency tree
- Feature flags are standard Rust practice for optional transports
- CI tests without the flag stay fast; feature-flag CI job tests with it
- Guidebook is the canonical source

```toml
# channels/Cargo.toml
[features]
telegram = ["teloxide", "tokio"]
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
    ▼ (via teloxide long-polling, inside the VM)
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
| `GatewaySession` | `types/src/channel.rs` | `{ user_id, runtime_session_id }` |
| `GatewayClientHello` | `types/src/channel.rs` | `{ request_id, protocol_version, user_id, runtime_session_id? }` — will be updated: `runtime_session_id` → `session_id`, add `create_new_session` |
| `GatewayClientFrame` | `types/src/channel.rs` | `Hello, SendTurn, CancelActiveTurn, HealthCheck` |
| `GatewayServerFrame` | `types/src/channel.rs` | `HelloAck, TurnStarted, AssistantDelta, TurnCompleted, TurnCancelled, Error, HealthStatus, TurnProgress, ScheduledNotification` |
| `RuntimeProgressKind` | `types/src/model.rs` | `ProviderCall, ToolExecution { tool_names }, RollingSummary` |
| `GATEWAY_PROTOCOL_VERSION` | `types/src/channel.rs` | `1` |
| `AgentConfig` | `types/src/config.rs` | No `gateway`, `channels`, or `agents` fields. Channels config will go in `RunnerUserConfig` (host-side, per-user). Agent definitions will go here. |
| `ToolExecutionContext` | `types/src/tool.rs` | `{ user_id?, session_id? }` |

### Actual Gateway State (as-is)

```rust
// gateway/src/lib.rs
pub struct GatewayServer {
    turn_runner: Arc<dyn GatewayTurnRunner>,
    startup_status: Option<StartupStatusReport>,
    sessions: RwLock<HashMap<String, Arc<UserSessionState>>>,  // keyed by user_id
    next_connection_id: AtomicU64,
}

struct UserSessionState {
    user_id: String,
    runtime_session_id: String,                       // single session per user
    events: broadcast::Sender<GatewayServerFrame>,
    active_turn: Mutex<Option<ActiveTurnState>>,      // one turn at a time
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
gateway ← types, runtime, tools
tui ← types (standalone binary)
runner ← types, provider, tools, runtime, memory, gateway
```

Key: `channels` does NOT depend on `gateway`. `tools` does NOT depend on `runtime`. `gateway` does NOT depend on `memory` or `channels`.

### Actual Migrations

Last migration: `0019_create_schedule_runs_table.sql`
Required tables: 10 (`memory_migrations`, `sessions`, `conversation_events`, `session_state`, `files`, `chunks`, `chunks_vec`, `chunks_fts`, `schedules`, `schedule_runs`)
Required indexes: 11
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
│  ├── bot: teloxide::Bot (alice's bot token from env)            │
│  ├── authorized_senders: HashSet<platform_id>                   │
│  ├── session_map: chat_id → session_id                          │
│  └── Calls gateway methods directly (no WebSocket)              │
│                                                                 │
│  Message flow:                                                  │
│  Telegram API → teloxide poll → auth check → gateway.start_turn │
│                                                                 │
│  Response flow:                                                 │
│  gateway turn stream → accumulate deltas → bot.send_message()   │
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

### Step 1: Fix Tool Execution Context Race

**Goal:** Eliminate the shared mutable `ToolExecutionContext` before enabling concurrent sessions.

**Crates:** `types`, `runtime`, `gateway`

**Changes:**

1. **`runtime/src/lib.rs`:**
   - Remove the `tool_execution_context: Arc<Mutex<ToolExecutionContext>>` field from `AgentRuntime`
   - Remove the `set_tool_execution_context()` method
   - Add `tool_context: &ToolExecutionContext` parameter to `run_session_internal()`
   - Thread it through to `execute_tool_call()` and `execute_tool_and_format()`

2. **`runtime/src/tool_execution.rs`:**
   - Change `execute_tool_call()` to accept `tool_context: &ToolExecutionContext` instead of reading from `self.tool_execution_context`
   - Change `execute_tool_and_format()` similarly

3. **`runtime/src/lib.rs` public API:**
   - `run_session()` → add optional `tool_context` parameter (or new method variant)
   - `run_session_for_session()` → construct context from `session_id` parameter
   - `run_session_for_session_with_stream_events()` → construct context from parameters

4. **`gateway/src/turn_runner.rs`:**
   - `RuntimeGatewayTurnRunner::run_turn()` → construct `ToolExecutionContext { user_id, session_id }` locally and pass it to runtime, instead of calling `set_tool_execution_context()`

5. **`runtime/src/scheduler_executor.rs`:**
   - Update scheduled turn execution to pass context explicitly

**Verification:**
- All existing tests pass (`cargo test -p runtime -p gateway`)
- `cargo clippy --all-targets -D warnings` clean
- Add test: two concurrent `run_session_internal` calls with different user_ids produce isolated tool contexts

---

### Step 2: Protocol Update for Multi-Session + Channels

**Goal:** Update protocol version, add new Hello fields for session control, add new frame types. No backward compatibility with v1.

**Crates:** `types`, `gateway`, `tui`

**Changes:**

1. **`types/src/channel.rs`:**
   - Change `GATEWAY_PROTOCOL_VERSION` to `2`
   - Remove `GATEWAY_MIN_SUPPORTED_VERSION` / `GATEWAY_MAX_SUPPORTED_VERSION` if they exist (just one constant)
   - Update `GatewayClientHello`:
     ```rust
     pub struct GatewayClientHello {
         pub request_id: String,
         pub protocol_version: u16,
         pub user_id: String,
         #[serde(default, skip_serializing_if = "Option::is_none")]
         pub session_id: Option<String>,         // join existing session
         #[serde(default)]
         pub create_new_session: bool,            // create fresh session
     }
     ```
     Note: `runtime_session_id` renamed to `session_id` (no backward compat needed)

2. **`gateway/src/lib.rs`:**
   - Update protocol version check: reject anything `!= GATEWAY_PROTOCOL_VERSION`
   - Handle new Hello fields directly (no version negotiation logic)

3. **`tui`:**
   - Update Hello to send `protocol_version: 2`
   - Use `session_id` field (was `runtime_session_id`)

4. **`runner/src/lib.rs`:**
   - Update `probe_gateway_health()` to send v2 Hello

**Verification:**
- v2 Hello accepted
- Non-v2 Hello rejected
- `runner --tui --probe` works with updated Hello

---

### Step 3: Multi-Session Gateway Core

**Goal:** Replace single-session-per-user with multi-session model.

**Crates:** `gateway`

**Changes:**

1. **`gateway/src/lib.rs` (or new file `gateway/src/session.rs`):**
   - Define `UserState`:
     ```rust
     struct UserState {
         user_id: String,
         sessions: RwLock<HashMap<String, Arc<SessionState>>>,
         concurrent_turns: AtomicU32,
     }
     ```
   - Define `SessionState` (evolved from current `UserSessionState`):
     ```rust
     struct SessionState {
         session_id: String,
         user_id: String,
         agent_name: String,         // "default" initially
         parent_session_id: Option<String>,
         created_at: Instant,
         events: broadcast::Sender<GatewayServerFrame>,
         active_turn: Mutex<Option<ActiveTurnState>>,
         latest_terminal_frame: Mutex<Option<GatewayServerFrame>>,
     }
     ```

2. **Replace `GatewayServer.sessions`:**
   - From: `RwLock<HashMap<String, Arc<UserSessionState>>>` (keyed by user_id, one session)
   - To: `RwLock<HashMap<String, Arc<UserState>>>` (keyed by user_id, multiple sessions)

3. **Update `resolve_session()`:**
   - Hello with `create_new_session: true`: generate new UUID v7 session_id, create `SessionState`, add to user's session map
   - Hello with `session_id: Some(id)`: find existing session by that id, join it
   - Hello with neither: create a new session

4. **Update `start_turn()`:**
   - Move `active_turn` check to session level (already is, just formalize)
   - Add per-user concurrent turn counting via `UserState.concurrent_turns`
   - Enforce `max_concurrent_turns_per_user` (new config field, default 3)

5. **Update `SchedulerNotifier` implementation:**
   - `notify_user()` now iterates all sessions under a user and publishes to each

6. **Per-connection state tracking:**
   - In `handle_socket`, track `active_session_id` per connection
   - A connection's send/cancel operations only affect its active session

**Verification:**
- New test: two connections for same user with different sessions can run turns concurrently
- New test: concurrent turn limit enforcement (third concurrent turn rejected)
- New test: `create_new_session` creates fresh session
- New test: `session_id` joins existing session

---

### Step 4: Session Persistence

**Goal:** Sessions survive gateway restart. Session listing works from DB.

**Crates:** `types`, `memory`, `gateway`, `runner`

**Changes:**

1. **`types/src/session.rs` (new file):**
   - Define `SessionStore` trait:
     ```rust
     #[async_trait]
     pub trait SessionStore: Send + Sync {
         async fn create_session(&self, record: SessionRecord) -> Result<(), MemoryError>;
         async fn get_session(&self, session_id: &str) -> Result<Option<SessionRecord>, MemoryError>;
         async fn list_sessions(&self, user_id: &str, include_archived: bool) -> Result<Vec<SessionRecord>, MemoryError>;
         async fn touch_session(&self, session_id: &str) -> Result<(), MemoryError>;
         async fn archive_session(&self, session_id: &str) -> Result<(), MemoryError>;
     }

     #[derive(Debug, Clone)]
     pub struct SessionRecord {
         pub session_id: String,
         pub user_id: String,
         pub agent_name: String,
         pub display_name: Option<String>,
         pub channel_origin: String,
         pub parent_session_id: Option<String>,
         pub created_at: String,
         pub last_active_at: String,
         pub archived: bool,
     }
     ```

2. **`memory/src/session_store.rs` (new file):**
   - Implement `LibsqlSessionStore` using existing libSQL connection
   - Reuse `memory`'s connection pool/pattern

3. **`memory/src/schema.rs`:**
   - Add migration 0020 to `MIGRATIONS` array
   - Add `"gateway_sessions"` to `REQUIRED_TABLES`
   - Add new indexes to `REQUIRED_INDEXES`

4. **`memory/migrations/0020_create_gateway_sessions.sql` (new file):**
   - Create `gateway_sessions` table (see [Section 6](#6-database-migrations))

5. **`gateway/src/lib.rs`:**
   - Add `session_store: Option<Arc<dyn SessionStore>>` to `GatewayServer`
   - On session creation: persist to store
   - On turn completion: touch `last_active_at`
   - `list_sessions` reads from store

6. **`runner/src/bootstrap.rs` and `runner/src/bin/oxydra-vm.rs`:**
   - Build `LibsqlSessionStore` from memory backend
   - Pass to `GatewayServer` constructor
   - Update `GatewayServer::new()` signature

**Verification:**
- New test: create session, verify it exists in DB
- New test: restart gateway (reconstruct from DB), session is listed
- New test: `touch_session` updates `last_active_at`
- New test: `archive_session` marks as archived
- Migration test: fresh DB + upgrade DB both work

---

### Step 5: Session Lifecycle UX

**Goal:** `/new`, `/sessions`, `/switch` commands available in TUI and through protocol.

**Crates:** `types`, `gateway`, `tui`

**Changes:**

1. **`types/src/channel.rs` — new v2 client frames:**
   ```rust
   // Add to GatewayClientFrame enum:
   CreateSession(GatewayCreateSession),
   ListSessions(GatewayListSessions),
   SwitchSession(GatewaySwitchSession),

   pub struct GatewayCreateSession {
       pub request_id: String,
       pub display_name: Option<String>,
       pub agent_name: Option<String>,  // None → "default"
   }

   pub struct GatewayListSessions {
       pub request_id: String,
       pub include_archived: bool,
   }

   pub struct GatewaySwitchSession {
       pub request_id: String,
       pub session_id: String,  // maps to runtime_session_id internally
   }
   ```

2. **`types/src/channel.rs` — new v2 server frames:**
   ```rust
   // Add to GatewayServerFrame enum:
   SessionCreated(GatewaySessionCreated),
   SessionList(GatewaySessionList),
   SessionSwitched(GatewaySessionSwitched),
   ```

3. **`gateway/src/lib.rs`:**
   - Handle new client frame variants in `handle_client_frame()`
   - `CreateSession`: create new SessionState, persist, switch connection's active session
   - `ListSessions`: read from SessionStore
   - `SwitchSession`: unsubscribe from current session broadcast, subscribe to target session

4. **`tui/src/app.rs`:**
   - Intercept `/new`, `/sessions`, `/switch <id>` before sending as turns
   - Convert to appropriate `GatewayClientFrame` variants
   - Handle `SessionCreated`, `SessionList`, `SessionSwitched` server frames

5. **`tui/src/bin/oxydra-tui.rs`:**
   - Add `--session <id>` CLI flag
   - If provided: send it as `session_id` in Hello
   - If not provided: send `create_new_session: true` in Hello

6. **`tui/src/widgets.rs` and `tui/src/ui_model.rs`:**
   - Show current session ID (shortened) in status bar
   - Show `/new`, `/sessions` hints when idle

**Verification:**
- New test: `/new` creates session and switches to it
- New test: `/sessions` returns list
- New test: `/switch` changes active session
- New test: two TUI windows with separate sessions work independently
- TUI integration tests updated for v2 protocol and pass

---

### Step 6: Auth & Identity Pipeline

**Goal:** Sender authentication for external channels. Runs inside the VM alongside the gateway.

**Crates:** `types`, `channels`, `runner`

**Changes:**

1. **`types/src/runner.rs`:**
   - Add `SenderBinding` type (from Section 2)
   - Add `ChannelsConfig` to `RunnerUserConfig`:
     ```rust
     #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
     pub struct RunnerUserConfig {
         #[serde(default)]
         pub mounts: RunnerMountPaths,
         #[serde(default)]
         pub resources: RunnerResourceLimits,
         #[serde(default)]
         pub credential_refs: BTreeMap<String, String>,
         #[serde(default)]
         pub behavior: RunnerBehaviorOverrides,
         #[serde(default)]
         pub channels: ChannelsConfig,  // NEW
     }

     #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
     pub struct ChannelsConfig {
         #[serde(default)]
         pub telegram: Option<TelegramChannelConfig>,
         // Future: discord, whatsapp, etc.
     }

     #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
     pub struct TelegramChannelConfig {
         #[serde(default)]
         pub enabled: bool,
         pub bot_token_env: Option<String>,
         #[serde(default = "default_polling_timeout_secs")]
         pub polling_timeout_secs: u64,
         #[serde(default)]
         pub senders: Vec<SenderBinding>,
         #[serde(default = "default_max_message_length")]
         pub max_message_length: usize,
     }
     ```

2. **`types/src/runner.rs` — add channels to bootstrap envelope:**
   - Add `channels: Option<ChannelsConfig>` field to `RunnerBootstrapEnvelope` with `#[serde(default)]`
   - Runner populates it from the user's `RunnerUserConfig.channels`

3. **`runner/src/lib.rs` — forward channels config + bot token env vars:**
   - In `start_user_for_host()`: read `user_config.channels`, include in bootstrap envelope
   - In `collect_config_env_vars()`: also collect `bot_token_env` values from channels config

4. **`channels/src/sender_auth.rs` (new file):**
   - `SenderAuthPolicy` struct:
     ```rust
     pub struct SenderAuthPolicy {
         authorized: HashSet<String>, // all platform_ids from all sender bindings
     }

     impl SenderAuthPolicy {
         pub fn from_bindings(bindings: &[SenderBinding]) -> Self {
             let authorized = bindings.iter()
                 .flat_map(|b| b.platform_ids.iter().cloned())
                 .collect();
             Self { authorized }
         }
         pub fn is_authorized(&self, platform_id: &str) -> bool {
             self.authorized.contains(platform_id)
         }
     }
     ```

5. **`channels/src/audit.rs` (new file):**
   - `AuditLogger` for recording rejected senders
   - File-based implementation (writes to workspace log dir inside VM)

**Verification:**
- New test: known sender is authorized
- New test: unknown sender rejected, audit entry created
- Config test: channels config is optional (default = no channels)
- Config test: empty senders list means nobody can interact
- Config test: multiple platform_ids in one binding all authorize

---

### Step 7: Channel Session Mapping

**Goal:** Deterministic mapping from `(channel_id, channel_context_id)` to gateway session. Each Telegram chat maps to a stable session so conversation context is preserved.

**Crates:** `channels`, `memory`

**Changes:**

1. **`memory/migrations/0021_create_channel_session_mappings.sql` (new file):**
   ```sql
   CREATE TABLE IF NOT EXISTS channel_session_mappings (
       channel_id          TEXT NOT NULL,       -- e.g. "telegram"
       channel_context_id  TEXT NOT NULL,       -- e.g. Telegram chat_id
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
     - Look up existing session for this chat: found → use that session
     - Not found → create new session via gateway, save mapping
   - `/new` command → create new session, update mapping
   - `/switch` command → update mapping to different session_id

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
   telegram = ["teloxide", "tokio"]

   [dependencies]
   types = { path = "../types" }
   teloxide = { version = "0.13", optional = true }
   tokio = { version = "1", features = ["full"], optional = true }
   serde = { version = "1", features = ["derive"] }
   serde_json = "1"
   tracing = "0.1"
   ```

2. **`channels/src/telegram.rs` (new file, behind `#[cfg(feature = "telegram")]`):**
   - `TelegramAdapter` struct:
     ```rust
     pub struct TelegramAdapter {
         bot: teloxide::Bot,
         sender_auth: SenderAuthPolicy,
         session_store: Arc<dyn SessionStore>,   // for channel session mappings
         gateway: Arc<GatewayServer>,            // direct reference, in-process
         user_id: String,
         config: TelegramChannelConfig,
     }
     ```
   - `run()` method — long-polls Telegram updates, runs until cancellation token fires
   - Per incoming `Update`:
     - Extract `sender_id` from `message.from.id`
     - `sender_auth.is_authorized(sender_id)` → false = drop + audit, true = proceed
     - Command interception: `/new`, `/sessions`, `/switch`, `/cancel`, `/status`
       - These map to direct gateway method calls (create session, list sessions, etc.)
     - Normal message: resolve session via `session_store.get_channel_session()`, call `gateway.start_turn()` directly
   - Response handling:
     - Subscribe to session's broadcast channel (same mechanism TUI WebSocket uses internally)
     - Accumulate `AssistantDelta` text fragments
     - On `TurnCompleted`: format accumulated text, split at 4096 char limit at paragraph boundaries
     - Send via `bot.send_message()` with HTML formatting
     - Fallback to plain text on formatting errors
   - Rate limiting: respect Telegram's `retry_after` header on 429 responses
   - Per-chat message queue to avoid out-of-order delivery

3. **`runner/src/bin/oxydra-vm.rs`:**
   - After gateway + runtime construction:
     - Read `ChannelsConfig` from bootstrap envelope
     - If Telegram enabled: resolve bot token from env, build `SenderAuthPolicy`, construct `TelegramAdapter`
     - Spawn adapter with the VM's `CancellationToken`
   - Adapter stops automatically when VM shuts down

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
- Integration test with mock Telegram API: full message round-trip
- Integration test: unauthorized sender rejected, audit logged
- Integration test: rate limiting with simulated 429

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
   - Subagent turns also acquire parent user's permit (preventing fan-out bypass)

3. **Session cleanup background task:**
   - Spawn a periodic task (every 5 minutes) that:
     - Checks sessions with no subscribers and idle > TTL
     - Archives them in DB
     - Removes from in-memory map
   - Uses the gateway's `CancellationToken` for clean shutdown

**Verification:**
- Test: concurrent turn limit enforced
- Test: subagent turns count toward user's concurrent limit
- Test: session TTL cleanup archives stale sessions
- Test: archived session data remains in DB for listing

---

### Step 12: Integration Polish

**Goal:** End-to-end reliability, cross-feature interactions, docs.

**Changes:**

1. **System prompt updates:**
   - When agents are defined: add section about delegation capabilities

2. **Guidebook updates:**
   - Update Chapter 9 (Gateway and Channels) with multi-session model
   - Update Chapter 12 (External Channels) with auth/identity model
   - Update Chapter 14 (Productization) with session lifecycle
   - Update Chapter 15 (Build Plan) with completion status

3. **Integration tests:**
   - TUI + Telegram simultaneous sessions
   - Delegation from Telegram session
   - Session switching mid-conversation
   - Scheduled task alongside interactive session + delegation

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
| 7: Channel mapping | First message creates mapping; subsequent reuse; /new updates mapping; file persists across restarts |
| 8: Telegram | Update→auth→message construction; message splitting; command interception; mock API round-trip; 429 handling; reconnect |
| 9: Agent defs | Config parsing; tool validation; system prompt augmentation |
| 10: Delegation | Round-trip delegation; budget cascading; cancel cascade; depth limit; cycle prevention |
| 11: Concurrency | Semaphore enforcement; subagent counting; TTL cleanup |
| 12: Integration | TUI + Telegram; delegation from Telegram; scheduled + interactive |

### Mock Strategy

- **MockProvider:** Already exists in `runtime` — reuse for all agent tests
- **Mock Telegram API:** Small axum server or mock at the `teloxide::Bot` trait boundary
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
| Session memory leak | Medium | Medium | TTL-based cleanup; bounded session map; semaphore for turn permits |
| Tool context race (pre-fix) | **Confirmed** | High | **Fixed in Step 1** before any concurrent session work |
| Config complexity for users | Medium | Medium | All new sections optional with safe defaults; existing configs work unchanged |
| DB migration on running system | Low | Medium | Migrations are additive (new tables only); WAL mode enables concurrent reads during migration |

### What We Are NOT Doing (Explicitly Deferred)

1. **State graph routing** — Declarative graph definitions for complex workflows. Start with simple delegation tool.
2. **Cross-agent communication** — Agents talking to each other without a parent. Delegation through parent only.
3. **Webhook mode for Telegram** — Long polling first. Webhook requires public URL.
4. **Slack/Discord/WhatsApp adapters** — Same `Channel` trait pattern. Trivially addable after Telegram proves the pattern.
5. **Dynamic onboarding** — Invite codes, OAuth flows. Pre-configured binding is sufficient for now.
6. **Session migration across channels** — Starting a TUI session and continuing in Telegram. Sessions are independent per channel binding.
7. **Hot channel reload** — Adding/removing channels requires gateway restart.
8. **Observability/OpenTelemetry** — Phase 16 in guidebook; separate from this plan.

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
| `crates/channels/src/telegram.rs` | **New** — TelegramAdapter (in-process, calls gateway directly) | 8 |
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

**Architecture:** Channel adapters run inside the VM alongside the gateway. They call gateway methods directly (no WebSocket). The runner's only channel responsibility is including `ChannelsConfig` in the bootstrap envelope and forwarding bot token env vars.

### New Crate Dependencies

| Crate | New Dependency | Version | Reason |
|-------|---------------|---------|--------|
| `channels` | `teloxide` | `0.13` | Telegram bot framework (feature-gated) |
| `channels` | `tokio` | `1` (full) | Async runtime for adapter (feature-gated) |
| `types` or `tui` | `uuid` | `1` (features: `v7`) | Time-ordered session IDs (add `v7` feature to existing `uuid` dep) |
