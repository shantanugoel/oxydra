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

The `Channel` trait is intentionally scoped to **WebSocket-based clients** (today: TUI). In-process external adapters (Telegram, Discord, Slack, etc.) call the gateway internal API directly instead of implementing this trait.

## Gateway Protocol

Communication between channels and the gateway uses typed JSON frames over WebSocket:

### Client → Server Frames

```rust
pub enum GatewayClientFrame {
    Hello(GatewayClientHello),      // { user_id, session_id?, create_new_session }
    SendTurn(GatewaySendTurn),      // { session_id, turn_id, prompt, attachments? }
    CancelActiveTurn(GatewayCancelActiveTurn),  // { session_id, turn_id } (turn_id is advisory)
    CancelAllActiveTurns(GatewayCancelAllActiveTurns), // { request_id } user-wide cancel
    HealthCheck(GatewayHealthCheck),
    CreateSession(GatewayCreateSession),        // { display_name?, agent_name? }
    ListSessions(GatewayListSessions),          // { include_archived, include_subagent_sessions }
    SwitchSession(GatewaySwitchSession),        // { session_id }
}
```

### Server → Client Frames

```rust
pub enum GatewayServerFrame {
    HelloAck(GatewayHelloAck),              // { session: GatewaySession, active_turn? }
    TurnStarted(GatewayTurnStarted),
    AssistantDelta(GatewayAssistantDelta),   // { delta: String }
    TurnCompleted(GatewayTurnCompleted),     // { response: Response }
    TurnCancelled(GatewayTurnCancelled),
    TurnProgress(GatewayTurnProgress),       // ← runtime activity notification
    ScheduledNotification(GatewayScheduledNotification),  // ← scheduled task result
    Error(GatewayErrorFrame),
    HealthStatus(GatewayHealthStatus),
    SessionCreated(GatewaySessionCreated),   // ← new session confirmation
    SessionList(GatewaySessionList),         // ← session listing response
    SessionSwitched(GatewaySessionSwitched), // ← session switch confirmation
}
```

`GatewaySession` contains `{ user_id, session_id }` and is included in most server frames for routing context.

`TurnProgress` carries a `RuntimeProgressEvent` (provider call starting, tools executing, etc.) emitted by the runtime during multi-step execution. Channels decide how to surface it; the TUI shows the `progress.message` in the input bar title.

`ScheduledNotification` carries the result of a scheduled task that has been configured to notify the user. It contains the `schedule_id`, optional `schedule_name`, and the notification `message`. The TUI displays these as system messages in the chat history.

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

The gateway uses a two-level session model: users contain sessions, and each session tracks its own state independently.

**User State** (`UserState`):
- Keyed by `user_id`
- Contains a `RwLock<HashMap<String, Arc<SessionState>>>` of all sessions for that user
- Uses a per-user `Semaphore` for bounded top-level turn concurrency (`max_concurrent_turns_per_user`, default 10)
- Uses FIFO permit acquisition (Tokio semaphore fairness) so queued top-level turns are handled fairly
- Queueing is bounded per user (paired with `max_sessions_per_user`, default 50)

**Session State** (`SessionState`):
- Keyed by `session_id` (UUID v7 for new sessions, deterministic `runtime-{user_id}` for backward compatibility)
- Each session has its own broadcast channel, active turn tracker, agent name, and optional parent session ID (for subagent sessions)
- A `latest_terminal_frame` buffer per session supports reconnection

**Session creation follows three paths:**
- `create_new_session: true` in Hello → generates a UUID v7 session ID
- `session_id: Some(id)` in Hello → joins an existing session (in-memory or resumed from the session store)
- Neither set → backward-compatible deterministic session ID `runtime-{user_id}`

**Session persistence:**
- The `SessionStore` trait (defined in the `types` crate, implemented as `LibsqlSessionStore` in the `memory` crate) persists session metadata to a `gateway_sessions` SQLite table
- Sessions are persisted on creation and touched (`last_active_at` updated) on turn completion
- Sessions can be resumed from the store when not found in memory (e.g., after gateway restart)
- The store supports listing, archiving, and parent-child relationships
- A background cleanup task archives stale in-memory sessions (`session_idle_ttl_hours`, default 48) when they have no subscribers, then evicts their runtime context to cap memory growth

### Internal API for Channel Adapters

The `GatewayServer` exposes a programmatic internal API for in-process channel adapters (Telegram, Discord, etc.) that runs alongside the WebSocket handler:

```rust
impl GatewayServer {
    pub async fn create_or_get_session(...) -> Result<Arc<SessionState>, String>;
    pub async fn submit_turn(...) -> Option<GatewayServerFrame>;
    pub async fn cancel_session_turn(...) -> Option<GatewayServerFrame>;
    pub fn subscribe_events(...) -> broadcast::Receiver<GatewayServerFrame>;
    pub async fn list_user_sessions(...) -> Result<Vec<SessionRecord>, String>;
}
```

