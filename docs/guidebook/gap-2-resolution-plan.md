# Gap 2 Resolution Plan: TUI Interactive Client

## What Exists Today

- **`TuiChannelAdapter`**: Full gateway state machine — handles all `GatewayServerFrame` variants, updates `TuiUiState`, supports Ctrl+C cancellation, reconnection with session reuse
- **`GatewayWebSocketClient`**: WebSocket connect/send/receive with ping/pong and frame encoding/decoding
- **`TuiUiState`**: Tracks `connected`, `runtime_session_id`, `active_turn_id`, `prompt_buffer`, `rendered_output`, `last_error`
- **Runner `--tui` mode**: Discovers gateway endpoint via marker file, probes health, prints connection metadata — but never launches an interactive client
- **`bootstrap.rs`**: Full config loading, provider construction, memory init — ready for TUI standalone use

## What's Missing

The **terminal rendering loop** (ratatui + crossterm): drawing widgets, reading keyboard input, dispatching events between UI and WebSocket.

---

## Step-by-Step Plan

### Step 1: Add Dependencies to `crates/tui/Cargo.toml`

Add `ratatui`, `crossterm`, and `unicode-width`:
- `ratatui = "0.30"` — terminal UI framework
- `crossterm = "0.29"` — cross-platform terminal backend (matching ratatui's crossterm version)
- `unicode-width = "0.2"` — for text width calculation in input field

Update tokio features to include `time`, `macros`, `signal` (needed for tick intervals, async test convenience, and signal handling).

**Version compatibility check:** After adding dependencies, run `cargo tree -i crossterm` to verify ratatui 0.30 is compatible with crossterm 0.29 and there are no duplicate crossterm versions in the dependency graph. If ratatui pins a different crossterm version, align to ratatui's expected version.

No new workspace-level dependencies needed; these are TUI-crate-only.

### Step 2: Create `crates/tui/src/ui_model.rs` — Extended UI State for Rendering

Extend the rendering state beyond `TuiUiState` (which remains the gateway-synced state). Create a view model that the rendering loop consumes.

**State ownership principle:** `TuiChannelAdapter` remains the single authoritative owner of protocol/gateway state (`TuiUiState`). The `TuiViewModel` is purely for rendering and local interaction state. It observes `GatewayServerFrame`s to build `message_history` but never duplicates the protocol state machine — it reads snapshots from the adapter via `adapter.state_snapshot()` when needed.

- **`TuiViewModel`** struct (does NOT wrap `TuiUiState`; reads it via snapshots):
  - `message_history: Vec<ChatMessage>` — accumulated conversation messages (role + content), capped at `MAX_HISTORY_MESSAGES` (e.g., 1000) to prevent unbounded memory growth
  - `scroll_offset: usize` — vertical scroll position in the message pane
  - `auto_scroll: bool` — true when user is at the bottom; new output auto-scrolls only when true
  - `input_buffer: String` — local input text
  - `input_cursor_position: usize` — cursor index within the input buffer
  - `status_line: String` — connection status / active model / turn state
  - `spinner_tick: usize` — animation frame counter for streaming indicator
  - `connection_state: ConnectionState` — see below

- **`ConnectionState`** enum for explicit reconnection tracking:
  - `Connected` — normal operation
  - `Disconnected { since: Instant }` — connection lost
  - `Reconnecting { attempt: u32, next_retry: Instant }` — actively reconnecting
  - Reflected in status bar and input bar visual state

- **`ChatMessage`** enum:
  - `User(String)` — user prompt
  - `Assistant(String)` — assistant response (built incrementally from deltas)
  - `Error(String)` — error display
  - `System(String)` — connection/status messages

- **Methods on `TuiViewModel`**:
  - `apply_server_frame(&mut self, frame: &GatewayServerFrame)` — updates `message_history` only (appends deltas to the last assistant message, finalizes on `TurnCompleted`, shows errors on `Error`, handles `GatewayTurnState::Failed` by clearing active turn and appending error message)
  - `append_user_message(&mut self, prompt: &str)` — adds user message to history, trims if over cap
  - `scroll_up() / scroll_down() / scroll_to_bottom()` — adjust scroll offset; `scroll_to_bottom` sets `auto_scroll = true`
  - `should_auto_scroll(&self) -> bool` — returns true if user is at or near bottom
  - `insert_char(c: char) / delete_char() / move_cursor_left() / move_cursor_right()` — input buffer editing
  - `take_input(&mut self) -> String` — drain input buffer on submit

### Step 3: Create `crates/tui/src/widgets.rs` — ratatui Widget Definitions

Define the visual components using ratatui's widget system:

- **`MessagePane`** widget:
  - Renders `message_history` as styled `Paragraph` blocks inside a `Block` with borders
  - User messages: left-aligned, distinct color (e.g., cyan)
  - Assistant messages: left-aligned, default color, with streaming indicator (spinner/`▌` cursor) when `active_turn_id.is_some()` and this is the latest message
  - Error messages: red
  - Respects `scroll_offset` for vertical scrolling

- **`InputBar`** widget:
  - Single-line (or multi-line with wrapping) `Paragraph` inside a `Block` with border + title showing prompt status
  - Shows cursor position via `Frame::set_cursor_position()`
  - Visual cue when disabled (during active turn — grayed out border)

- **`StatusBar`** widget:
  - Bottom row: connection status (`●` green/red), session ID, model name, turn state
  - Shows `[Ctrl+C to cancel]` during active turn, `[Ctrl+C to exit]` when idle

- **Layout function** `render_app(frame: &mut Frame, model: &TuiViewModel)`:
  - Uses `Layout::vertical` with 3 constraints: message pane (fill), input bar (3 lines), status bar (1 line)
  - Calls each widget's render into its allocated area

### Step 4: Create `crates/tui/src/event_loop.rs` — Input Event Handling

Bridge crossterm events to application actions:

- **`AppAction`** enum:
  - `Char(char)`, `Backspace`, `Delete`, `CursorLeft`, `CursorRight`, `Home`, `End`
  - `Submit` (Enter key)
  - `ScrollUp`, `ScrollDown`, `PageUp`, `PageDown`
  - `Cancel` (Ctrl+C)
  - `Quit` (Ctrl+D or Ctrl+Q)
  - `Tick` — periodic timer for spinner animation + health checks

- `Resize(u16, u16)` — terminal resize event (width, height); ratatui needs redraw on resize

- **`EventReader`** struct:
  - Spawns a **dedicated blocking thread** via `std::thread::spawn` that calls `crossterm::event::read()` in a loop (avoids `EventStream` async compatibility issues with tokio)
  - Maps `KeyEvent` → `AppAction`, maps `Event::Resize` → `AppAction::Resize`
  - Sends actions into a `tokio::sync::mpsc::Sender<AppAction>`
  - **Tick** is handled separately in the async main loop via `tokio::time::interval` (~100ms) rather than in the blocking thread
  - Returns actions via an `mpsc::Receiver<AppAction>`

### Step 5: Create `crates/tui/src/app.rs` — Main Application Loop

The core orchestration tying everything together.

#### Terminal Cleanup Guard

Before the main loop, create a `TerminalGuard` RAII struct whose `Drop` impl restores terminal state:
- `disable_raw_mode()`
- `execute!(stdout, LeaveAlternateScreen, DisableMouseCapture, cursor::Show)`

Additionally, install a panic hook (via `std::panic::set_hook`) that attempts the same restoration before the default panic handler runs. This ensures the terminal is never left in raw mode on panics or early returns.

#### WebSocket Transport Splitting

The existing `GatewayWebSocketClient` wraps the WebSocket in a `Mutex<WebSocketStream>`, which deadlocks if a reader task holds the lock while the writer task also needs it. Instead of using `GatewayWebSocketClient` directly in the app loop:

- After `connect_async()`, **split** the `WebSocketStream` into read/write halves using `.split()`
- Spawn a **reader task**: loops reading frames from the read half, sends decoded `GatewayServerFrame`s into `mpsc::Sender<GatewayServerFrame>` (`gateway_rx`)
- Spawn a **writer task**: receives `GatewayClientFrame`s from `mpsc::Receiver<GatewayClientFrame>` (`ws_tx`), encodes and sends via the write half
- The main loop never touches the WebSocket directly — it only sends to `ws_tx` and receives from `gateway_rx`
- Both tasks hold `JoinHandle`s so they can be aborted and replaced on reconnection

#### State Flow (No Double-Send)

- **User submits prompt:** main loop calls `adapter.submit_prompt(...)` → adapter enqueues a `GatewayClientFrame::SendTurn` into its broadcast channel → main loop drains `adapter.listen()` receiver → forwards the frame to `ws_tx` (writer task sends it). The main loop does NOT send to the WebSocket directly.
- **Gateway sends frame:** reader task receives `GatewayServerFrame` → main loop receives via `gateway_rx` → calls `adapter.send(ChannelOutboundEvent { frame })` (updates `TuiUiState`) → calls `view_model.apply_server_frame(&frame)` (updates message history for rendering)
- **Result:** adapter is the single protocol state machine; view model is rendering-only.

#### `TuiApp` struct

- Fields: `adapter: TuiChannelAdapter`, `view_model: TuiViewModel`, `gateway_endpoint: String`, `user_id: String`, `connection_id: String`
- Constructor: takes `gateway_endpoint`, `user_id`, `connection_id`

#### `TuiApp::run(&mut self) -> Result<(), CliError>`

1. Create `TerminalGuard` (enables raw mode, enters alternate screen, hides cursor, enables mouse capture)
2. Create `ratatui::Terminal` with `CrosstermBackend`
3. Connect WebSocket, split into reader/writer tasks, send `Hello` frame via `ws_tx`, wait for `HelloAck` on `gateway_rx`
4. Create `EventReader` (spawns blocking input thread)
5. Start `adapter.listen()` to get inbound event receiver (`adapter_rx`)
6. Start `tokio::time::interval(Duration::from_millis(100))` for tick
7. **Main `tokio::select!` loop** over four channels:
   - `gateway_rx.recv()` → call `adapter.send(outbound_event)` + `view_model.apply_server_frame(&frame)`, if `auto_scroll` then `scroll_to_bottom()`
   - `adapter_rx.recv()` → forward `event.frame` to `ws_tx` (sends client frames to gateway)
   - `action_rx.recv()` → handle action:
     - `Submit`: if no active turn (check adapter snapshot), call `adapter.submit_prompt(...)`, append user message to view model, auto-scroll
     - `Cancel`: call `adapter.handle_ctrl_c(...)`, if `Exit` → break loop
     - `Quit` → break loop
     - Text editing actions → mutate `view_model` input buffer
     - Scroll actions → mutate `view_model` scroll offset; manual scroll disables `auto_scroll`
     - `Resize` → redraw (ratatui handles new size automatically on next `draw()`)
   - `tick.tick()` → increment `spinner_tick`, send `HealthCheck` via `ws_tx` every ~5s (not every tick)
   - After any event: `terminal.draw(|frame| render_app(frame, &self.view_model, &adapter_state))?`
8. On exit: `TerminalGuard` drop restores terminal automatically

#### Reconnection Strategy

When the reader task's `gateway_rx` channel closes (WebSocket disconnect):

1. Set `view_model.connection_state = Disconnected { since: Instant::now() }`
2. Abort reader and writer task handles
3. Enter reconnection loop with exponential backoff: 250ms → 500ms → 1s → 2s → 5s cap, with jitter
4. On each attempt: set `view_model.connection_state = Reconnecting { attempt, next_retry }`
5. On successful reconnect: split new socket, spawn new reader/writer tasks, send `Hello` with prior `runtime_session_id` via `adapter.build_hello_frame()`
6. On `HelloAck` received: set `view_model.connection_state = Connected`, re-enable input
7. During reconnection: input is disabled (grayed out), status bar shows reconnection state, queued outbound frames are dropped (user must re-submit after reconnect)
8. Continue main `select!` loop with new channels

#### Concurrency Constraints

- Terminal drawing (`terminal.draw()`) must happen from a single task — the main loop. Never draw from spawned tasks.
- Avoid holding `adapter`'s `Mutex<TuiUiState>` across awaits in the main loop — always take a snapshot (`adapter.state_snapshot().await`) then render from the snapshot.
- Reader/writer tasks hold `JoinHandle`s for clean abort on shutdown and reconnection.

### Step 6: Create `crates/tui/src/bin/oxydra-tui.rs` — Standalone TUI Binary

A thin binary entry point:

```
fn main() -> ExitCode:
  1. Parse CLI args: --gateway-endpoint <url> | --user <id> (for runner-based discovery)
  2. If --gateway-endpoint provided: connect directly
  3. If --user provided (or default): use runner's connect_tui() to discover endpoint
  4. Generate connection_id (UUID)
  5. Construct TuiApp with discovered endpoint + user_id
  6. tokio::runtime::Builder::new_multi_thread().build().block_on(app.run())
  7. Return exit code
```

Add `[[bin]]` entry in `crates/tui/Cargo.toml`:
```toml
[[bin]]
name = "oxydra-tui"
path = "src/bin/oxydra-tui.rs"
```

Add `clap` and `uuid` dependencies.

### Step 7: Wire `runner --tui` to Launch Interactive Client

The runner is currently a lean control CLI. Embedding a full-screen interactive TUI directly would pull UI dependencies into runner and blur responsibilities. Instead:

- **Preferred approach:** `runner --tui` discovers the gateway endpoint (existing `connect_tui()` logic), then **exec/spawns** the `oxydra-tui` binary, passing `--gateway-endpoint <url>` and `--user <id>` as arguments. This keeps runner lean and avoids pulling ratatui/crossterm into the runner crate.
- The current print-and-exit behavior is preserved as `runner --tui --probe` (add a `--probe` flag) for scripts/automation.
- Default `runner --tui` without `--probe` execs the interactive TUI client.
- If `oxydra-tui` binary is not found in `PATH`, emit a clear error message suggesting installation.

### Step 8: Update `lib.rs` — Module Registration

Add new modules to `crates/tui/src/lib.rs`:
```rust
mod ui_model;
mod widgets;
mod event_loop;
mod app;

pub use app::TuiApp;
pub use ui_model::*;
```

Keep existing `bootstrap` and `channel_adapter` exports unchanged.

### Step 9: Tests

- **`ui_model` unit tests**:
  - Verify `apply_server_frame` builds correct `message_history` for each frame type
  - Verify `GatewayTurnState::Failed` clears active turn and appends error
  - Verify `HelloAck` with `active_turn` on reconnect correctly represents in-progress streaming
  - Verify message history cap (inserting > `MAX_HISTORY_MESSAGES` trims oldest)
  - Verify scroll bounds, `auto_scroll` behavior, input buffer editing
  - Verify `ConnectionState` transitions
- **`widgets` snapshot tests**: render widgets to a `TestBackend` (ratatui provides this), assert expected layout content for each state (connected, disconnected, reconnecting, streaming, idle, error)
- **`event_loop` tests**: verify `KeyEvent` → `AppAction` mapping correctness, including `Event::Resize` mapping
- **`app` integration test**: mock WebSocket (use `tokio_tungstenite`'s test utilities or a localhost WS server), verify full handshake → prompt → delta → completion cycle drives correct view model state; verify reconnection flow
- **Existing tests**: all current `channel_adapter` tests remain untouched and must continue passing

### Step 10: Verify & Update Documentation

- Update `docs/guidebook/09-gateway-and-channels.md` section "Visual Rendering" to document the implemented rendering loop
- Update `docs/guidebook/15-progressive-build-plan.md` to mark Gap 2 as resolved
- Run `cargo clippy`, `cargo fmt`, `cargo test`, `cargo deny check` to ensure all gates pass

---

## Dependency Graph

```
Step 1 (deps) → Step 2 (model) → Step 3 (widgets) → Step 5 (app)
                                   Step 4 (events) → Step 5 (app)
Step 5 (app) → Step 6 (binary) → Step 7 (runner wiring)
Step 5 (app) → Step 8 (lib.rs)
Steps 2-5 → Step 9 (tests)
Step 9 → Step 10 (docs/verify)
```

Steps 2, 3, and 4 can be implemented in parallel since they have no mutual dependencies. Step 5 depends on all three. Steps 6 and 7 can proceed in parallel after Step 5.

---

## Design Decisions & Edge Cases

### State Ownership

`TuiChannelAdapter` is the single authoritative protocol state machine. `TuiViewModel` is rendering-only. The main loop bridges them:
- Inbound gateway frames → adapter updates `TuiUiState` → view model observes the frame for `message_history`
- User actions → adapter methods (`submit_prompt`, `handle_ctrl_c`) → adapter enqueues client frames → main loop forwards to WebSocket writer

This avoids double-sending and state drift.

### WebSocket Concurrency

The WebSocket stream is split into independent read/write halves after connection. The main loop communicates with them solely via `mpsc` channels. This avoids `Mutex` contention and enables clean reconnection (abort old tasks, spawn new ones with new socket halves).

### Terminal Safety

A `TerminalGuard` RAII struct plus a panic hook guarantee terminal restoration. The guard is created first and dropped last. All rendering happens from a single task (the main loop) — never from spawned tasks.

### Reconnection

Explicit state machine (`Connected` / `Disconnected` / `Reconnecting`) with exponential backoff (250ms → 5s cap, with jitter). Old reader/writer tasks are aborted on disconnect. Queued outbound frames are dropped during reconnection — the user must re-submit. The `HelloAck` with `active_turn` field handles reconnecting mid-stream.

### Edge Cases Handled

- **Terminal resize:** `Event::Resize` triggers redraw; ratatui recalculates layout on next `draw()`
- **Auto-scroll:** Only auto-scroll when user is already at the bottom; manual scrolling disables auto-scroll until `scroll_to_bottom()` is called
- **Message history cap:** Oldest messages are trimmed when exceeding `MAX_HISTORY_MESSAGES` to prevent unbounded memory growth
- **Failed turns:** `GatewayTurnState::Failed` clears active turn indicator and appends error to chat history
- **Reconnect mid-stream:** `HelloAck.active_turn` present on reconnect correctly represents in-progress streaming even though `TurnStarted` was not seen
- **Paste input:** The blocking input thread reads events continuously; the main loop batches redraws (renders once per `select!` iteration, not per character)
- **Health checks:** Sent via `ws_tx` every ~5 seconds when connected, not on every 100ms tick
