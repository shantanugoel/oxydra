# Final Plan: External Channels + Multi-Agent + Session Lifecycle + Auth/Identity

**Date:** 2026-02-25
**Status:** Consolidated from PLAN-MULTI-AGENT-CHANNEL.md, REPORT-GPT.MD, BEST-PLAN-EVER.md, codebase review, and auth/onboarding analysis.
**Guidebook phases covered:** 14 (External Channels + Identity), 15 (Multi-Agent), 18 (Session Lifecycle)

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

Issues raised across the three plans, resolved with rationale grounded in the actual codebase.

### D1: Default session behavior (C1 from REPORT-GPT)

**Conflict:** Original plan said both "new session per TUI window" AND "resume most recent if session_id absent."

**Resolution:** Server behavior is deterministic and version-aware. The _client_ decides what it wants:

- **v1 clients** (current TUI): Omit `session_id` in Hello → gateway preserves current behavior (single stable `runtime-{user_id}` session, same as today).
- **v2 clients**: Must be explicit — send `create_new_session: true` for a fresh session, or `session_id: "<id>"` to join an existing one. If neither is set, gateway creates a new session (no ambiguous resume heuristic).
- **v2 TUI default**: New TUI launch without `--session` sends `create_new_session: true`. Reconnection sends the `session_id` received from prior HelloAck.

This eliminates all ambiguity.

### D2: Delegation tool crate boundary (C2 from REPORT-GPT)

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

### D3: Protocol compatibility (C3 from REPORT-GPT)

**Resolution:**
- Track `negotiated_protocol_version` per WebSocket connection after Hello handshake
- Gateway accepts v1 AND v2 in the version check (instead of `!= 1`, check `> GATEWAY_MAX_SUPPORTED_VERSION`)
- For v1 connections: never emit new frame variants; new `RuntimeProgressKind` variants are filtered out
- For v2 connections: full new frame support
- Bump `GATEWAY_PROTOCOL_VERSION` constant to `2`; add `GATEWAY_MIN_SUPPORTED_VERSION = 1`

### D4: Session persistence (C4 from REPORT-GPT)

**Conflict:** Gateway currently has zero persistence; plan assumes DB access.

**Resolution:**
- Define `SessionStore` trait in `types` crate (boundary-safe)
- Implement `LibsqlSessionStore` in `memory` crate (reuses existing libSQL connection infrastructure)
- Inject into gateway via constructor at bootstrap time (runner wires it)
- Update `memory/src/schema.rs` MIGRATIONS list AND `REQUIRED_TABLES`/`REQUIRED_INDEXES` arrays
- In-memory session state is authoritative for active sessions; DB is for listing/resumption after restart

### D5: Subagent progress transport (C5 from REPORT-GPT)

**Conflict:** Plan uses both `TurnProgress` extension AND separate `SubagentProgress` frame.

**Resolution:** Single path only. Extend existing `RuntimeProgressKind` with a new variant:
```rust
RuntimeProgressKind::SubagentExecution {
    agent_name: String,
    subagent_session_id: String,
}
```
Reuse existing `GatewayServerFrame::TurnProgress`. No new frame type. v1 connections skip this variant.

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

### D7: Tool execution context race (from BEST-PLAN-EVER)

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

### D8: Session lifetime and memory pressure (H4 from REPORT-GPT)

**Resolution:**
- Active session state (broadcast channels, turn locks) lives in memory
- Conversation history lives in DB (already the case via `conversation_events`)
- Gateway maintains a bounded in-memory session map; sessions are evicted from memory after `session_idle_ttl` with no subscribers
- Evicted sessions can be resumed by reloading from DB
- Gateway-level session registry (DB) tracks all sessions for listing/searching

### D9: `runtime_session_id` naming (Decision 6 from original plan)

**Resolution:** Keep `runtime_session_id` in the wire protocol and existing types. The field appears in 150+ references across 15 files. Renaming has massive blast radius with zero functional benefit. New session lifecycle frames use `session_id` as the user-facing field, which maps to `runtime_session_id` internally.

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
[channels.telegram]
enabled = true
bot_token_env = "TELEGRAM_BOT_TOKEN"