Both the WebSocket handler and in-process adapters call these same methods, ensuring identical behavior regardless of transport. The existing `Channel` trait is for WebSocket-based client adapters (TUI); in-process adapters use the internal API directly and do not implement `Channel`.

### Turn Execution

When a `SendTurn` frame arrives:

1. The gateway validates any inline attachments (count limit, per-file size limit, total payload limit, MIME type format)
2. The gateway locates the user's session
3. If an active turn exists, the request is rejected with an error
4. A `RuntimeGatewayTurnRunner` is created to bridge gateway and runtime
5. The runner constructs a `Context` and a per-turn `ToolExecutionContext` (with user_id and session_id), then calls `AgentRuntime::run_session_for_session_with_stream_events`, passing the tool context as a parameter (not stored as shared state)
6. Stream items from the runtime are forwarded according to type:
   - `StreamItem::Text(delta)` → published as an `AssistantDelta` frame
   - `StreamItem::Progress(event)` → published as a `TurnProgress` frame (channels display it however fits their UX; the TUI shows it in the input bar title)
   - `StreamItem::Media(attachment)` → published as a `MediaAttachment` frame for channel delivery
   - All other `StreamItem` variants (tool call assembly, reasoning traces, usage updates) are not forwarded to channels
7. On completion, a `TurnCompleted` frame is sent with the final message and usage data

The internal `delta_sender` channel between `RuntimeGatewayTurnRunner` and the gateway's spawn loop carries `StreamItem` values (not raw strings), allowing the gateway to distinguish text deltas from progress and media events.

Concurrency permits apply to top-level user turns only. Subagent turns execute under their parent turn's permit because delegation is synchronous from the parent tool call.

### Reconnection Support

The gateway maintains a `latest_terminal_frame` buffer per session. When a TUI client reconnects to an existing session, it can immediately see the last known state or pick up an active streaming response.

### Cancellation

When a `CancelActiveTurn` frame arrives:
1. The gateway looks up the session's active turn
2. If one exists, it fires that turn's `CancellationToken` (without requiring exact turn-id match)
3. The runtime's `tokio::select!` catches the cancellation
4. The turn is aborted cleanly and a `TurnCancelled` frame is sent

This ensures Ctrl+C in the TUI cancels only the active turn without killing the guest process.

When a `CancelAllActiveTurns` frame arrives, the gateway iterates all sessions for that user and fires cancellation tokens for every currently active turn.

### Scheduler Notification

The `GatewayServer` implements the `SchedulerNotifier` trait (defined in the `runtime` crate), enabling the scheduler executor to publish notifications to connected users without a direct dependency on the gateway:

```rust
#[async_trait]
impl SchedulerNotifier for GatewayServer {
    async fn notify_user(&self, schedule: &ScheduleDefinition, frame: GatewayServerFrame) {
        // Routes notification based on the schedule's origin channel
    }
}
```

Notification routing is origin-aware:

1. **TUI sessions** (`channel_id == "gateway"`) — delivers to the specific WebSocket session matching the `channel_context_id` stored on the schedule
2. **External channels** (e.g. Telegram) — looks up a registered `ProactiveSender` for the channel and invokes `send_notification(channel_context_id, frame)`
3. **Legacy fallback** — broadcasts to all sessions for the user when no specific origin is available

### ProactiveSender Trait

**File:** `types/src/proactive.rs`

External channels that support unsolicited outbound messages implement the `ProactiveSender` trait:

```rust
#[async_trait]
pub trait ProactiveSender: Send + Sync {
    async fn send_notification(&self, channel_context_id: &str, frame: GatewayServerFrame);
}
```

Channels register their sender with `GatewayServer::register_proactive_sender(channel_id, sender)` at startup. The Telegram channel, for example, registers a `TelegramProactiveSender` that parses the `channel_context_id` into a chat ID and sends the notification as a Telegram message.

### Per-Turn Origin Propagation

Each turn submission carries a `TurnOrigin` (defined in `gateway/src/turn_runner.rs`) that captures the ingress channel:

```rust
pub struct TurnOrigin {
    pub channel_id: Option<String>,
    pub channel_context_id: Option<String>,
    pub agent_name: Option<String>,
    pub channel_capabilities: Option<ChannelCapabilities>,
}
```

This origin is threaded through `ToolExecutionContext` so that tools like `schedule_create` can capture which channel the user was in when they created the schedule. It also carries the session `agent_name` used to select the effective provider/model route for the turn. The gateway provides `submit_turn_from_channel()` for external channels to submit turns with their channel identity.

## Session Lifecycle UX

The gateway supports interactive session management through dedicated protocol frames. Users can create, list, and switch between sessions without disconnecting.

### Protocol Frames

