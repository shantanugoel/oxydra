# Chapter 12: External Channels and Identity

## Overview

Oxydra supports external channel adapters (Telegram, and future Discord/Slack/WhatsApp) that run as in-process components inside the VM alongside the gateway. Each adapter calls the gateway's internal API directly — no WebSocket overhead.

The foundation for external channels is built in layers:
- **Config types** (`ChannelsConfig`, `TelegramChannelConfig`, `SenderBinding`) in the `types` crate define per-user channel configuration
- **Sender authentication** (`SenderAuthPolicy`) in the `channels` crate implements default-deny authorization
- **Audit logging** (`AuditLogger`) in the `channels` crate records rejected sender events
- **Bootstrap propagation** — the runner includes channel config in the `RunnerBootstrapEnvelope` and forwards bot token env vars to the VM
- The `Channel` trait (defined in `types`) is for WebSocket-based client adapters (TUI); in-process adapters use the gateway's internal API directly and do not implement `Channel`

## Architecture

### Two Adapter Patterns

Oxydra has two distinct adapter patterns for connecting to the gateway:

1. **WebSocket client adapters** (TUI): Implement the `Channel` trait, connect over WebSocket, and communicate via the gateway protocol frames. The TUI is the primary example.

2. **In-process adapters** (Telegram, future Discord/Slack): Run inside the VM alongside the gateway. They call the gateway's internal API methods directly (`create_or_get_session()`, `submit_turn()`, `subscribe_events()`, etc.). This avoids WebSocket overhead and provides identical behavior to WebSocket clients since both call the same underlying methods.

### Channel Adapters Run Inside the VM

Channel adapters run inside the VM (same process as the gateway), not in the runner:
- Gateway is in the same process — direct function calls, no WebSocket overhead
- Adapter lifecycle matches VM lifecycle automatically — no separate management
- Each VM handles only its own user's bot — no multi-user routing complexity
- Follows the same pattern as provider, memory, scheduler — everything runs in the VM
- Bot tokens are same trust level as LLM API keys, which already enter the VM

### What Remains Outside the VM (Host-Side, in Runner)

- `RunnerUserConfig` with `channels` section — config source of truth
- `RunnerBootstrapEnvelope` carries channels config into the VM
- Bot token env var forwarding (runner reads `bot_token_env`, forwards the value)
- Everything else about channels (auth, adapters, session mapping, audit) runs inside the VM

## Per-User Channel Configuration

Channel configuration lives in `RunnerUserConfig` (per-user, host-side config at `users/<user>/config.toml`). It is delivered to the VM via the `RunnerBootstrapEnvelope`.

### Configuration Types

```rust
// types/src/runner.rs

/// Per-user channel configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ChannelsConfig {
    pub telegram: Option<TelegramChannelConfig>,
    // Future: discord, whatsapp, etc.
}

/// Telegram channel adapter configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TelegramChannelConfig {
    pub enabled: bool,                        // default: false
    pub bot_token_env: Option<String>,        // env var name holding the bot token
    pub polling_timeout_secs: u64,            // default: 30
    pub senders: Vec<SenderBinding>,          // authorized sender identities
    pub max_message_length: usize,            // default: 4096
}

/// A sender identity binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SenderBinding {
    pub platform_ids: Vec<String>,            // platform-specific sender IDs
    pub display_name: Option<String>,         // human-readable name for audit
}
```

### Example Configuration

```toml
# users/alice/config.toml (RunnerUserConfig — per-user, host-side)

[channels.telegram]
enabled = true
bot_token_env = "ALICE_TELEGRAM_BOT_TOKEN"
polling_timeout_secs = 30
max_message_length = 4096

# Authorized senders — only these platform IDs can interact
[[channels.telegram.senders]]
platform_ids = ["12345678"]         # Alice's Telegram user ID
display_name = "Alice"

[[channels.telegram.senders]]
platform_ids = ["87654321", "11223344"]  # Bob has two Telegram accounts
display_name = "Bob"
```

### Bootstrap Propagation