# Sender identity bindings — maps platform sender IDs to Oxydra identity + role
[[channels.telegram.senders]]
platform_id = "12345678"        # Telegram user ID
user_id = "alice"               # Oxydra user identity
role = "owner"                  # Full access

[[channels.telegram.senders]]
platform_id = "87654321"
user_id = "alice"               # Same Oxydra user — this is alice's friend
role = "guest"                  # Limited access
display_name = "Bob"            # Optional label for the agent to see
```

### Role Hierarchy

| Role | Description | Capabilities |
|------|-------------|-------------|
| `owner` | The primary Oxydra user. Full access. | All tools, all operations, budget as configured |
| `member` | A trusted collaborator. Can interact normally. | All read tools, side-effecting tools gated by config |
| `guest` | A known sender with limited access. | Read-only interactions; messages prefixed with sender identity for the agent |

**Behavior per role:**

1. **Owner messages** → processed as normal user turns. The agent sees them as `MessageRole::User`.
2. **Member messages** → processed as user turns but tools may be restricted (configurable tool allowlist per role).
3. **Guest messages** → the agent receives the message with metadata indicating it's from a guest. The system prompt tells the agent how to handle guests. Guests cannot trigger side-effecting tools directly.
4. **Unknown senders** → rejected silently. Audit log entry created. No response sent (prevents enumeration).

### Group Channel Scenario (Discord)

When the bot is added to a Discord server/channel:

```toml
[channels.discord]
enabled = true
bot_token_env = "DISCORD_BOT_TOKEN"

# Which Discord channels (guild+channel IDs) the bot should listen to
allowed_channel_ids = ["1234567890123456"]

[[channels.discord.senders]]
platform_id = "111222333444555666"  # Discord user ID
user_id = "alice"
role = "owner"