**Client → Server:**
- `CreateSession { request_id, display_name?, agent_name? }` — create a new session and switch to it
- `ListSessions { request_id, include_archived, include_subagent_sessions }` — list the user's sessions
- `SwitchSession { request_id, session_id }` — switch the connection to a different existing session

**Server → Client:**
- `SessionCreated { request_id, session, display_name?, agent_name }` — confirms creation and includes the new session's identity
- `SessionList { request_id, sessions: Vec<GatewaySessionSummary> }` — returns session summaries with IDs, names, timestamps, and status
- `SessionSwitched { request_id, session, active_turn? }` — confirms the switch and reports any active turn on the target session

### Gateway Handling

When `CreateSession` arrives, the gateway creates a new UUID v7 session, persists it to the session store (with optional display name), and switches the connection's active broadcast subscription to the new session. The old session remains in memory for other connections.
If `agent_name` is provided, turns in that session are executed using that agent's effective provider/model selection route (explicit agent override or inherited root/default behavior).

When `ListSessions` arrives, the gateway reads from the session store, filters out subagent sessions (those with `parent_session_id`) unless `include_subagent_sessions` is true, and returns `GatewaySessionSummary` records.

When `SwitchSession` arrives, the gateway looks up the target session by ID (in-memory first, then from the session store for resumed sessions). On success, the connection's broadcast subscription is switched and the response includes the target session's active turn status.

### TUI Integration

The TUI intercepts slash commands before sending them as turns:

- `/new [name]` → `CreateSession` with optional display name
- `/sessions` → `ListSessions` (shows formatted listing in chat)
- `/switch <session_id>` → `SwitchSession` (prefix matching supported)
- `/cancel` → `CancelActiveTurn` (session-scoped)
- `/cancelall` → `CancelAllActiveTurns` (user-scoped across sessions)

The status bar shows the current session ID (shortened to 8 chars) and idle hints for available commands: `[Ctrl+C to exit | /new /sessions /switch /cancel /cancelall]`.

The `--session <id>` CLI flag on `oxydra-tui` joins an existing session instead of creating a new one. Without the flag, a new session is created automatically on first connect.

## TUI Channel Adapter

**File:** `tui/src/channel_adapter.rs`

The `TuiChannelAdapter` implements the `Channel` trait as a WebSocket client to the gateway.

### Connection

Uses `tokio-tungstenite` to establish a WebSocket connection to the gateway's `/ws` endpoint. The connection flow:

1. Connect to WebSocket URL (from `gateway-endpoint` marker file)
2. Send `Hello { user_id, session_id?, create_new_session }` frame
3. Receive `HelloAck { session: GatewaySession }`
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
    pub last_scheduled_notification: Option<GatewayScheduledNotification>,  // latest scheduled task notification
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
3. On successful reconnect: new socket is split, new tasks spawned, `Hello` is sent with the prior `session_id` for session resume.
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

- **Per-user concurrent turn limit:** Configurable via `max_concurrent_turns_per_user` (default: 10). When the limit is reached, new top-level turns queue fairly (bounded FIFO via Tokio `Semaphore`). When the queue itself is full (`max_sessions_per_user`, default 50), new turns are rejected with an error frame. Subagent turns execute under their parent turn's permit and do not consume queue slots.
- **External channels:** The Telegram adapter is implemented (see Chapter 12) including sender auth, channel session mapping, edit-message streaming, and command interception. It runs in-process alongside the gateway, calling the internal API directly. Future channels (Discord, Slack, WhatsApp) follow the same pattern but are not yet implemented.
- **No multi-agent routing:** The gateway currently routes to a single runtime instance per user; multi-agent delegation with subagent spawning is in progress (Phase 15)
- **Scheduled notifications are origin-routed:** Notifications are delivered to the channel that created the schedule. If the user was in a TUI session and that session is no longer connected, the notification is lost (results persist in memory and can be retrieved via `schedule_runs`/`schedule_run_output` tools). External channels with registered `ProactiveSender`s (e.g. Telegram) can deliver notifications even when the user is offline.

## Inline Attachment Ingress Limits

The gateway enforces ingress limits on inline media attachments at the protocol level, before any turn processing begins:

| Limit | Value |
|-------|-------|
| Max attachments per turn | 4 |
| Max size per attachment | 10 MB |
| Max total attachment payload | 40 MB |

Additionally, each attachment's MIME type is validated for format correctness (must contain exactly one `/` separator, ASCII alphanumeric characters plus `-._+`).

These limits are enforced in `validate_inline_attachments()` and apply uniformly to all channels (WebSocket, Telegram, etc.). Violations produce an immediate `GatewayServerFrame::Error` response.

The `UserTurnInput` struct bundles the user's text prompt and optional attachments for the turn runner:

```rust
pub struct UserTurnInput {
    pub prompt: String,
    pub attachments: Vec<InlineMedia>,
}
```
