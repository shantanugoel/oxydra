//! Main TUI application loop.
//!
//! [`TuiApp`] orchestrates the terminal lifecycle: WebSocket transport
//! (split into independent reader/writer tasks), the channel adapter
//! (authoritative protocol state machine), the view model (rendering
//! state), the crossterm event reader, and a single `tokio::select!` loop
//! that draws to the terminal once per iteration.
//!
//! ## State Ownership
//!
//! - **`TuiChannelAdapter`** is the single authoritative owner of protocol
//!   state (`TuiUiState`). The main loop reads snapshots via
//!   `adapter.state_snapshot()` for rendering -- never holding the mutex
//!   across an `.await`.
//! - **`TuiViewModel`** is rendering-only. It observes `GatewayServerFrame`s
//!   to build `message_history` but never duplicates the protocol state
//!   machine.
//!
//! ## No Double-Send
//!
//! User submits prompt -> `adapter.submit_prompt()` enqueues a
//! `GatewayClientFrame` into its broadcast channel -> the main loop drains
//! `adapter.listen()` receiver -> forwards the frame to `ws_tx` (writer
//! task). The main loop never sends to the WebSocket directly except
//! through `ws_tx`.
//!
//! ## Terminal Safety
//!
//! [`TerminalGuard`] is an RAII struct whose `Drop` restores terminal
//! state. A panic hook is installed to attempt the same restoration before
//! the default handler runs.

use std::io::{self, Write};
use std::time::{Duration, Instant};

use crossterm::cursor;
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use types::{Channel, GatewayClientFrame, GatewayHealthCheck, GatewayServerFrame};

use crate::bootstrap::CliError;
use crate::channel_adapter::{TuiChannelAdapter, TuiCtrlCOutcome, encode_gateway_client_frame};
use crate::event_loop::{AppAction, EventReader};
use crate::ui_model::{ConnectionState, TuiViewModel};
use crate::widgets::render_app;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Tick interval for the main loop timer (spinner animation, periodic work).
const TICK_INTERVAL: Duration = Duration::from_millis(100);

/// How often to send a HealthCheck frame when connected (~5 seconds).
const HEALTH_CHECK_INTERVAL: Duration = Duration::from_secs(5);

/// Initial backoff for reconnection attempts.
const RECONNECT_BACKOFF_INITIAL: Duration = Duration::from_millis(250);

/// Maximum backoff cap for reconnection attempts.
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(5);

/// Capacity of the mpsc channel between reader task and main loop.
const GATEWAY_CHANNEL_CAPACITY: usize = 256;

/// Capacity of the mpsc channel between main loop and writer task.
const WS_WRITER_CHANNEL_CAPACITY: usize = 128;

/// Capacity of the event reader's action channel.
const EVENT_READER_BUFFER: usize = 64;

/// Capacity of the adapter listen stream.
const ADAPTER_LISTEN_BUFFER: usize = 128;

// ---------------------------------------------------------------------------
// TerminalGuard
// ---------------------------------------------------------------------------

/// RAII guard that restores terminal state on drop.
///
/// Created at the start of `TuiApp::run`, it enables raw mode, enters the
/// alternate screen, hides the cursor, and enables mouse capture. Its
/// [`Drop`] implementation reverses all of these. A complementary panic
/// hook is installed to attempt restoration even on unwind.
struct TerminalGuard;

impl TerminalGuard {
    /// Set up raw mode, alternate screen, hidden cursor, and mouse capture.
    /// Returns the guard whose `Drop` undoes everything.
    fn setup() -> Result<Self, CliError> {
        enable_raw_mode().map_err(io::Error::other)?;
        let mut stdout = io::stdout();
        execute!(
            stdout,
            EnterAlternateScreen,
            EnableMouseCapture,
            cursor::Hide,
        )
        .map_err(io::Error::other)?;

        // Install panic hook so the terminal is restored even on panics.
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // Best-effort restoration; ignore errors.
            let _ = restore_terminal();
            default_hook(info);
        }));

        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = restore_terminal();
    }
}