[[channels.discord.senders]]
platform_id = "666555444333222111"
user_id = "alice"                   # All senders map to same Oxydra user
role = "guest"
display_name = "Charlie"
```

In a group channel, the agent sees:
```
[alice (owner)]: Can you summarize the discussion?
[Charlie (guest)]: I think the API should use REST
[Dave (unknown)] → silently dropped, audit logged
```

The agent knows who is speaking and their trust level. The system prompt instructs the agent on how to handle different roles.

### Why Not Dynamic Onboarding?

For v1, we deliberately avoid invite-code or OAuth flows because:
1. They add attack surface (invite code leakage, phishing)
2. They require state management for pending invites
3. They're unnecessary for the primary use case (personal agent)
4. Pre-configured binding is zero-trust: only the operator with file system access can authorize senders

Dynamic onboarding (invite codes, admin commands) can be added later as an enhancement _on top of_ the static binding model.

### Config-Level Identity Types

```rust
// In types/src/config.rs

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SenderRole {
    Owner,
    Member,
    Guest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SenderBinding {
    /// Platform-specific sender identifier (Telegram user_id, Discord user_id, etc.)
    pub platform_id: String,
    /// Oxydra user identity this sender maps to
    pub user_id: String,
    /// Access level
    pub role: SenderRole,
    /// Optional human-readable name for the agent to see in group contexts
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Optional tool allowlist override for this sender (None = role default)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<Vec<String>>,
}
```

### Runtime Identity Propagation

When a message arrives from an external channel:

```
Platform message (Telegram user_id: 12345678)
    │
    ▼
Channel Adapter: extract sender platform_id
    │
    ▼
Sender Auth Pipeline:
    ├── Look up platform_id in channel's sender bindings
    ├── NOT FOUND → reject, audit log, done
    ├── FOUND → extract (user_id, role, display_name)
    │
    ▼
Construct ChannelInboundEvent with identity metadata:
    ChannelInboundEvent {
        channel_id: "telegram",
        connection_id: "tg-12345678",
        sender_identity: SenderIdentity {
            user_id: "alice",
            role: Owner,
            display_name: None,
            platform_id: "12345678",
        },
        frame: GatewayClientFrame::SendTurn { ... }
    }
    │
    ▼
Gateway: route to user_id's session
    ├── Owner/Member → standard turn execution
    └── Guest → turn execution with restricted tool set,
                message prefixed with guest identity for agent awareness
```

---

## 3. Codebase Ground Truth

Current state verified against actual code, not plan assumptions.

### Actual Types (as-is)

| Type | Location | Current State |
|------|----------|---------------|
| `GatewaySession` | `types/src/channel.rs` | `{ user_id, runtime_session_id }` |
| `GatewayClientHello` | `types/src/channel.rs` | `{ request_id, protocol_version, user_id, runtime_session_id? }` |
| `GatewayClientFrame` | `types/src/channel.rs` | `Hello, SendTurn, CancelActiveTurn, HealthCheck` |
| `GatewayServerFrame` | `types/src/channel.rs` | `HelloAck, TurnStarted, AssistantDelta, TurnCompleted, TurnCancelled, Error, HealthStatus, TurnProgress, ScheduledNotification` |
| `RuntimeProgressKind` | `types/src/model.rs` | `ProviderCall, ToolExecution { tool_names }, RollingSummary` |
| `GATEWAY_PROTOCOL_VERSION` | `types/src/channel.rs` | `1` |
| `AgentConfig` | `types/src/config.rs` | No `gateway`, `channels`, or `agents` fields |
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
channels ← types (only)
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
│  │  ├── negotiated_protocol_version: u16                     │   │
│  │  └── active_session_id: String                            │   │
│  └──────────────────────────────────────────────────────────┘   │
└──────────────────────────────────────────────────────────────────┘
```

### 4.2 Identity & Auth Flow

```
External Channel Message
        │
        ▼
Channel Adapter: extract sender platform_id
        │
        ▼
Sender Auth Pipeline (in gateway):
        │
        ├── Lookup sender in channel config senders list
        │     │
        │     ├── NOT FOUND → audit log + silent drop
        │     │
        │     └── FOUND → SenderIdentity { user_id, role, ... }
        │
        ▼
Role-Based Routing:
        │
        ├── Owner/Member → resolve session → start turn (full tools or role-scoped tools)
        │
        └── Guest → resolve session → start turn with:
                - restricted tool registry (read-only by default)
                - message prefixed with "[Guest: {display_name}]"
                - lower budget limits (configurable)
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

### Step 2: Protocol Negotiation & v1/v2 Compatibility

**Goal:** Gateway accepts both v1 and v2 clients. Lay groundwork for new frames without breaking existing TUI.

**Crates:** `types`, `gateway`

**Changes:**

1. **`types/src/channel.rs`:**
   - Change `GATEWAY_PROTOCOL_VERSION` to `2`
   - Add `const GATEWAY_MIN_PROTOCOL_VERSION: u16 = 1;`
   - Add `const GATEWAY_MAX_PROTOCOL_VERSION: u16 = 2;`

2. **`gateway/src/lib.rs`:**
   - Change protocol version check from `!= GATEWAY_PROTOCOL_VERSION` to:
     ```rust
     if hello.protocol_version < GATEWAY_MIN_PROTOCOL_VERSION
         || hello.protocol_version > GATEWAY_MAX_PROTOCOL_VERSION
     ```
   - Store `negotiated_protocol_version` alongside connection state (initially as a local variable in `handle_socket`, threaded through `handle_client_frame`)
   - Before emitting any frame to a connection, check if the frame is supported at the negotiated version
   - For v1 connections: filter out new `RuntimeProgressKind` variants in `TurnProgress` frames; skip new frame types entirely

3. **`types/src/channel.rs`:**
   - Add v2-only fields to `GatewayClientHello` (backward-compatible via `serde(default)`):
     ```rust
     #[serde(default)]
     pub create_new_session: bool,
     // session_id already exists as runtime_session_id
     ```

**Verification:**
- Existing TUI (sends v1 Hello) still connects and works identically
- New test: v2 Hello accepted, v0 and v3 rejected
- New test: v1 connection does not receive v2-only frames
- `runner --tui --probe` still works (it uses v1 Hello)

---

### Step 3: Multi-Session Gateway Core

**Goal:** Replace single-session-per-user with multi-session model. v1 clients see zero behavior change.

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
   - v1 Hello: preserve exact current behavior — use `runtime-{user_id}` as session_id, find-or-create single session for the user (backward compatible)
   - v2 Hello with `create_new_session: true`: generate new UUID v7 session_id, create `SessionState`, add to user's session map
   - v2 Hello with `runtime_session_id: Some(id)`: find existing session by that id, join it
   - v2 Hello with neither: create a new session (no ambiguous resume)

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
- All existing gateway tests pass (v1 backward compatibility)
- New test: two v2 connections for same user with different sessions can run turns concurrently
- New test: concurrent turn limit enforcement (third concurrent turn rejected)
- New test: v1 client still gets stable `runtime-{user_id}` session

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
   - v1 connections: these frames never arrive (old TUI doesn't send them)
   - `CreateSession`: create new SessionState, persist, switch connection's active session
   - `ListSessions`: read from SessionStore
   - `SwitchSession`: unsubscribe from current session broadcast, subscribe to target session

4. **`tui/src/app.rs`:**
   - Intercept `/new`, `/sessions`, `/switch <id>` before sending as turns
   - Convert to appropriate `GatewayClientFrame` variants
   - Handle `SessionCreated`, `SessionList`, `SessionSwitched` server frames

5. **`tui/src/bin/oxydra-tui.rs`:**
   - Add `--session <id>` CLI flag
   - If provided: send it as `runtime_session_id` in Hello
   - If not provided: send `create_new_session: true` in v2 Hello

6. **`tui/src/widgets.rs` and `tui/src/ui_model.rs`:**
   - Show current session ID (shortened) in status bar
   - Show `/new`, `/sessions` hints when idle

**Verification:**
- New test: `/new` creates session and switches to it
- New test: `/sessions` returns list
- New test: `/switch` changes active session
- New test: two TUI windows with separate sessions work independently
- Existing TUI integration tests still pass

---

### Step 6: Auth & Identity Pipeline

**Goal:** Sender authentication and identity resolution for external channels. Framework before any specific adapter.

**Crates:** `types`, `gateway`, `memory`

**Changes:**

1. **`types/src/config.rs`:**
   - Add `SenderRole`, `SenderBinding` types (from Section 2)
   - Add `ChannelsConfig` (initially just shared auth config structure)
   - Add `channels: ChannelsConfig` field to `AgentConfig` with `#[serde(default)]`

2. **`types/src/channel.rs`:**
   - Add `SenderIdentity` struct:
     ```rust
     pub struct SenderIdentity {
         pub user_id: String,
         pub role: SenderRole,
         pub platform_id: String,
         pub display_name: Option<String>,
     }
     ```
   - Add `sender_identity: Option<SenderIdentity>` to `ChannelInboundEvent`
     (None for TUI/WebSocket where identity is in the Hello handshake)

3. **`gateway/src/sender_auth.rs` (new file):**
   - `SenderAuthPolicy` struct:
     ```rust
     pub struct SenderAuthPolicy {
         bindings: HashMap<String, HashMap<String, SenderBinding>>, // channel_id → platform_id → binding
     }

     impl SenderAuthPolicy {
         pub fn authenticate(&self, channel_id: &str, platform_id: &str) -> Option<&SenderBinding> { ... }
     }
     ```
   - Build from config at startup

4. **`memory/migrations/0021_create_sender_audit_log.sql` (new file):**
   - Audit table for rejected senders

5. **`memory/src/session_store.rs`:**
   - Add `log_rejected_sender()` method to `SessionStore` trait
   - Add `LibsqlSessionStore` implementation

6. **`memory/src/schema.rs`:**
   - Add migration 0021 to MIGRATIONS
   - Add `"sender_audit_log"` to REQUIRED_TABLES

7. **`gateway/src/lib.rs`:**
   - Add `sender_auth: Option<SenderAuthPolicy>` to `GatewayServer`
   - Add channel inbound routing method:
     ```rust
     pub async fn handle_channel_inbound(&self, event: ChannelInboundEvent) { ... }
     ```
   - This method: validates sender → resolves session → starts turn

**Verification:**
- New test: known sender authenticates successfully
- New test: unknown sender rejected, audit log created
- New test: guest sender gets restricted tool set
- New test: owner sender gets full access

---

### Step 7: Channel Session Mapping

**Goal:** Deterministic mapping from `(user_id, channel_id, channel_context_id)` to session_id.

**Crates:** `types`, `memory`, `gateway`

**Changes:**

1. **`memory/migrations/0022_create_channel_session_mappings.sql` (new file):**
   - See [Section 6](#6-database-migrations)

2. **`memory/src/schema.rs`:**
   - Add migration 0022
   - Add `"channel_session_mappings"` to REQUIRED_TABLES

3. **`types/src/session.rs`:**
   - Add to `SessionStore` trait:
     ```rust
     async fn get_channel_session(
         &self,
         user_id: &str,
         channel_id: &str,
         channel_context_id: &str,
     ) -> Result<Option<String>, MemoryError>;

     async fn set_channel_session(
         &self,
         user_id: &str,
         channel_id: &str,
         channel_context_id: &str,
         session_id: &str,
     ) -> Result<(), MemoryError>;
     ```

4. **`gateway/src/lib.rs`:**
   - Channel inbound routing uses the mapping:
     - Incoming Telegram message from chat_id `12345` for user "alice"
     - Lookup `("alice", "telegram", "12345")` → found session_id → use it
     - Not found → create new session, store mapping

**Verification:**
- New test: first message from a channel creates mapping
- New test: subsequent messages from same channel reuse session
- New test: `/new` on channel creates new session and updates mapping

---

### Step 8: Telegram Channel Adapter

**Goal:** Working Telegram bot that receives messages, authenticates senders, and routes through gateway.

**Crates:** `channels`, `gateway`, `runner`

**Changes:**

1. **`channels/Cargo.toml`:**
   ```toml
   [features]
   default = []
   telegram = ["teloxide", "tokio"]

   [dependencies]
   teloxide = { version = "0.13", optional = true }
   tokio = { version = "1", features = ["full"], optional = true }
   ```

2. **`channels/src/telegram.rs` (new file, behind `#[cfg(feature = "telegram")]`):**
   - `TelegramAdapter` implementing `Channel` trait
   - Long-polling update listener via teloxide
   - Telegram `Update` → `ChannelInboundEvent` conversion:
     - Extract `sender_id` from `message.from.id`
     - Set `connection_id` to `tg-{chat_id}`
     - Map message text to `GatewayClientFrame::SendTurn`
   - `ChannelOutboundEvent` → `bot.send_message()` conversion:
     - Accumulate `AssistantDelta` frames until `TurnCompleted`
     - Split messages at 4096 char Telegram limit at paragraph boundaries
     - Use Telegram HTML formatting (more forgiving than MarkdownV2)
     - Fallback to plain text on formatting errors
   - Command interception: `/new`, `/sessions`, `/switch`, `/cancel`, `/status`
   - Rate limiting: respect Telegram's `retry_after` header on 429 responses
   - Per-chat message queue to avoid out-of-order delivery

3. **`types/src/config.rs`:**
   - Add `TelegramChannelConfig`:
     ```rust
     pub struct TelegramChannelConfig {
         pub enabled: bool,                    // default: false
         pub bot_token_env: Option<String>,    // env var name for bot token
         pub polling_timeout_secs: u64,        // default: 30
         pub senders: Vec<SenderBinding>,      // sender identity bindings
         pub max_message_length: usize,        // default: 4096
     }
     ```
   - Add to `ChannelsConfig`:
     ```rust
     pub struct ChannelsConfig {
         #[serde(default)]
         pub telegram: TelegramChannelConfig,
     }
     ```

4. **`runner/src/bootstrap.rs`:**
   - When `channels.telegram.enabled` is true AND feature is compiled in:
     - Resolve bot token from env var
     - Construct `TelegramAdapter`
     - Build `SenderAuthPolicy` from `channels.telegram.senders`
     - Pass adapter and auth policy to gateway

5. **`runner/src/bin/oxydra-vm.rs`:**
   - After gateway construction: register Telegram adapter
   - Spawn adapter listener task
   - Wire inbound events to `gateway.handle_channel_inbound()`

6. **`runner/Cargo.toml`:**
   - Add `channels` dependency with optional `telegram` feature pass-through

**Verification:**
- Unit test: Telegram Update → ChannelInboundEvent conversion
- Unit test: long message splitting at paragraph boundaries
- Unit test: command interception (`/new`, etc.)
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
   - When channels are configured: add section about multi-channel awareness
   - When agents are defined: add section about delegation capabilities
   - When guest senders exist: add section about how to handle guest messages
     ```
     ## Guest Users
     Some messages may come from guest users, indicated by a "[Guest: name]" prefix.
     Treat guest messages as informational. Do not execute side-effecting tools on behalf of guests.
     You may respond to guests with helpful information but prioritize the owner's requests.
     ```

2. **Guidebook updates:**
   - Update Chapter 9 (Gateway and Channels) with multi-session model
   - Update Chapter 12 (External Channels) with auth/identity model
   - Update Chapter 14 (Productization) with session lifecycle
   - Update Chapter 15 (Build Plan) with completion status

3. **Integration tests:**
   - TUI + Telegram simultaneous sessions
   - Delegation from Telegram session
   - Guest message handling
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

### Migration 0021: sender_audit_log

```sql
-- 0021_create_sender_audit_log.sql
CREATE TABLE IF NOT EXISTS sender_audit_log (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    channel_id    TEXT NOT NULL,
    platform_id   TEXT NOT NULL,
    user_id_hint  TEXT,
    reason        TEXT NOT NULL,
    rejected_at   TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_sender_audit_channel_time
    ON sender_audit_log(channel_id, rejected_at DESC);
```

### Migration 0022: channel_session_mappings

```sql
-- 0022_create_channel_session_mappings.sql
CREATE TABLE IF NOT EXISTS channel_session_mappings (
    user_id             TEXT NOT NULL,
    channel_id          TEXT NOT NULL,
    channel_context_id  TEXT NOT NULL,
    session_id          TEXT NOT NULL REFERENCES gateway_sessions(session_id) ON DELETE CASCADE,
    created_at          TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at          TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (user_id, channel_id, channel_context_id)
);
```

### Schema verification additions

In `memory/src/schema.rs`, add to `REQUIRED_TABLES`:
```rust
"gateway_sessions",
"sender_audit_log",
"channel_session_mappings",
```

Add to `REQUIRED_INDEXES`:
```rust
"idx_gateway_sessions_user_active",
"idx_gateway_sessions_parent",
"idx_sender_audit_channel_time",
```

---

## 7. Protocol Evolution

### Modified Client Frame Types

```rust
// GatewayClientHello (v1 fields unchanged, v2 adds create_new_session)
pub struct GatewayClientHello {
    pub request_id: String,
    pub protocol_version: u16,
    pub user_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_session_id: Option<String>,
    #[serde(default)]
    pub create_new_session: bool,  // NEW: v2 only, ignored by v1 gateway path
}

// New v2 client frames (added to GatewayClientFrame enum)
CreateSession(GatewayCreateSession),
ListSessions(GatewayListSessions),
SwitchSession(GatewaySwitchSession),
```

### Modified Server Frame Types

```rust
// New v2 server frames (added to GatewayServerFrame enum)
SessionCreated(GatewaySessionCreated),
SessionList(GatewaySessionList),
SessionSwitched(GatewaySessionSwitched),

// RuntimeProgressKind gains:
SubagentExecution {
    agent_name: String,
    subagent_session_id: String,
}
```

### v1/v2 Compatibility Matrix

| Frame | v1 Client | v2 Client |
|-------|-----------|-----------|
| Hello (v1 fields) | ✅ Normal | ✅ Accepted |
| Hello (create_new_session) | Ignored (serde default) | ✅ Processed |
| SendTurn | ✅ Unchanged | ✅ Unchanged |
| CancelActiveTurn | ✅ Unchanged | ✅ Unchanged |
| HealthCheck | ✅ Unchanged | ✅ Unchanged |
| CreateSession | N/A (old TUI never sends) | ✅ Processed |
| ListSessions | N/A | ✅ Processed |
| SwitchSession | N/A | ✅ Processed |
| HelloAck | ✅ Unchanged | ✅ Unchanged |
| AssistantDelta | ✅ Unchanged | ✅ Unchanged |
| TurnCompleted | ✅ Unchanged | ✅ Unchanged |
| TurnProgress (new kinds) | **Filtered out** | ✅ Emitted |
| SessionCreated | **Never emitted** | ✅ Emitted |
| SessionList | **Never emitted** | ✅ Emitted |
| SessionSwitched | **Never emitted** | ✅ Emitted |

**Critical rule:** The gateway tracks negotiated version per connection. For v1 connections, new server frame variants are never serialized to the wire. This prevents `serde` deserialization failures in old TUI binaries.

---

## 8. Configuration Schema

### Full Example

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

# ── Channels ────────────────────────────────────────────────
[channels.telegram]
enabled = true
bot_token_env = "TELEGRAM_BOT_TOKEN"
polling_timeout_secs = 30
max_message_length = 4096

# Telegram sender bindings
[[channels.telegram.senders]]
platform_id = "12345678"
user_id = "alice"
role = "owner"

[[channels.telegram.senders]]
platform_id = "87654321"
user_id = "alice"
role = "guest"
display_name = "Bob"

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

### Defaults

All new config sections use `#[serde(default)]` so existing configs continue to work without modification:
- `gateway`: defaults defined in impl, no sessions limit = 10, no concurrency limit = 3
- `channels.telegram.enabled`: `false`
- `channels.telegram.senders`: empty vec (no senders allowed)
- `agents`: empty BTreeMap (no named agents, no delegation tool registered)

---

## 9. Testing Strategy

### Per-Step Test Requirements

| Step | Required Tests |
|------|---------------|
| 1: Context fix | Concurrent tool execution with different user_ids produces isolated contexts |
| 2: Protocol | v1 Hello works unchanged; v2 Hello accepted; v0/v3 rejected; v1 client doesn't receive v2 frames |
| 3: Multi-session | Two v2 sessions run turns concurrently; v1 backward compat; concurrent turn limit |
| 4: Persistence | Session create → DB verify; restart recovery; touch/archive operations |
| 5: Lifecycle UX | /new creates session; /sessions lists; /switch changes; two TUI windows independent |
| 6: Auth pipeline | Known sender authenticates; unknown rejected + audit; role-based routing |
| 7: Channel mapping | First message creates mapping; subsequent reuse; /new updates mapping |
| 8: Telegram | Update conversion; message splitting; command interception; mock API round-trip; 429 handling |
| 9: Agent defs | Config parsing; tool validation; system prompt augmentation |
| 10: Delegation | Round-trip delegation; budget cascading; cancel cascade; depth limit; cycle prevention |
| 11: Concurrency | Semaphore enforcement; subagent counting; TTL cleanup |
| 12: Integration | TUI + Telegram; delegation from Telegram; guest handling; scheduled + interactive |

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
| Gateway complexity increase | High | High | Strong session/connection separation; comprehensive integration tests; incremental steps with separate verification |
| Protocol breakage for v1 TUI | Medium | High | Negotiated version tracking; compatibility tests; v1 behavior is the default path |
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
5. **Dynamic onboarding** — Invite codes, OAuth flows. Pre-configured binding is sufficient for v1.
6. **Session migration across channels** — Starting a TUI session and continuing in Telegram. Sessions are independent per channel binding.
7. **Hot channel reload** — Adding/removing channels requires gateway restart.
8. **Observability/OpenTelemetry** — Phase 16 in guidebook; separate from this plan.

### File Change Summary

| File | Change Type | Step |
|------|------------|------|
| `crates/types/src/channel.rs` | Modified — protocol v2, new frames, sender identity | 2, 5, 6 |
| `crates/types/src/config.rs` | Modified — GatewayConfig, ChannelsConfig, AgentDefinition, SenderBinding | 6, 8, 9, 11 |
| `crates/types/src/session.rs` | **New** — SessionStore trait, SessionRecord | 4 |
| `crates/types/src/delegation.rs` | **New** — DelegationExecutor trait, request/result types | 10 |
| `crates/types/src/error.rs` | Modified — add delegation error variants | 10 |
| `crates/types/src/model.rs` | Modified — SubagentExecution progress kind | 10 |
| `crates/types/src/lib.rs` | Modified — re-export new modules | 4, 6, 10 |
| `crates/runtime/src/lib.rs` | Modified — remove shared tool context, add context parameter | 1 |
| `crates/runtime/src/tool_execution.rs` | Modified — accept tool context parameter | 1 |
| `crates/runtime/src/delegation.rs` | **New** — RuntimeDelegationExecutor | 10 |
| `crates/gateway/src/lib.rs` | **Major evolution** — multi-session, auth, channel routing | 3, 4, 5, 6, 11 |
| `crates/gateway/src/session.rs` | **New** — UserState, SessionState | 3 |
| `crates/gateway/src/sender_auth.rs` | **New** — SenderAuthPolicy | 6 |
| `crates/gateway/src/turn_runner.rs` | Modified — pass tool context directly | 1 |
| `crates/channels/Cargo.toml` | Modified — add telegram feature + deps | 8 |
| `crates/channels/src/lib.rs` | Modified — re-export telegram module | 8 |
| `crates/channels/src/telegram.rs` | **New** — TelegramAdapter | 8 |
| `crates/memory/src/session_store.rs` | **New** — LibsqlSessionStore | 4, 6, 7 |
| `crates/memory/src/schema.rs` | Modified — add migrations 0020-0022, required tables/indexes | 4, 6, 7 |
| `crates/memory/migrations/0020_*.sql` | **New** — gateway_sessions | 4 |
| `crates/memory/migrations/0021_*.sql` | **New** — sender_audit_log | 6 |
| `crates/memory/migrations/0022_*.sql` | **New** — channel_session_mappings | 7 |
| `crates/tools/src/delegation_tools.rs` | **New** — delegate_to_agent tool | 10 |
| `crates/tools/src/lib.rs` | Modified — register_delegation_tools() | 10 |
| `crates/runner/src/bootstrap.rs` | Modified — channel/auth/delegation wiring | 4, 6, 8, 9, 10 |
| `crates/runner/src/bin/oxydra-vm.rs` | Modified — channel adapter startup, session store | 4, 8 |
| `crates/runner/Cargo.toml` | Modified — add channels dep with telegram feature | 8 |
| `crates/tui/src/app.rs` | Modified — /new, /sessions, /switch handling | 5 |
| `crates/tui/src/bin/oxydra-tui.rs` | Modified — --session flag, v2 Hello | 5 |
| `crates/tui/src/ui_model.rs` | Modified — session info display | 5 |
| `crates/tui/src/widgets.rs` | Modified — session info in status bar | 5 |
| `crates/tui/src/channel_adapter.rs` | Modified — new frame handling | 5 |

### New Crate Dependencies

| Crate | New Dependency | Version | Reason |
|-------|---------------|---------|--------|
| `channels` | `teloxide` | `0.13` | Telegram bot framework (feature-gated) |
| `channels` | `tokio` | `1` (full) | Async runtime for adapter (feature-gated) |
| `types` or `tui` | `uuid` | `1` (features: `v7`) | Time-ordered session IDs (add `v7` feature to existing `uuid` dep) |
