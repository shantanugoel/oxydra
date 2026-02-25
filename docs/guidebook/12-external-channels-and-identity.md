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
| Channel session mapping (DB-backed) | Planned (Step 7) | `memory/`, `channels/` |
| Telegram adapter | Planned (Step 8) | `channels/src/telegram.rs` |
| Edit-message streaming | Planned (Step 8) | `channels/src/telegram.rs` |
| Discord/Slack/WhatsApp adapters | Deferred | — |

## Design Boundaries

- Channel adapters never access the runtime directly — all routing flows through the gateway's internal API
- Platform-specific SDK dependencies are fully contained within their feature-flagged adapter code — no platform types leak into `types` or `runtime`
- Sender authentication is non-negotiable: there is no "open mode" that skips allowlist validation for external channels
- The TUI remains a WebSocket client adapter, not a privileged path — it follows the same gateway protocol as always
- In-process adapters use the gateway's internal API; the existing `Channel` trait is for WebSocket-based client adapters only
