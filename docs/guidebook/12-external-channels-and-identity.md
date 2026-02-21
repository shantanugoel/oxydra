# Chapter 12: External Channels and Identity

## Overview

Oxydra's channel architecture is designed so that the TUI is not a special case — it is one adapter among many, all implementing the same `Channel` trait. This chapter describes how external channel adapters (Telegram, Slack, Discord, WhatsApp) will be implemented, how sender authentication works, and how channel sessions map to stable user identities.

The foundation for external channels is already built: the `Channel` trait is defined in `types`, the `ChannelRegistry` provides thread-safe adapter storage in `channels`, the gateway routes traffic from any `Channel` implementation, and the TUI demonstrates the full adapter contract end-to-end.

## External Channel Adapters

### Architecture

Each external channel adapter lives behind a feature flag in the `channels` crate:

```toml
[features]
telegram = ["teloxide"]
discord = ["serenity"]
slack = ["slack-morphism"]
whatsapp = ["reqwest"]  # WhatsApp Cloud API
```

This keeps compile times lean — only the adapters you enable are compiled. Each adapter implements the `Channel` trait:

```rust
#[async_trait]
pub trait Channel: Send + Sync {
    async fn send(&self, event: ChannelOutboundEvent) -> Result<(), ChannelError>;
    async fn listen(&self, buffer_size: usize) -> Result<ChannelListenStream, ChannelError>;
    async fn health_check(&self) -> Result<ChannelHealthStatus, ChannelError>;
}
```

### Adapter Responsibilities

Each adapter is responsible for:

1. **Wire format translation** — converting platform-specific payloads (Telegram `Update`, Discord `MessageCreate`, Slack `event_callback`) into the unified `ChannelInboundEvent` and converting `ChannelOutboundEvent` back to platform-native responses
2. **Connection management** — maintaining webhooks, long-polling connections, or WebSocket connections as required by the platform
3. **Rate limit compliance** — respecting platform-specific rate limits and retry semantics
4. **Media handling** — extracting text content from rich messages, handling attachments as file references

### Registration

Channel adapters are registered in the gateway through configuration:

1. Operator adds channel configuration to the gateway config (channel type, credentials, webhook URL)
2. On gateway start (or explicit reload), the gateway constructs the adapter and registers it in the `ChannelRegistry`
3. The gateway begins listening on all registered channels

There is no implicit hot binary registration path — channel activation requires explicit gateway reload or restart.

## Sender Authentication

### Default-Deny Ingress

Every inbound message must pass sender authentication before it reaches the agent runtime. The policy is default-deny:

```rust
pub struct ChannelSenderAuthPolicy {
    /// Per-user, per-channel allowed sender identifiers
    /// e.g., { "telegram": ["12345678"], "slack": ["U01ABC"] }
    pub allowed_senders: HashMap<String, Vec<String>>,
}
```

### Validation Flow

```
Platform message arrives
        │
        ▼
Extract platform sender ID
(Telegram: chat_id, Slack: user_id, Discord: author.id)
        │
        ▼
Look up user's channel allowlist
        │
        ├── Sender ID found → route to gateway
        │
        └── Sender ID NOT found → reject + audit log
```

When a message is rejected:
- The rejection reason is written to an audit log with the platform sender ID, channel type, and timestamp
- No agent turn is started
- An optional rejection response can be configured per channel (e.g., "Unauthorized" or silent drop)

### Configuration

Per-user channel config declares allowed senders:

```toml
[channels.telegram]
allowed_sender_ids = ["12345678", "87654321"]

[channels.slack]
allowed_sender_ids = ["U01ABCDEF"]

[channels.discord]
allowed_sender_ids = ["123456789012345678"]
```

## Session Identity Mapping

### The Problem

A single user may connect through multiple channels (TUI, Telegram, Slack) and each channel has its own session semantics (Telegram chat IDs, Slack thread IDs, Discord channel IDs). Without explicit mapping, each channel would create isolated sessions with fragmented conversation history.

### Canonical Session Identity

The solution is a deterministic identity mapping:

```
(user_id, channel_id, channel_session_id) → runtime session_id
```

- `user_id` — the Oxydra user identity, established during runner provisioning
- `channel_id` — the channel adapter identifier (e.g., "telegram", "slack")
- `channel_session_id` — the platform-specific session context (e.g., Telegram chat ID, Slack thread timestamp)

This triple maps deterministically to a stable `session_id` that the runtime uses for conversation persistence. The mapping ensures:

- Messages from the same user through the same channel thread always land in the same conversation
- Different channels for the same user share the same workspace and memory namespace (keyed by `user_id`)
- Only transient channel metadata (message formatting, reaction state) is isolated per connection

### Cross-Channel Continuity

When a user connects through a channel they haven't used before:

1. The gateway validates the sender against the user's allowlist
2. A new channel-session binding is created
3. The user's existing workspace, memory, and long-term context are available
4. A new conversation session starts (channel-specific threads are not merged across platforms)

This means a user can start a task in the TUI and later check on it from Telegram — the workspace state is shared, but the conversation threads are independent per channel.

## Gateway Integration

### Routing

The gateway's router evaluates inbound events from all registered channels identically:

1. Extract sender identity from platform-specific payload
2. Validate against user's channel allowlist
3. Map to canonical session identity
4. Route to the user's `AgentRuntime` via the same path the TUI uses

### Protocol Extension

External channels introduce additional frame types in the gateway protocol:

- `ChannelMetadata` — platform-specific context (message ID, thread ID, reply-to reference) attached to inbound events
- `ChannelDeliveryReceipt` — confirmation that an outbound message was delivered to the platform API

These are carried as optional metadata alongside the existing `GatewayClientFrame` / `GatewayServerFrame` types.

## Implementation Approach

External channels are built incrementally:

1. **First adapter** — implement one external channel (likely Telegram, as it has the simplest webhook model) end-to-end through gateway routing
2. **Sender auth** — implement `ChannelSenderAuthPolicy` with default-deny validation and audit logging
3. **Identity mapping** — implement the canonical `(user_id, channel_id, channel_session_id) → session_id` mapping
4. **Additional adapters** — add Discord, Slack, WhatsApp adapters behind feature flags following the same contract
5. **Gateway reload** — add explicit channel registration reload without full process restart

## Design Boundaries

- Channel adapters never access the runtime directly — all routing flows through the gateway
- Platform-specific SDK dependencies are fully contained within their feature-flagged adapter code — no platform types leak into `types` or `runtime`
- Sender authentication is non-negotiable: there is no "open mode" that skips allowlist validation for external channels
- The TUI remains a channel adapter, not a privileged path — it follows the same routing as external channels