/// Best-effort terminal state restoration.
fn restore_terminal() -> Result<(), io::Error> {
    disable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        LeaveAlternateScreen,
        DisableMouseCapture,
        cursor::Show,
    )?;
    stdout.flush()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// WebSocket helpers
// ---------------------------------------------------------------------------

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
type WsSink = SplitSink<WsStream, WsMessage>;

/// Connect to the gateway endpoint and return the raw WebSocket stream.
async fn ws_connect(endpoint: &str) -> Result<WsStream, CliError> {
    let (socket, _) = connect_async(endpoint)
        .await
        .map_err(|e| CliError::Io(io::Error::new(io::ErrorKind::ConnectionRefused, e)))?;
    Ok(socket)
}

/// Spawn the reader task that decodes incoming WebSocket messages into
/// `GatewayServerFrame`s and sends them on `tx`.
///
/// Returns a `JoinHandle` for abort/cleanup.
fn spawn_ws_reader(
    mut read_half: futures_util::stream::SplitStream<WsStream>,
    tx: mpsc::Sender<GatewayServerFrame>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let msg = match read_half.next().await {
                Some(Ok(msg)) => msg,
                Some(Err(_)) | None => {
                    // Connection lost or closed.
                    return;
                }
            };

            let frame = match msg {
                WsMessage::Text(payload) => {
                    match serde_json::from_str::<GatewayServerFrame>(&payload) {
                        Ok(f) => f,
                        Err(_) => continue, // Skip malformed frames.
                    }
                }
                WsMessage::Binary(payload) => {
                    match serde_json::from_slice::<GatewayServerFrame>(&payload) {
                        Ok(f) => f,
                        Err(_) => continue,
                    }
                }
                WsMessage::Ping(_) | WsMessage::Pong(_) => continue,
                WsMessage::Close(_) => return,
                _ => continue,
            };

            if tx.send(frame).await.is_err() {
                // Receiver dropped -- main loop exited.
                return;
            }
        }
    })
}

/// Spawn the writer task that encodes outbound `GatewayClientFrame`s
/// and sends them on the WebSocket write half.
///
/// Returns a `JoinHandle` for abort/cleanup.
fn spawn_ws_writer(
    mut write_half: WsSink,
    mut rx: mpsc::Receiver<GatewayClientFrame>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            let payload = match encode_gateway_client_frame(&frame) {
                Ok(p) => p,
                Err(_) => continue,
            };
            if write_half
                .send(WsMessage::Text(payload.into()))
                .await
                .is_err()
            {
                // Connection lost.
                return;
            }
        }
    })
}

/// Generate a simple request-id using the current timestamp.
fn next_request_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("req-{nanos}")
}

/// Generate a simple turn id.
fn next_turn_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("turn-{nanos}")
}

/// Compute exponential backoff with jitter.
///
/// Returns a duration between `base * 2^attempt` and `base * 2^(attempt+1)`,
/// capped at `max_backoff`.
fn backoff_with_jitter(attempt: u32, base: Duration, max_backoff: Duration) -> Duration {
    let multiplier = 1u64.checked_shl(attempt).unwrap_or(u64::MAX);
    let raw = base.saturating_mul(multiplier.try_into().unwrap_or(u32::MAX));
    let capped = raw.min(max_backoff);
    // Simple jitter: pick a duration in [capped/2, capped].
    let half = capped / 2;
    let jitter_nanos = if half.as_nanos() > 0 {
        // Use cheap timestamp-based pseudo-random to avoid pulling in `rand`.
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos() as u128;
        (seed % half.as_nanos()) as u64
    } else {
        0
    };
    half + Duration::from_nanos(jitter_nanos)
}

// ---------------------------------------------------------------------------
// TuiApp
// ---------------------------------------------------------------------------

