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
    TurnProgress { progress: RuntimeProgressEvent },  // ← runtime activity notification
    Error { message: String },
    HealthStatus { ... },
}
```

`TurnProgress` carries a `RuntimeProgressEvent` (provider call starting, tools executing, etc.) emitted by the runtime during multi-step execution. Channels decide how to surface it; the TUI shows the `progress.message` in the input bar title.

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
     │                           │◄── StreamItem::Progress ─ │  ← ProviderCall
     │◄── TurnProgress ──────  │                           │
     │                           │◄── StreamItem::Text ──── │
     │◄── AssistantDelta ──────  │                           │
     │◄── AssistantDelta ──────  │◄── StreamItem::Text ──── │
     │                           │◄── StreamItem::Progress ─ │  ← ToolExecution
     │◄── TurnProgress ──────  │                           │
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
5. Stream items from the runtime are forwarded according to type:
   - `StreamItem::Text(delta)` → published as an `AssistantDelta` frame
   - `StreamItem::Progress(event)` → published as a `TurnProgress` frame (channels display it however fits their UX; the TUI shows it in the input bar title)
   - All other `StreamItem` variants (tool call assembly, reasoning traces, usage updates) are not forwarded to channels
6. On completion, a `TurnCompleted` frame is sent with the final message and usage data

The internal `delta_sender` channel between `RuntimeGatewayTurnRunner` and the gateway's spawn loop carries `StreamItem` values (not raw strings), allowing the gateway to distinguish text deltas from progress events.

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
    pub activity_status: Option<String>,  // most recent progress message from TurnProgress
}
```

`activity_status` is populated from `GatewayServerFrame::TurnProgress` frames and cleared when the turn completes, is cancelled, or errors. The TUI shows it in the input bar title (replacing "Waiting...") while a turn is active.

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

**File:** `tui/src/app.rs`, `tui/src/widgets.rs`, `tui/src/ui_model.rs`, `tui/src/event_loop.rs`

The TUI rendering loop is built on `ratatui` (terminal UI framework) and `crossterm` (cross-platform terminal backend). It provides a full-screen interactive chat interface with three visual regions stacked vertically:

- **Message pane** (fills remaining space): scrollable chat history with per-role styling — user messages in cyan with `you:` prefix, assistant messages in default color, errors in red, system notices in dim yellow.
- **Input bar** (3 rows minimum, grows dynamically): bordered multi-line text input. Height expands to accommodate newlines in the buffer (up to 10 rows), so multi-line prompts composed with Alt+Enter display correctly. The title shows `Prompt` when idle, the most recent `RuntimeProgressEvent.message` (e.g. `"[2/8] Executing tools: file_read"`) during an active turn if a progress event has been received, or `Waiting...` when a turn is active but no progress event has arrived yet. The border grays out when input is disabled. Cursor position accounts for logical newlines and visual wrapping via `unicode-width`.
- **Status bar** (1 row): connection indicator (green/red/yellow), session ID, turn state (`idle`/`streaming`), and key hints (`[Ctrl+C to cancel]` during a turn, `[Ctrl+C to exit]` when idle).

#### State Ownership

Two separate state structures drive the rendering:

- **`TuiChannelAdapter`** (in `channel_adapter.rs`) owns `TuiUiState` — the authoritative protocol/gateway state (connected flag, runtime session ID, active turn ID, rendered output, last error). The main loop reads snapshots via `adapter.state_snapshot()` — never holding the mutex across `.await` boundaries.
- **`TuiViewModel`** (in `ui_model.rs`) is rendering-only state: message history (`Vec<ChatMessage>`, capped at 1000 entries), scroll offset, auto-scroll flag, input buffer with cursor position, spinner tick counter, and `ConnectionState` enum (`Connected`/`Disconnected`/`Reconnecting`).

The main loop bridges them: inbound gateway frames update both the adapter (protocol state) and the view model (message history), then a single `terminal.draw()` call renders the combined state.

#### Main Loop Architecture

`TuiApp::run()` in `app.rs` orchestrates the full lifecycle:

1. **Terminal setup**: `TerminalGuard` RAII struct enables raw mode, enters the alternate screen, and hides the cursor. Mouse capture is intentionally **not** enabled so users retain native terminal mouse selection and copy/paste. Its `Drop` restores terminal state. A panic hook provides the same restoration on unwind.
2. **WebSocket transport**: after `connect_async()`, the stream is split into independent read/write halves. A reader task decodes `GatewayServerFrame`s into an `mpsc` channel (`gateway_rx`). A writer task receives `GatewayClientFrame`s from another channel (`ws_tx`) and sends them on the wire. The main loop never touches the WebSocket directly.
3. **Hello handshake**: a `Hello` frame is sent via `ws_tx`; the loop waits for `HelloAck` on `gateway_rx` with a 10-second timeout.
4. **Event reader**: `EventReader` spawns a dedicated `std::thread` that calls `crossterm::event::read()` in a blocking loop, mapping `KeyEvent`s to `AppAction` variants (character input including `'\n'` via Alt+Enter, backspace, cursor movement, submit, scroll, cancel, quit, resize) and sending them into an `mpsc` channel.
5. **`tokio::select!` loop** over four channels:
   - `gateway_rx` (inbound server frames) → adapter update + view model update + auto-scroll
   - `adapter_rx` (outbound client frames from the adapter's broadcast channel) → forward to `ws_tx`
   - `action_rx` (user input) → handle text editing (including multi-line newlines from Alt+Enter), scrolling, submit (via `adapter.submit_prompt()`), cancel (via `adapter.handle_ctrl_c()`), quit
   - `tick` (100ms interval) → increment spinner, send `HealthCheck` via `ws_tx` every ~5 seconds
   - After any event: single `terminal.draw()` call renders the full UI from the view model + adapter snapshot.

#### No Double-Send

User submits prompt → `adapter.submit_prompt()` enqueues a `GatewayClientFrame::SendTurn` into the adapter's broadcast channel → the main loop's `adapter_rx` arm receives it → forwards to `ws_tx` → writer task sends it on the wire. The main loop never sends to the WebSocket directly except through `ws_tx`.

#### Reconnection

When the reader task's channel closes (WebSocket disconnect):

1. `ConnectionState` transitions to `Disconnected`, then `Reconnecting` with exponential backoff (250ms → 5s cap, with jitter).
2. Old reader/writer task handles are aborted.
3. On successful reconnect: new socket is split, new tasks spawned, `Hello` is sent with the prior `runtime_session_id` for session resume.
4. On `HelloAck`: `ConnectionState` returns to `Connected`, input re-enabled.
5. Queued outbound frames are dropped during reconnection — the user must re-submit.

#### Standalone Binary

`oxydra-tui` (`tui/src/bin/oxydra-tui.rs`) is a thin CLI entry point that parses `--gateway-endpoint` and `--user` arguments, generates a UUID connection ID, and runs `TuiApp` inside a multi-threaded tokio runtime. Runner-based endpoint discovery (`runner --tui`) execs this binary.

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
  ├──► StreamItem::Progress(ProviderCall) emitted
  │      │
  │      ▼ TurnProgress frame → WebSocket → TUI input bar title updates
  │
  ├──► Provider.stream(context) → SSE tokens
  │      │
  │      ▼
  │    StreamItem::Text → AssistantDelta → WebSocket → TUI
  │
  ├──► Tool calls detected
  │      │
  │      ├──► StreamItem::Progress(ToolExecution) emitted
  │      │      └──► TurnProgress frame → WebSocket → TUI input bar title updates
  │      │
  │      └──► execute → results appended → loop back to Provider.stream()
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
