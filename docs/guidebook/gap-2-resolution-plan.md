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

No new workspace-level dependencies needed; these are TUI-crate-only.

### Step 2: Create `crates/tui/src/ui_model.rs` — Extended UI State for Rendering

Extend the rendering state beyond `TuiUiState` (which remains the gateway-synced state). Create a view model that the rendering loop consumes:

- **`TuiViewModel`** struct wrapping `TuiUiState` with rendering-specific fields:
  - `message_history: Vec<ChatMessage>` — accumulated conversation messages (role + content)
  - `scroll_offset: usize` — vertical scroll position in the message pane
  - `input_cursor_position: usize` — cursor index within the input buffer
  - `status_line: String` — connection status / active model / turn state
  - `spinner_tick: usize` — animation frame counter for streaming indicator

- **`ChatMessage`** enum:
  - `User(String)` — user prompt
  - `Assistant(String)` — assistant response (built incrementally from deltas)
  - `Error(String)` — error display
  - `System(String)` — connection/status messages

- **Methods on `TuiViewModel`**:
  - `apply_gateway_frame(&mut self, frame: &GatewayServerFrame)` — delegates to `TuiUiState::apply_gateway_frame` and also updates `message_history` (appends deltas to the last assistant message, finalizes on `TurnCompleted`)
  - `append_user_message(&mut self, prompt: &str)` — adds user message to history
  - `scroll_up() / scroll_down() / scroll_to_bottom()` — adjust scroll offset
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

- **`EventReader`** struct:
  - Spawns a task polling `crossterm::event::EventStream` (async-compatible)
  - Maps `KeyEvent` → `AppAction`
  - Sends `Tick` every ~100ms via `tokio::time::interval`
  - Returns actions via an `mpsc::Receiver<AppAction>`

### Step 5: Create `crates/tui/src/app.rs` — Main Application Loop

The core orchestration tying everything together:

- **`TuiApp`** struct:
  - Fields: `adapter: TuiChannelAdapter`, `ws_client: GatewayWebSocketClient`, `view_model: TuiViewModel`
  - Constructor: takes `gateway_endpoint`, `user_id`, `connection_id`

- **`TuiApp::run(&mut self) -> Result<(), CliError>`**:
  1. Initialize crossterm: `enable_raw_mode()`, `EnterAlternateScreen`, mouse capture
  2. Create `ratatui::Terminal` with `CrosstermBackend`
  3. Send `Hello` frame via `ws_client`, wait for `HelloAck`
  4. Spawn **WebSocket receive task**: loop calling `ws_client.receive_frame()`, sending frames into an `mpsc::Sender<GatewayServerFrame>`
  5. Create `EventReader` for terminal input
  6. **Main select! loop** over three channels:
     - `gateway_rx.recv()` → `view_model.apply_gateway_frame(frame)` → trigger redraw
     - `action_rx.recv()` → handle action:
       - `Submit`: if no active turn, call `adapter.submit_prompt(...)`, also send `SendTurn` via `ws_client`, append user message to view model
       - `Cancel`: call `adapter.handle_ctrl_c(...)`, if `Exit` → break loop
       - `Quit` → break loop
       - Text editing actions → mutate `view_model` input buffer
       - Scroll actions → mutate `view_model` scroll offset
       - `Tick` → increment spinner, trigger redraw
     - After any event: `terminal.draw(|frame| render_app(frame, &self.view_model))?`
  7. On exit: `disable_raw_mode()`, `LeaveAlternateScreen`, restore terminal

- **Error handling**: if WebSocket disconnects, show error in status bar, attempt reconnect with `build_hello_frame` (reusing existing `runtime_session_id`)

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

Modify `crates/runner/src/main.rs`:
- Instead of just printing connection metadata and exiting, after `connect_tui()` succeeds, construct and run `TuiApp` using the discovered `gateway_endpoint` and `user_id`
- The print-and-exit behavior becomes `--tui --probe` (add a `--probe` flag) for scripts/automation
- Default `--tui` without `--probe` launches the interactive client

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

- **`ui_model` unit tests**: verify `apply_gateway_frame` builds correct `message_history`, scroll bounds, input buffer editing
- **`widgets` snapshot tests**: render widgets to a `TestBackend` (ratatui provides this), assert expected layout content
- **`event_loop` tests**: verify `KeyEvent` → `AppAction` mapping correctness
- **`app` integration test**: mock WebSocket (use `tokio_tungstenite`'s test utilities or a localhost WS server), verify full handshake → prompt → delta → completion cycle drives correct view model state
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