/// Main TUI application that drives the interactive terminal client.
///
/// Owns the channel adapter (authoritative protocol state), the view model
/// (rendering state), and the gateway endpoint information. Call
/// [`TuiApp::run`] to enter the interactive loop.
pub struct TuiApp {
    adapter: TuiChannelAdapter,
    view_model: TuiViewModel,
    gateway_endpoint: String,
}

impl TuiApp {
    /// Create a new `TuiApp`.
    ///
    /// - `gateway_endpoint`: WebSocket URL of the gateway (e.g.
    ///   `ws://127.0.0.1:9090/ws`).
    /// - `user_id`: user identifier for the gateway handshake.
    /// - `connection_id`: unique connection identifier.
    pub fn new(
        gateway_endpoint: impl Into<String>,
        user_id: impl Into<String>,
        connection_id: impl Into<String>,
    ) -> Self {
        let user_id = user_id.into();
        let connection_id = connection_id.into();
        Self {
            adapter: TuiChannelAdapter::new(user_id, connection_id),
            view_model: TuiViewModel::new(),
            gateway_endpoint: gateway_endpoint.into(),
        }
    }

    /// Run the interactive TUI loop.
    ///
    /// This takes ownership of the terminal (raw mode, alternate screen) via
    /// [`TerminalGuard`], connects to the gateway, and enters the main
    /// `tokio::select!` loop. Returns when the user quits.
    pub async fn run(&mut self) -> Result<(), CliError> {
        // 1. Terminal setup.
        let _guard = TerminalGuard::setup()?;
        let backend = CrosstermBackend::new(io::stdout());
        let mut terminal = Terminal::new(backend).map_err(io::Error::other)?;

        // 2. Initial WebSocket connection + Hello handshake.
        let (mut gateway_rx, mut ws_tx, mut reader_handle, mut writer_handle) =
            self.connect_and_handshake().await?;

        // 3. Event reader (blocking thread for crossterm input).
        let mut event_reader = EventReader::spawn(EVENT_READER_BUFFER);

        // 4. Adapter listen stream (client frames from adapter -> ws writer).
        let mut adapter_rx = self
            .adapter
            .listen(ADAPTER_LISTEN_BUFFER)
            .await
            .map_err(|e| CliError::Io(io::Error::other(e)))?;

        // 5. Tick timer.
        let mut tick = time::interval(TICK_INTERVAL);
        tick.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
        let mut last_health_check = Instant::now();

        // 6. Initial draw.
        {
            let adapter_state = self.adapter.state_snapshot().await;
            terminal.draw(|frame| render_app(frame, &self.view_model, &adapter_state))?;
        }

        // 7. Main select! loop.
        loop {
            let needs_draw;

            tokio::select! {
                // -- Inbound gateway frames from the WS reader task ----------
                frame_opt = gateway_rx.recv() => {
                    match frame_opt {
                        Some(frame) => {
                            // Update adapter (authoritative state).
                            self.adapter.apply_gateway_frame(&frame).await;
                            // Update view model (rendering state).
                            self.view_model.apply_server_frame(&frame);
                            if self.view_model.should_auto_scroll() {
                                self.view_model.scroll_to_bottom();
                            }
                            needs_draw = true;
                        }
                        None => {
                            // Reader task closed -- connection lost.
                            self.adapter.mark_disconnected().await;
                            self.view_model.connection_state =
                                ConnectionState::Disconnected { since: Instant::now() };

                            // Draw the disconnected state, then reconnect.
                            let adapter_state = self.adapter.state_snapshot().await;
                            let _ = terminal.draw(|frame| {
                                render_app(frame, &self.view_model, &adapter_state);
                            });

                            // Abort old tasks.
                            reader_handle.abort();
                            writer_handle.abort();

                            // Reconnect loop.
                            let (new_gw_rx, new_ws_tx, new_rh, new_wh) =
                                self.reconnect_loop(&mut terminal).await?;
                            gateway_rx = new_gw_rx;
                            ws_tx = new_ws_tx;
                            reader_handle = new_rh;
                            writer_handle = new_wh;
                            last_health_check = Instant::now();

                            // Re-create adapter listen stream for new
                            // outbound frames.
                            adapter_rx = self
                                .adapter
                                .listen(ADAPTER_LISTEN_BUFFER)
                                .await
                                .map_err(|e| {
                                    CliError::Io(io::Error::other(e))
                                })?;

                            needs_draw = true;
                        }
                    }
                }

                // -- Outbound client frames from the adapter -----------------
                event_opt = adapter_rx.recv() => {
                    match event_opt {
                        Some(Ok(event)) => {
                            // Forward client frame to ws writer.
                            // If not connected, drop the frame (reconnection
                            // scenario).
                            if matches!(
                                self.view_model.connection_state,
                                ConnectionState::Connected
                            ) {
                                let _ = ws_tx.send(event.frame).await;
                            }
                        }
                        Some(Err(_)) | None => {
                            // Channel closed or error -- not fatal for the
                            // main loop; adapter can be re-listened.
                        }
                    }
                    needs_draw = false;
                }

                // -- User input actions from the event reader ----------------
                action_opt = event_reader.receiver_mut().recv() => {
                    match action_opt {
                        Some(action) => {
                            let should_break =
                                self.handle_action(action, &mut ws_tx).await?;
                            if should_break {
                                break;
                            }
                            needs_draw = true;
                        }
                        None => {
                            // Event reader thread exited -- treat as quit.
                            break;
                        }
                    }
                }

                // -- Tick timer for spinner + health checks ------------------
                _ = tick.tick() => {
                    self.view_model.spinner_tick =
                        self.view_model.spinner_tick.wrapping_add(1);

                    // Send periodic health check when connected.
                    if matches!(
                        self.view_model.connection_state,
                        ConnectionState::Connected
                    ) && last_health_check.elapsed() >= HEALTH_CHECK_INTERVAL
                    {
                        let hc_frame = GatewayClientFrame::HealthCheck(GatewayHealthCheck {
                            request_id: next_request_id(),
                        });
                        let _ = ws_tx.send(hc_frame).await;
                        last_health_check = Instant::now();
                    }

                    needs_draw = true;
                }
            }

            // Single draw point per iteration.
            if needs_draw {
                let adapter_state = self.adapter.state_snapshot().await;
                terminal.draw(|frame| render_app(frame, &self.view_model, &adapter_state))?;
            }
        }

        // 8. Cleanup: abort WS tasks. TerminalGuard::drop restores terminal.
        reader_handle.abort();
        writer_handle.abort();

        Ok(())
    }

