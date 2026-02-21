# Chapter 9: Gateway and Channel Architecture

## Overview

The gateway is Oxydra's central control plane — a WebSocket server that multiplexes all ingress and egress traffic between channel adapters and the agent runtime. Every interaction with Oxydra flows through the gateway, whether from the TUI, future external channels, or API clients.

## Channel Trait

Defined in `types/src/channel.rs`:

```rust
#[async_trait]
pub trait Channel: Send + Sync {
    async fn send(&self, event: ChannelOutboundEvent) -> Result<(), ChannelError>;
    async fn listen(&self, buffer_size: usize) -> Result<ChannelListenStream, ChannelError>;
    async fn health_check(&self) -> Result<ChannelHealthStatus, ChannelError>;
}
```

The `Channel` trait is the same abstraction used by both the TUI and future external channel adapters (Telegram, Slack, etc.). This ensures all ingress follows the same routing semantics.

## Gateway Protocol

Communication between channels and the gateway uses typed JSON frames over WebSocket:

### Client → Server Frames

```rust
pub enum GatewayClientFrame {
    Hello { user_id: String },
    SendTurn { content: String },
    CancelActiveTurn,
    HealthCheck,
}
```

### Server → Client Frames

```rust
pub enum GatewayServerFrame {
    HelloAck { runtime_session_id: String },
    TurnStarted { turn_id: String },
    AssistantDelta { text: String },
    TurnCompleted { final_message: String, usage: Option<UsageUpdate> },
    TurnCancelled,
    Error { message: String },
    HealthStatus { ... },
}
```

## Gateway Server

**File:** `gateway/src/lib.rs`

The `GatewayServer` is an Axum-based WebSocket server that handles session management and turn routing.

### Architecture

```
Channel Adapter                Gateway                    Runtime
     │                           │                           │
     ├── Hello ─────────────────►│                           │
     │◄── HelloAck ──────────── │                           │
     │                           │                           │
     ├── SendTurn ──────────────►│                           │
     │                           ├── create context ────────►│
     │◄── TurnStarted ────────  │                           │
     │                           │◄── StreamItem::Text ──── │
     │◄── AssistantDelta ──────  │                           │
     │◄── AssistantDelta ──────  │◄── StreamItem::Text ──── │
     │                           │                           │
     │                           │◄── Response (no tools) ── │
     │◄── TurnCompleted ──────  │                           │
     │                           │                           │
```

### Session Management

Sessions are managed via `UserSessionState`, identified by `user_id`:

- Each session maintains its own `runtime_session_id`
- A broadcast channel delivers server frames to all connected clients for that session
- An `active_turn: Mutex<Option<ActiveTurnState>>` tracks the current turn

### Turn Execution

When a `SendTurn` frame arrives:

1. The gateway locates the user's session
2. If an active turn exists, the request is rejected with an error
3. A `RuntimeGatewayTurnRunner` is created to bridge gateway and runtime
4. The runner constructs a `Context` and calls `AgentRuntime::run_session`
5. Stream items from the runtime are forwarded as `AssistantDelta` frames
6. On completion, a `TurnCompleted` frame is sent with the final message and usage data

### Reconnection Support

The gateway maintains a `latest_terminal_frame` buffer per session. When a TUI client reconnects to an existing session, it can immediately see the last known state or pick up an active streaming response.

### Cancellation

When a `CancelActiveTurn` frame arrives:
1. The gateway fires the `CancellationToken` for the active turn
2. The runtime's `tokio::select!` catches the cancellation
3. The turn is aborted cleanly
4. A `TurnCancelled` frame is sent to the client

This ensures Ctrl+C in the TUI cancels only the active turn without killing the guest process.

## TUI Channel Adapter

**File:** `tui/src/channel_adapter.rs`

The `TuiChannelAdapter` implements the `Channel` trait as a WebSocket client to the gateway.

### Connection

Uses `tokio-tungstenite` to establish a WebSocket connection to the gateway's `/ws` endpoint. The connection flow:

1. Connect to WebSocket URL (from `gateway-endpoint` marker file)
2. Send `Hello { user_id }` frame
3. Receive `HelloAck { runtime_session_id }`
4. Begin listening for server frames

### State Management

`TuiUiState` tracks the current UI state:

```rust
pub struct TuiUiState {
    pub connected: bool,
    pub active_turn_id: Option<String>,
    pub prompt_buffer: String,
    pub rendered_output: String,      // accumulated stream text
    pub last_error: Option<String>,
}
```

### Event Handling

The adapter translates between gateway frames and channel events:

- `SendTurn` → wraps user input as `GatewayClientFrame::SendTurn`
- `AssistantDelta` → appends text to `rendered_output`
- `TurnCompleted` → clears active turn state
- `CancelActiveTurn` → sends cancellation frame to gateway

### Ctrl+C Handling

- If a turn is active: sends `CancelActiveTurn` to gateway
- If idle: returns `Exit` outcome to the caller, allowing clean shutdown

### Visual Rendering

**Current state:** The `ratatui` + `crossterm` terminal rendering loop is not yet implemented. The TUI crate contains the channel adapter, state management, and gateway connection logic, but not the visual terminal interface. The rendering infrastructure is planned as a completion item.

## Channel Registry

**File:** `channels/src/lib.rs`

The `ChannelRegistry` provides thread-safe storage for channel instances:

```rust
pub struct ChannelRegistry {
    channels: RwLock<BTreeMap<String, SharedChannel>>,
}
```

Channels are registered by ID and can be looked up for routing. The `collect_channel_health` utility snapshots the health of all registered adapters.

## End-to-End Flow

Here is the complete path of a user message through the system:

```
User types message in TUI
  │
  ▼
TuiChannelAdapter.send(SendTurn { content })
  │
  ▼ WebSocket
GatewayServer receives GatewayClientFrame::SendTurn
  │
  ▼
RuntimeGatewayTurnRunner creates Context
  │
  ▼
AgentRuntime.run_session(context, cancellation_token)
  │
  ├──► Provider.stream(context) → SSE tokens
  │      │
  │      ▼
  │    StreamItem::Text → AssistantDelta → WebSocket → TUI
  │
  ├──► Tool calls detected → execute → results appended
  │      │
  │      └──► Loop back to Provider.stream()
  │
  ▼
Final response (no tool calls)
  │
  ▼
TurnCompleted { final_message, usage } → WebSocket → TUI
```

## Current Limitations

- **Single-user isolation:** Sessions are isolated per user but true lane-based queueing (where background tasks don't block real-time chat) is implemented as mutex-based rejection — a new turn is rejected if one is already active, rather than queued
- **No external channels yet:** Only the TUI adapter is implemented; external channel adapters (Telegram, Discord, Slack) are planned for Phase 14
- **No multi-agent routing:** The gateway currently routes to a single runtime instance per user; multi-agent delegation with subagent spawning is planned for Phase 15