The `RunnerBootstrapEnvelope` includes an optional `channels: Option<ChannelsConfig>` field. The runner populates it from the user's config and also forwards bot token environment variables to the VM container alongside existing API key env vars.

```rust
// In runner's start_user_for_host():
let bootstrap = RunnerBootstrapEnvelope {
    // ... existing fields ...
    channels: if user_config.channels.is_empty() {
        None
    } else {
        Some(user_config.channels.clone())
    },
};
```

### Config Design Principles

- All new config sections use `#[serde(default)]` so existing configs work without modification
- `RunnerUserConfig.channels` defaults to empty (no channels enabled)
- `TelegramChannelConfig.enabled` defaults to `false`
- `TelegramChannelConfig.senders` defaults to empty vec (nobody can interact)
- Channels config is per-user because bot tokens and sender bindings differ per user
- Agent behavior config (`agent.toml`) remains separate — channels config doesn't belong there

## Sender Authentication

### Default-Deny Ingress

Every inbound message must pass sender authentication before it reaches the agent runtime. The policy is **default-deny**: only platform IDs explicitly listed in the configuration are allowed to interact.

### SenderAuthPolicy

Implemented in `channels/src/sender_auth.rs`:

```rust
pub struct SenderAuthPolicy {
    authorized: HashSet<String>,  // flattened set of all platform IDs
}

impl SenderAuthPolicy {
    pub fn from_bindings(bindings: &[SenderBinding]) -> Self;
    pub fn is_authorized(&self, platform_id: &str) -> bool;
    pub fn authorized_count(&self) -> usize;
    pub fn is_empty(&self) -> bool;
}
```

The policy is built from the user's configured `SenderBinding` list. All `platform_ids` from all bindings are flattened into a single `HashSet` for O(1) lookup. Empty bindings produce a policy that rejects everyone.

### Authorization Model

Binary decision: a sender is either **authorized** or **rejected**.

- **Authorized senders** (listed in `channels.*.senders`): Messages are processed as normal user turns. The agent sees them as `MessageRole::User`, identical to TUI input.
- **Unknown senders** (not in the list): Rejected silently. Audit log entry created. No response sent (prevents enumeration).

All authorized senders are treated identically as the owning user — there is no role hierarchy or permission differentiation. If alice authorizes Bob's Telegram ID, Bob's messages are processed exactly as if alice typed them in the TUI.

### Validation Flow

```
Platform message arrives
        │
        ▼
Extract platform sender ID
(Telegram: message.from.id)
        │
        ▼
sender_auth.is_authorized(sender_id)
        │
        ├── true → route to gateway (submit_turn)
        │
        └── false → audit_logger.log_rejected_sender() + silent drop
```

## Audit Logging

Implemented in `channels/src/audit.rs`:

```rust
pub struct AuditEntry {
    pub timestamp: String,          // ISO 8601 UTC
    pub channel: String,            // e.g., "telegram"
    pub sender_id: String,          // rejected platform ID
    pub reason: String,             // brief rejection reason
    pub context: Option<String>,    // optional context (chat_id, etc.)
}

pub struct AuditLogger {
    log_path: PathBuf,
}
```

### Behavior

- Writes JSON-lines to `<workspace>/.oxydra/sender_audit.log`
- Each line is a self-contained JSON object
- Parent directories created automatically on first write
- Append-only (no rotation — simple for v1)
- Failures to write are logged via `tracing::warn` but **never propagated** — audit logging must not break message processing

### Example Audit Line

```json
{"timestamp":"2026-02-25T12:00:00Z","channel":"telegram","sender_id":"99999999","reason":"sender not in authorized list","context":"chat_id=12345"}
```

## Session Identity Mapping

### The Problem

A single user may connect through multiple channels (TUI, Telegram) and each channel has its own session semantics. Without explicit mapping, each channel creates isolated sessions with fragmented context.

### Canonical Session Identity

Each unique `(channel_id, channel_context_id)` maps to one gateway session:

- `channel_id` — the channel adapter identifier (e.g., "telegram")
- `channel_context_id` — the platform-specific session context, derived per platform (D14 in the plan):
  - **Telegram (forum groups):** `"{chat_id}:{message_thread_id}"` — each topic is a separate session
  - **Telegram (regular chats/DMs):** `"{chat_id}"` — single session per chat
  - **Discord:** `"{guild_id}:{channel_id}:{thread_id?}"` — threads are separate sessions

This means each topic/thread gets its own session with its own `active_turn` — enabling true concurrency within a single chat. Within a single topic, "turn already active" still applies naturally.

### Database-Backed Mapping

Channel session mappings are persisted in the `channel_session_mappings` table (migration 0021):

```sql
CREATE TABLE channel_session_mappings (
    channel_id          TEXT NOT NULL,
    channel_context_id  TEXT NOT NULL,
    session_id          TEXT NOT NULL REFERENCES gateway_sessions(session_id) ON DELETE CASCADE,
    created_at          TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at          TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (channel_id, channel_context_id)
);
```

The `SessionStore` trait (in `types`) provides `get_channel_session()` and `set_channel_session()` methods. The `ChannelSessionMap` wrapper (in `channels/src/session_map.rs`) provides a thin adapter-friendly API:

```rust
pub struct ChannelSessionMap {
    store: Arc<dyn SessionStore>,
}

impl ChannelSessionMap {
    pub async fn get_session_id(&self, channel_id: &str, channel_context_id: &str) -> Result<Option<String>, MemoryError>;
    pub async fn set_session_id(&self, channel_id: &str, channel_context_id: &str, session_id: &str) -> Result<(), MemoryError>;
}
```

### Cross-Channel Continuity

Different channels for the same user share the same workspace and memory namespace (keyed by `user_id`). Conversation threads are independent per channel — a user can start a task in the TUI and check on workspace state from Telegram, but the conversation histories are separate.

## Why Not Dynamic Onboarding?

For the initial implementation, we deliberately avoid invite-code or OAuth flows because:
1. They add attack surface (invite code leakage, phishing)
2. They require state management for pending invites
3. They're unnecessary for the primary use case (personal agent)
4. Pre-configured binding is zero-trust: only the operator with file system access can authorize senders

Dynamic onboarding can be added later as an enhancement on top of the static binding model.

## Implementation Status

| Component | Status | Location |
|-----------|--------|----------|
| `ChannelsConfig`, `TelegramChannelConfig`, `SenderBinding` types | ✅ Implemented | `types/src/runner.rs` |
| Bootstrap envelope propagation | ✅ Implemented | `runner/src/lib.rs` |
| Bot token env var forwarding | ✅ Implemented | `runner/src/lib.rs` |
| `SenderAuthPolicy` | ✅ Implemented | `channels/src/sender_auth.rs` |
| `AuditLogger` + `AuditEntry` | ✅ Implemented | `channels/src/audit.rs` |
| Channel session mapping (DB-backed) | ✅ Implemented | `types/src/session.rs`, `memory/src/session_store.rs`, `channels/src/session_map.rs` |
| `channel_session_mappings` DB migration | ✅ Implemented | `memory/migrations/0021_create_channel_session_mappings.sql` |
| `ChannelSessionMap` wrapper | ✅ Implemented | `channels/src/session_map.rs` |
| Telegram adapter (`TelegramAdapter`) | ✅ Implemented | `channels/src/telegram.rs` |
| Edit-message streaming (`ResponseStreamer`) | ✅ Implemented | `channels/src/telegram.rs` |
| Markdown → Telegram HTML conversion | ✅ Implemented | `channels/src/telegram.rs` |
| Telegram command interception (`/new`, `/sessions`, `/switch`, `/cancel`, `/status`) | ✅ Implemented | `channels/src/telegram.rs` |
| Adapter spawning in oxydra-vm | ✅ Implemented | `runner/src/bin/oxydra-vm.rs` |
| Feature-flagged `telegram` in channels + runner | ✅ Implemented | `channels/Cargo.toml`, `runner/Cargo.toml` |
| Discord/Slack/WhatsApp adapters | Deferred | — |