    // -- Connection helpers --------------------------------------------------

    /// Connect WebSocket, split, spawn reader/writer, perform Hello handshake.
    ///
    /// Returns `(gateway_rx, ws_tx, reader_handle, writer_handle)`.
    async fn connect_and_handshake(
        &mut self,
    ) -> Result<
        (
            mpsc::Receiver<GatewayServerFrame>,
            mpsc::Sender<GatewayClientFrame>,
            JoinHandle<()>,
            JoinHandle<()>,
        ),
        CliError,
    > {
        let socket = ws_connect(&self.gateway_endpoint).await?;
        let (write_half, read_half) = socket.split();

        let (gw_tx, mut gateway_rx) = mpsc::channel(GATEWAY_CHANNEL_CAPACITY);
        let (ws_tx, ws_rx) = mpsc::channel(WS_WRITER_CHANNEL_CAPACITY);

        let reader_handle = spawn_ws_reader(read_half, gw_tx);
        let writer_handle = spawn_ws_writer(write_half, ws_rx);

        // Send Hello frame.
        let hello = self.adapter.build_hello_frame(next_request_id()).await;
        ws_tx
            .send(hello)
            .await
            .map_err(|_| CliError::Io(io::Error::new(io::ErrorKind::BrokenPipe, "ws_tx closed")))?;

        // Wait for HelloAck.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(CliError::Io(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timeout waiting for HelloAck from gateway",
                )));
            }

            let frame = tokio::time::timeout(remaining, gateway_rx.recv()).await;
            match frame {
                Ok(Some(f @ GatewayServerFrame::HelloAck(_))) => {
                    self.adapter.apply_gateway_frame(&f).await;
                    self.view_model.apply_server_frame(&f);
                    self.view_model.connection_state = ConnectionState::Connected;
                    break;
                }
                Ok(Some(other)) => {
                    // Unexpected pre-handshake frame; process but keep waiting.
                    self.adapter.apply_gateway_frame(&other).await;
                    self.view_model.apply_server_frame(&other);
                }
                Ok(None) => {
                    return Err(CliError::Io(io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        "gateway connection closed before HelloAck",
                    )));
                }
                Err(_) => {
                    return Err(CliError::Io(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "timeout waiting for HelloAck from gateway",
                    )));
                }
            }
        }

        Ok((gateway_rx, ws_tx, reader_handle, writer_handle))
    }

    /// Reconnection loop with exponential backoff and jitter.
    ///
    /// Draws status updates to the terminal during reconnection so the user
    /// sees progress. On success, sends Hello with the prior session via
    /// `adapter.build_hello_frame()`.
    async fn reconnect_loop(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    ) -> Result<
        (
            mpsc::Receiver<GatewayServerFrame>,
            mpsc::Sender<GatewayClientFrame>,
            JoinHandle<()>,
            JoinHandle<()>,
        ),
        CliError,
    > {
        let mut attempt: u32 = 0;

        loop {
            attempt = attempt.saturating_add(1);
            let delay = backoff_with_jitter(
                attempt.saturating_sub(1),
                RECONNECT_BACKOFF_INITIAL,
                RECONNECT_BACKOFF_MAX,
            );
            let next_retry = Instant::now() + delay;

            self.view_model.connection_state = ConnectionState::Reconnecting {
                attempt,
                next_retry,
            };

            // Draw reconnection state.
            {
                let adapter_state = self.adapter.state_snapshot().await;
                let _ = terminal.draw(|frame| {
                    render_app(frame, &self.view_model, &adapter_state);
                });
            }

            time::sleep(delay).await;

            match self.try_reconnect().await {
                Ok(result) => return Ok(result),
                Err(_) => {
                    // Will loop and try again with increased backoff.
                }
            }
        }
    }

    /// Single reconnection attempt: connect, split, handshake.
    async fn try_reconnect(
        &mut self,
    ) -> Result<
        (
            mpsc::Receiver<GatewayServerFrame>,
            mpsc::Sender<GatewayClientFrame>,
            JoinHandle<()>,
            JoinHandle<()>,
        ),
        CliError,
    > {
        let socket = ws_connect(&self.gateway_endpoint).await?;
        let (write_half, read_half) = socket.split();

        let (gw_tx, mut gateway_rx) = mpsc::channel(GATEWAY_CHANNEL_CAPACITY);
        let (ws_tx, ws_rx) = mpsc::channel(WS_WRITER_CHANNEL_CAPACITY);

        let reader_handle = spawn_ws_reader(read_half, gw_tx);
        let writer_handle = spawn_ws_writer(write_half, ws_rx);

        // Send Hello with prior session for resume.
        let hello = self.adapter.build_hello_frame(next_request_id()).await;
        if ws_tx.send(hello).await.is_err() {
            reader_handle.abort();
            writer_handle.abort();
            return Err(CliError::Io(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "ws_tx closed during reconnect",
            )));
        }

        // Wait for HelloAck (shorter timeout for reconnection).
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                reader_handle.abort();
                writer_handle.abort();
                return Err(CliError::Io(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timeout waiting for HelloAck on reconnect",
                )));
            }

            match tokio::time::timeout(remaining, gateway_rx.recv()).await {
                Ok(Some(f @ GatewayServerFrame::HelloAck(_))) => {
                    self.adapter.apply_gateway_frame(&f).await;
                    self.view_model.apply_server_frame(&f);
                    self.view_model.connection_state = ConnectionState::Connected;
                    return Ok((gateway_rx, ws_tx, reader_handle, writer_handle));
                }
                Ok(Some(other)) => {
                    self.adapter.apply_gateway_frame(&other).await;
                    self.view_model.apply_server_frame(&other);
                }
                Ok(None) | Err(_) => {
                    reader_handle.abort();
                    writer_handle.abort();
                    return Err(CliError::Io(io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        "connection lost before HelloAck on reconnect",
                    )));
                }
            }
        }
    }

    // -- Action handler ------------------------------------------------------

    /// Handle a single user action. Returns `true` if the main loop should
    /// exit.
    async fn handle_action(
        &mut self,
        action: AppAction,
        ws_tx: &mut mpsc::Sender<GatewayClientFrame>,
    ) -> Result<bool, CliError> {
        match action {
            AppAction::Submit => {
                let input = self.view_model.input_buffer.trim().to_owned();
                if input.is_empty() {
                    return Ok(false);
                }

                // Only allow submit when connected and no active turn.
                let snapshot = self.adapter.state_snapshot().await;
                if snapshot.active_turn_id.is_some() {
                    return Ok(false);
                }
                if !matches!(self.view_model.connection_state, ConnectionState::Connected) {
                    return Ok(false);
                }

                let prompt = self.view_model.take_input();
                self.view_model.append_user_message(&prompt);
                if self.view_model.should_auto_scroll() {
                    self.view_model.scroll_to_bottom();
                }

                // This enqueues a client frame into the adapter's broadcast
                // channel, which the main loop's `adapter_rx` arm will
                // pick up and forward to `ws_tx`. No direct WS send here.
                let _ = self
                    .adapter
                    .submit_prompt(next_request_id(), next_turn_id(), prompt)
                    .await;
            }

            AppAction::Cancel => {
                let outcome = self
                    .adapter
                    .handle_ctrl_c(next_request_id())
                    .await
                    .unwrap_or(TuiCtrlCOutcome::Exit);
                if outcome == TuiCtrlCOutcome::Exit {
                    return Ok(true);
                }
                // CancelActiveTurn frame is enqueued via adapter -> adapter_rx
                // -> ws_tx path.
            }

            AppAction::Quit => {
                return Ok(true);
            }

            // -- Text editing ------------------------------------------------
            AppAction::Char(c) => self.view_model.insert_char(c),
            AppAction::Backspace => self.view_model.delete_char(),
            AppAction::Delete => {
                // Delete char at cursor (move right then backspace).
                self.view_model.move_cursor_right();
                self.view_model.delete_char();
            }
            AppAction::CursorLeft => self.view_model.move_cursor_left(),
            AppAction::CursorRight => self.view_model.move_cursor_right(),
            AppAction::Home => {
                self.view_model.input_cursor_position = 0;
            }
            AppAction::End => {
                self.view_model.input_cursor_position = self.view_model.input_buffer.len();
            }

            // -- Scrolling ---------------------------------------------------
            AppAction::ScrollUp => self.view_model.scroll_up(),
            AppAction::ScrollDown => self.view_model.scroll_down(),
            AppAction::PageUp => {
                for _ in 0..10 {
                    self.view_model.scroll_up();
                }
            }
            AppAction::PageDown => {
                for _ in 0..10 {
                    self.view_model.scroll_down();
                }
            }

            // -- Terminal resize ---------------------------------------------
            AppAction::Resize(_, _) => {
                // ratatui recalculates layout on next draw(); nothing to do.
            }
        }

        // Suppress unused-variable warning. ws_tx is used for potential
        // direct health check sends and available for future use.
        let _ = ws_tx;

        Ok(false)
    }
}