## Telegram Adapter

### Overview

The Telegram adapter (`channels/src/telegram.rs`, feature-gated behind `telegram`) is an in-process component that runs alongside the gateway inside the VM. It uses the `frankenstein` crate (v0.47, `client-reqwest` feature) for Telegram Bot API access.

### Architecture

```
Telegram API (long-polling)
    │
    ▼
TelegramAdapter::run() loop
    ├── bot.get_updates() → Update list
    │
    ▼ per Update
    ├── Extract sender ID (message.from.id)
    ├── SenderAuthPolicy.is_authorized() → reject + audit, or continue
    ├── derive_channel_context_id(chat_id, thread_id)
    ├── Command interception (/new, /sessions, /switch, /cancel, /status, /help)
    │    └── Call gateway internal API directly
    ├── ChannelSessionMap.get_session_id() → resolve or create session
    ├── gateway.subscribe_events() (before submit, to not miss frames)
    ├── gateway.submit_turn() → start the turn
    │
    ▼
ResponseStreamer (edit-message streaming)
    ├── send_message("⏳ Working...") → placeholder
    ├── TurnProgress → edit with status line
    ├── AssistantDelta → accumulate + throttled edit (1.5s)
    ├── Message splitting → new message at ~3896 chars
    └── TurnCompleted → final edit with Markdown→HTML, fallback to plain text
```

### Edit-Message Streaming (D15)

The adapter uses Telegram's `editMessageText` API to stream responses live:

1. **Turn starts** → Send placeholder "⏳ Working..."
2. **Progress events** → Edit message with status ("🔍 Searching the web...")
3. **Token deltas** → Accumulate text, edit message every 1.5 seconds
4. **Near char limit** → Stop editing, send new continuation message
5. **Turn completed** → Final edit with complete response (Markdown→HTML)

The 1.5-second throttle stays safely within Telegram's ~30 edits/minute rate limit.

### Markdown → Telegram HTML

The `markdown_to_telegram_html()` utility converts common Markdown to Telegram's HTML subset:

| Markdown | Telegram HTML |
|----------|--------------|
| `**bold**` | `<b>bold</b>` |
| `*italic*` | `<i>italic</i>` |
| `` `code` `` | `<code>code</code>` |
| ```` ```code``` ```` | `<pre>code</pre>` |
| `[text](url)` | `<a href="url">text</a>` |
| `~~strike~~` | `<s>strike</s>` |
| `# Header` | `<b>Header</b>` |

HTML conversion is used only in the final edit. Interim edits use plain text for speed. If HTML parsing fails (Telegram returns an error), the adapter falls back to plain text.

### Commands

| Command | Description |
|---------|-------------|
| `/new [name]` | Create a new session (optionally named) |
| `/sessions` | List active sessions |
| `/switch <id>` | Switch to a different session |
| `/cancel` | Cancel the active turn |
| `/status` | Show current session info |
| `/start`, `/help` | Show help text |

### Feature Flag

The Telegram adapter is behind the `telegram` feature flag in both the `channels` and `runner` crates. It's included in default features for both crates.

```toml
# channels/Cargo.toml
[features]
default = ["telegram"]
telegram = ["dep:frankenstein", "dep:gateway", "dep:tokio", "dep:tokio-util", "dep:uuid"]

# runner/Cargo.toml
[features]
default = ["telegram"]
telegram = ["dep:channels", "channels/telegram"]
```

## Design Boundaries

- Channel adapters never access the runtime directly — all routing flows through the gateway's internal API
- Platform-specific SDK dependencies are fully contained within their feature-flagged adapter code — no platform types leak into `types` or `runtime`
- Sender authentication is non-negotiable: there is no "open mode" that skips allowlist validation for external channels
- The TUI remains a WebSocket client adapter, not a privileged path — it follows the same gateway protocol as always
- In-process adapters use the gateway's internal API; the existing `Channel` trait is for WebSocket-based client adapters only
