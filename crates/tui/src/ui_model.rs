use std::time::Instant;

use types::{GatewayServerFrame, GatewayTurnState};

/// Maximum number of chat messages retained in history to prevent unbounded memory growth.
const MAX_HISTORY_MESSAGES: usize = 1000;

/// Connection lifecycle state for the TUI client.
///
/// Reflected in the status bar and input bar visual state — e.g., a disconnected
/// or reconnecting state disables input and shows appropriate indicators.
#[derive(Debug, Clone)]
pub enum ConnectionState {
    /// Normal operation — WebSocket is connected and healthy.
    Connected,
    /// Connection lost at the given instant.
    Disconnected { since: Instant },
    /// Actively reconnecting with exponential backoff.
    Reconnecting { attempt: u32, next_retry: Instant },
}

/// A single entry in the chat message history.
///
/// The view model accumulates these from gateway frames and user input.
/// `Assistant` variants are built incrementally from `AssistantDelta` frames
/// and finalized on `TurnCompleted`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatMessage {
    /// A prompt submitted by the user.
    User(String),
    /// An assistant response, built incrementally from streaming deltas.
    Assistant(String),
    /// An error surfaced to the user (protocol errors, failed turns, etc.).
    Error(String),
    /// A system/status message (connection events, reconnection notices, etc.).
    System(String),
}

/// Rendering-only view model consumed by the TUI rendering loop.
///
/// `TuiViewModel` does **not** embed or duplicate `TuiUiState`. The adapter
/// remains the single authoritative owner of protocol/gateway state. This
/// struct only tracks what is needed for visual rendering and local interaction:
/// message history, scroll position, input buffer, and connection state.
///
/// The main loop bridges the two: inbound gateway frames update the adapter
/// (via `adapter.send()`), then the same frame is passed to
/// `view_model.apply_server_frame()` to update message history.
pub struct TuiViewModel {
    /// Accumulated conversation messages (role + content), capped at
    /// [`MAX_HISTORY_MESSAGES`] to prevent unbounded memory growth.
    pub message_history: Vec<ChatMessage>,
    /// Vertical scroll position in the message pane (line offset from top).
    ///
    /// The sentinel value [`usize::MAX`] means "pin to bottom" and is resolved
    /// to the actual maximum scroll offset during rendering.
    pub scroll_offset: usize,
    /// When true, new output auto-scrolls the message pane. Set to `true` when
    /// the user is at the bottom; manual scrolling disables it.
    pub auto_scroll: bool,
    /// Maximum scroll offset as computed during the most recent render pass.
    ///
    /// Used by [`scroll_up`] and [`scroll_down`] to resolve the `usize::MAX`
    /// sentinel into an actual position before decrementing/incrementing.
    /// Updated by [`render_app`](crate::widgets::render_app) each frame.
    pub last_max_scroll: usize,
    /// Local input text being composed by the user.
    pub input_buffer: String,
    /// Cursor byte-index within [`input_buffer`].
    pub input_cursor_position: usize,
    /// Human-readable status line (connection status, model, turn state).
    pub status_line: String,
    /// Animation frame counter for the streaming spinner indicator.
    pub spinner_tick: usize,
    /// Current connection lifecycle state.
    pub connection_state: ConnectionState,
}

impl Default for TuiViewModel {
    fn default() -> Self {
        Self::new()
    }
}

impl TuiViewModel {
    /// Create a new view model with default (empty/connected) state.
    pub fn new() -> Self {
        Self {
            message_history: Vec::new(),
            scroll_offset: 0,
            auto_scroll: true,
            last_max_scroll: 0,
            input_buffer: String::new(),
            input_cursor_position: 0,
            status_line: String::new(),
            spinner_tick: 0,
            connection_state: ConnectionState::Connected,
        }
    }

    // ── Server frame handling ───────────────────────────────────────────

    /// Update `message_history` based on an inbound gateway frame.
    ///
    /// This method handles rendering concerns only — it does **not** touch
    /// protocol state (that is the adapter's job). Specifically:
    ///
    /// - `AssistantDelta`: appends delta text to the last `Assistant` message,
    ///   or creates a new one if the last message is not `Assistant`.
    /// - `TurnCompleted`: finalizes the last assistant message with the full
    ///   response content (if no deltas were received, creates one).
    /// - `TurnCancelled`: appends a system message noting cancellation.
    /// - `Error`: appends an error message. If the error's turn state is
    ///   `Failed`, also appends a system notice about the failed turn.
    /// - `HelloAck`: if reconnecting with an `active_turn`, appends a system
    ///   message noting the resumed stream.
    /// - `TurnStarted`: appends a new empty `Assistant` message ready for deltas.
    /// - `HealthStatus`: no-op for message history.
    pub fn apply_server_frame(&mut self, frame: &GatewayServerFrame) {
        match frame {
            GatewayServerFrame::HelloAck(ack) => {
                if let Some(turn) = &ack.active_turn
                    && turn.state == GatewayTurnState::Running
                {
                    self.push_message(ChatMessage::System(
                        "Reconnected — resuming active stream".to_owned(),
                    ));
                    // Ensure an assistant message is ready for incoming deltas.
                    self.push_message(ChatMessage::Assistant(String::new()));
                }
            }
            GatewayServerFrame::TurnStarted(_) => {
                // Prepare an empty assistant message for incoming deltas.
                self.push_message(ChatMessage::Assistant(String::new()));
            }
            GatewayServerFrame::AssistantDelta(delta) => {
                self.append_to_last_assistant(&delta.delta);
            }
            GatewayServerFrame::TurnCompleted(completed) => {
                let final_content = completed.response.message.content.as_deref().unwrap_or("");

                // If the last message is an in-progress assistant message,
                // replace its content with the final text (unless deltas
                // already populated it and the final text is empty).
                match self.message_history.last_mut() {
                    Some(ChatMessage::Assistant(buf)) if buf.is_empty() => {
                        *buf = final_content.to_owned();
                    }
                    Some(ChatMessage::Assistant(_)) => {
                        // Deltas already populated — keep as-is.
                    }
                    _ => {
                        // No assistant message in progress; create one.
                        if !final_content.is_empty() {
                            self.push_message(ChatMessage::Assistant(final_content.to_owned()));
                        }
                    }
                }
            }
            GatewayServerFrame::TurnCancelled(_) => {
                self.push_message(ChatMessage::System("Turn cancelled".to_owned()));
            }
            GatewayServerFrame::Error(error) => {
                self.push_message(ChatMessage::Error(error.message.clone()));

                // If the turn failed, note it explicitly so the user knows
                // the turn is no longer active.
                if let Some(turn) = &error.turn
                    && turn.state == GatewayTurnState::Failed
                {
                    self.push_message(ChatMessage::System(
                        format!("Turn {} failed", turn.turn_id,),
                    ));
                }
            }
            GatewayServerFrame::HealthStatus(_) => {
                // Health status does not affect message history.
            }
            GatewayServerFrame::TurnProgress(_) => {
                // Progress events are reflected in the adapter's `activity_status`
                // field and shown in the input bar title.  They do not produce
                // permanent chat history entries.
            }
            GatewayServerFrame::ScheduledNotification(notification) => {
                let label = notification
                    .schedule_name
                    .as_deref()
                    .unwrap_or(&notification.schedule_id);
                self.push_message(ChatMessage::System(format!(
                    "[Scheduled: {label}] {}",
                    notification.message
                )));
            }
            GatewayServerFrame::SessionCreated(created) => {
                let name = created
                    .display_name
                    .as_deref()
                    .unwrap_or(&created.session.session_id);
                self.push_message(ChatMessage::System(format!(
                    "New session created: {name} (agent: {})",
                    created.agent_name,
                )));
            }
            GatewayServerFrame::SessionList(list) => {
                if list.sessions.is_empty() {
                    self.push_message(ChatMessage::System("No sessions found.".to_owned()));
                } else {
                    self.push_message(ChatMessage::System("Sessions:".to_owned()));
                    for s in &list.sessions {
                        let name = s.display_name.as_deref().unwrap_or("-");
                        let short_id = if s.session_id.len() > 13 {
                            &s.session_id[..13]
                        } else {
                            &s.session_id
                        };
                        self.push_message(ChatMessage::System(format!(
                            "  {short_id}  {name}  ({}, last active: {})",
                            s.agent_name, s.last_active_at,
                        )));
                    }
                    self.push_message(ChatMessage::System(
                        "Use /switch <session_id> to switch sessions.".to_owned(),
                    ));
                }
            }
            GatewayServerFrame::SessionSwitched(switched) => {
                self.push_message(ChatMessage::System(format!(
                    "Switched to session: {}",
                    switched.session.session_id,
                )));
                if let Some(ref turn) = switched.active_turn
                    && turn.state == GatewayTurnState::Running
                {
                    self.push_message(ChatMessage::System(
                        "Active turn in progress — resuming stream.".to_owned(),
                    ));
                    self.push_message(ChatMessage::Assistant(String::new()));
                }
            }
            GatewayServerFrame::MediaAttachment(media) => {
                // In the TUI, media attachments cannot be displayed inline.
                // Show a system message indicating media was sent via the channel.
                let file_name = media.attachment.file_name.as_deref().unwrap_or("file");
                self.push_message(ChatMessage::System(format!(
                    "📎 Sent {:?}: {file_name}",
                    media.attachment.media_type,
                )));
            }
        }
    }

    // ── Message history helpers ─────────────────────────────────────────

    /// Append a user message to history and enforce the cap.
    pub fn append_user_message(&mut self, prompt: &str) {
        self.push_message(ChatMessage::User(prompt.to_owned()));
    }

    /// Push a message and trim oldest entries if over cap.
    pub fn push_message(&mut self, message: ChatMessage) {
        self.message_history.push(message);
        self.enforce_history_cap();
    }

    /// Append text to the last `Assistant` message, creating one if needed.
    fn append_to_last_assistant(&mut self, text: &str) {
        match self.message_history.last_mut() {
            Some(ChatMessage::Assistant(buf)) => {
                buf.push_str(text);
            }
            _ => {
                self.push_message(ChatMessage::Assistant(text.to_owned()));
            }
        }
    }

    /// Trim oldest messages when history exceeds the cap.
    fn enforce_history_cap(&mut self) {
        if self.message_history.len() > MAX_HISTORY_MESSAGES {
            let excess = self.message_history.len() - MAX_HISTORY_MESSAGES;
            self.message_history.drain(..excess);
            // Adjust scroll offset to remain valid after trimming.
            self.scroll_offset = self.scroll_offset.saturating_sub(excess);
        }
    }

    // ── Scrolling ───────────────────────────────────────────────────────

    /// Scroll up by one line.
    ///
    /// When `scroll_offset` is the `usize::MAX` sentinel (meaning "pinned to
    /// bottom"), it is first resolved to `last_max_scroll` — the actual
    /// maximum offset from the most recent render — before decrementing.
    pub fn scroll_up(&mut self) {
        // Resolve the "pin to bottom" sentinel to a real position.
        if self.scroll_offset == usize::MAX {
            self.scroll_offset = self.last_max_scroll;
        }
        if self.scroll_offset > 0 {
            self.scroll_offset -= 1;
            self.auto_scroll = false;
        }
    }

    /// Scroll down by one line.
    ///
    /// When `scroll_offset` is the `usize::MAX` sentinel it is first
    /// resolved to `last_max_scroll` before incrementing. The offset is
    /// capped at `last_max_scroll` so the user cannot scroll past the
    /// bottom; reaching the bottom re-enables auto-scroll.
    pub fn scroll_down(&mut self) {
        // Resolve the "pin to bottom" sentinel to a real position.
        if self.scroll_offset == usize::MAX {
            self.scroll_offset = self.last_max_scroll;
        }
        if self.scroll_offset < self.last_max_scroll {
            self.scroll_offset = self.scroll_offset.saturating_add(1);
        }
        // Re-enable auto-scroll when the user reaches the bottom.
        if self.scroll_offset >= self.last_max_scroll {
            self.auto_scroll = true;
        }
    }

    /// Jump to the bottom of the message pane and re-enable auto-scroll.
    pub fn scroll_to_bottom(&mut self) {
        // The exact offset is determined during rendering when the total line
        // count is known. Setting to `usize::MAX` signals "pin to bottom".
        self.scroll_offset = usize::MAX;
        self.auto_scroll = true;
    }

    /// Returns `true` if auto-scroll is active (user is at or near the bottom).
    pub fn should_auto_scroll(&self) -> bool {
        self.auto_scroll
    }

    // ── Input buffer editing ────────────────────────────────────────────

    /// Insert a character at the current cursor position.
    pub fn insert_char(&mut self, c: char) {
        self.input_buffer.insert(self.input_cursor_position, c);
        self.input_cursor_position += c.len_utf8();
    }

    /// Delete the character before the cursor (backspace).
    pub fn delete_char(&mut self) {
        if self.input_cursor_position > 0 {
            // Walk back one character boundary.
            let prev = self.input_buffer[..self.input_cursor_position]
                .char_indices()
                .next_back()
                .map(|(idx, _)| idx)
                .unwrap_or(0);
            self.input_buffer.drain(prev..self.input_cursor_position);
            self.input_cursor_position = prev;
        }
    }

    /// Move the cursor one character to the left.
    pub fn move_cursor_left(&mut self) {
        if self.input_cursor_position > 0 {
            self.input_cursor_position = self.input_buffer[..self.input_cursor_position]
                .char_indices()
                .next_back()
                .map(|(idx, _)| idx)
                .unwrap_or(0);
        }
    }

    /// Move the cursor one character to the right.
    pub fn move_cursor_right(&mut self) {
        if self.input_cursor_position < self.input_buffer.len() {
            self.input_cursor_position = self.input_buffer[self.input_cursor_position..]
                .char_indices()
                .nth(1)
                .map(|(idx, _)| self.input_cursor_position + idx)
                .unwrap_or(self.input_buffer.len());
        }
    }

    /// Drain the input buffer and return its contents, resetting cursor.
    pub fn take_input(&mut self) -> String {
        self.input_cursor_position = 0;
        std::mem::take(&mut self.input_buffer)
    }
}

#[cfg(test)]
mod tests {
    use types::{
        GatewayAssistantDelta, GatewayErrorFrame, GatewayHelloAck, GatewayServerFrame,
        GatewaySession, GatewayTurnCancelled, GatewayTurnCompleted, GatewayTurnStarted,
        GatewayTurnState, GatewayTurnStatus, Message, MessageRole, Response,
    };

    use super::*;

    // ── Test helpers ────────────────────────────────────────────────────

    fn session(session_id: &str) -> GatewaySession {
        GatewaySession {
            user_id: "alice".to_owned(),
            session_id: session_id.to_owned(),
        }
    }

    fn turn_status(turn_id: &str, state: GatewayTurnState) -> GatewayTurnStatus {
        GatewayTurnStatus {
            turn_id: turn_id.to_owned(),
            state,
        }
    }

    fn hello_ack_frame(active_turn: Option<GatewayTurnStatus>) -> GatewayServerFrame {
        GatewayServerFrame::HelloAck(GatewayHelloAck {
            request_id: "req-1".to_owned(),
            protocol_version: 1,
            session: session("rt-1"),
            active_turn,
        })
    }

    fn turn_started_frame(turn_id: &str) -> GatewayServerFrame {
        GatewayServerFrame::TurnStarted(GatewayTurnStarted {
            request_id: "req-1".to_owned(),
            session: session("rt-1"),
            turn: turn_status(turn_id, GatewayTurnState::Running),
        })
    }

    fn delta_frame(turn_id: &str, text: &str) -> GatewayServerFrame {
        GatewayServerFrame::AssistantDelta(GatewayAssistantDelta {
            request_id: "req-1".to_owned(),
            session: session("rt-1"),
            turn: turn_status(turn_id, GatewayTurnState::Running),
            delta: text.to_owned(),
        })
    }

    fn completed_frame(turn_id: &str, final_text: &str) -> GatewayServerFrame {
        GatewayServerFrame::TurnCompleted(GatewayTurnCompleted {
            request_id: "req-1".to_owned(),
            session: session("rt-1"),
            turn: turn_status(turn_id, GatewayTurnState::Completed),
            response: Response {
                message: Message {
                    role: MessageRole::Assistant,
                    content: Some(final_text.to_owned()),
                    tool_calls: Vec::new(),
                    tool_call_id: None,
                    attachments: Vec::new(),
                },
                tool_calls: Vec::new(),
                finish_reason: Some("stop".to_owned()),
                usage: None,
            },
        })
    }

    fn cancelled_frame(turn_id: &str) -> GatewayServerFrame {
        GatewayServerFrame::TurnCancelled(GatewayTurnCancelled {
            request_id: "req-1".to_owned(),
            session: session("rt-1"),
            turn: turn_status(turn_id, GatewayTurnState::Cancelled),
        })
    }

    fn error_frame(message: &str, turn: Option<GatewayTurnStatus>) -> GatewayServerFrame {
        GatewayServerFrame::Error(GatewayErrorFrame {
            request_id: Some("req-1".to_owned()),
            session: Some(session("rt-1")),
            turn,
            message: message.to_owned(),
        })
    }

    // ── apply_server_frame tests ────────────────────────────────────────

    #[test]
    fn turn_started_creates_empty_assistant_message() {
        let mut vm = TuiViewModel::new();
        vm.apply_server_frame(&turn_started_frame("t1"));

        assert_eq!(vm.message_history.len(), 1);
        assert_eq!(vm.message_history[0], ChatMessage::Assistant(String::new()));
    }

    #[test]
    fn deltas_append_to_last_assistant_message() {
        let mut vm = TuiViewModel::new();
        vm.apply_server_frame(&turn_started_frame("t1"));
        vm.apply_server_frame(&delta_frame("t1", "hel"));
        vm.apply_server_frame(&delta_frame("t1", "lo"));

        assert_eq!(vm.message_history.len(), 1);
        assert_eq!(
            vm.message_history[0],
            ChatMessage::Assistant("hello".to_owned())
        );
    }

    #[test]
    fn delta_without_prior_assistant_creates_one() {
        let mut vm = TuiViewModel::new();
        vm.apply_server_frame(&delta_frame("t1", "surprise"));

        assert_eq!(vm.message_history.len(), 1);
        assert_eq!(
            vm.message_history[0],
            ChatMessage::Assistant("surprise".to_owned())
        );
    }

    #[test]
    fn turn_completed_keeps_delta_content() {
        let mut vm = TuiViewModel::new();
        vm.apply_server_frame(&turn_started_frame("t1"));
        vm.apply_server_frame(&delta_frame("t1", "streamed"));
        vm.apply_server_frame(&completed_frame("t1", "streamed"));

        assert_eq!(vm.message_history.len(), 1);
        assert_eq!(
            vm.message_history[0],
            ChatMessage::Assistant("streamed".to_owned())
        );
    }

    #[test]
    fn turn_completed_without_deltas_uses_final_message() {
        let mut vm = TuiViewModel::new();
        vm.apply_server_frame(&turn_started_frame("t1"));
        vm.apply_server_frame(&completed_frame("t1", "full response"));

        assert_eq!(vm.message_history.len(), 1);
        assert_eq!(
            vm.message_history[0],
            ChatMessage::Assistant("full response".to_owned())
        );
    }

    #[test]
    fn turn_completed_without_any_prior_message_creates_assistant() {
        let mut vm = TuiViewModel::new();
        vm.apply_server_frame(&completed_frame("t1", "standalone"));

        assert_eq!(vm.message_history.len(), 1);
        assert_eq!(
            vm.message_history[0],
            ChatMessage::Assistant("standalone".to_owned())
        );
    }

    #[test]
    fn turn_cancelled_appends_system_message() {
        let mut vm = TuiViewModel::new();
        vm.apply_server_frame(&turn_started_frame("t1"));
        vm.apply_server_frame(&cancelled_frame("t1"));

        assert_eq!(vm.message_history.len(), 2);
        assert_eq!(
            vm.message_history[1],
            ChatMessage::System("Turn cancelled".to_owned())
        );
    }

    #[test]
    fn error_frame_appends_error_message() {
        let mut vm = TuiViewModel::new();
        vm.apply_server_frame(&error_frame("something broke", None));

        assert_eq!(vm.message_history.len(), 1);
        assert_eq!(
            vm.message_history[0],
            ChatMessage::Error("something broke".to_owned())
        );
    }

    #[test]
    fn failed_turn_appends_error_and_system_notice() {
        let mut vm = TuiViewModel::new();
        let failed_turn = turn_status("t1", GatewayTurnState::Failed);
        vm.apply_server_frame(&error_frame("provider timeout", Some(failed_turn)));

        assert_eq!(vm.message_history.len(), 2);
        assert_eq!(
            vm.message_history[0],
            ChatMessage::Error("provider timeout".to_owned())
        );
        assert_eq!(
            vm.message_history[1],
            ChatMessage::System("Turn t1 failed".to_owned())
        );
    }

    #[test]
    fn hello_ack_with_active_turn_appends_reconnect_notice() {
        let mut vm = TuiViewModel::new();
        let active = turn_status("t1", GatewayTurnState::Running);
        vm.apply_server_frame(&hello_ack_frame(Some(active)));

        assert_eq!(vm.message_history.len(), 2);
        assert_eq!(
            vm.message_history[0],
            ChatMessage::System("Reconnected — resuming active stream".to_owned())
        );
        assert_eq!(vm.message_history[1], ChatMessage::Assistant(String::new()));
    }

    #[test]
    fn hello_ack_without_active_turn_is_silent() {
        let mut vm = TuiViewModel::new();
        vm.apply_server_frame(&hello_ack_frame(None));

        assert!(vm.message_history.is_empty());
    }

    #[test]
    fn health_status_does_not_affect_history() {
        let mut vm = TuiViewModel::new();
        vm.apply_server_frame(&GatewayServerFrame::HealthStatus(
            types::GatewayHealthStatus {
                request_id: "req-1".to_owned(),
                healthy: true,
                session: Some(session("rt-1")),
                active_turn: None,
                startup_status: None,
                message: None,
            },
        ));

        assert!(vm.message_history.is_empty());
    }

    #[test]
    fn turn_progress_does_not_affect_history() {
        use types::{GatewayTurnProgress, RuntimeProgressEvent, RuntimeProgressKind};

        let mut vm = TuiViewModel::new();
        vm.apply_server_frame(&GatewayServerFrame::TurnProgress(GatewayTurnProgress {
            request_id: "req-1".to_owned(),
            session: session("rt-1"),
            turn: turn_status("t1", GatewayTurnState::Running),
            progress: RuntimeProgressEvent {
                kind: RuntimeProgressKind::ProviderCall,
                message: "[1/8] Calling provider".to_owned(),
                turn: 1,
                max_turns: 8,
            },
        }));

        // Progress events do not produce chat history entries.
        assert!(
            vm.message_history.is_empty(),
            "turn_progress should not add to message history"
        );
    }

    // ── Full turn cycle ─────────────────────────────────────────────────

    #[test]
    fn full_turn_cycle_produces_user_and_assistant_messages() {
        let mut vm = TuiViewModel::new();
        vm.append_user_message("hello");
        vm.apply_server_frame(&turn_started_frame("t1"));
        vm.apply_server_frame(&delta_frame("t1", "hi "));
        vm.apply_server_frame(&delta_frame("t1", "there"));
        vm.apply_server_frame(&completed_frame("t1", "hi there"));

        assert_eq!(vm.message_history.len(), 2);
        assert_eq!(vm.message_history[0], ChatMessage::User("hello".to_owned()));
        assert_eq!(
            vm.message_history[1],
            ChatMessage::Assistant("hi there".to_owned())
        );
    }

    // ── History cap ─────────────────────────────────────────────────────

    #[test]
    fn history_cap_trims_oldest_messages() {
        let mut vm = TuiViewModel::new();
        for i in 0..MAX_HISTORY_MESSAGES + 50 {
            vm.append_user_message(&format!("msg-{i}"));
        }

        assert_eq!(vm.message_history.len(), MAX_HISTORY_MESSAGES);
        // Oldest retained message should be msg-50.
        assert_eq!(
            vm.message_history[0],
            ChatMessage::User("msg-50".to_owned())
        );
    }

    #[test]
    fn history_cap_adjusts_scroll_offset() {
        let mut vm = TuiViewModel::new();
        vm.scroll_offset = 20;
        for _ in 0..MAX_HISTORY_MESSAGES + 10 {
            vm.append_user_message("x");
        }
        // 10 messages were trimmed; scroll offset should decrease by 10.
        assert_eq!(vm.scroll_offset, 10);
    }

    // ── Scroll behavior ─────────────────────────────────────────────────

    #[test]
    fn scroll_up_disables_auto_scroll() {
        let mut vm = TuiViewModel::new();
        vm.scroll_offset = 5;
        vm.auto_scroll = true;
        vm.scroll_up();

        assert_eq!(vm.scroll_offset, 4);
        assert!(!vm.auto_scroll);
    }

    #[test]
    fn scroll_up_at_zero_is_noop() {
        let mut vm = TuiViewModel::new();
        vm.scroll_up();

        assert_eq!(vm.scroll_offset, 0);
        // auto_scroll remains true since nothing happened.
        assert!(vm.auto_scroll);
    }

    #[test]
    fn scroll_down_increments() {
        let mut vm = TuiViewModel::new();
        // last_max_scroll must be > 0 for scroll_down to increment.
        vm.last_max_scroll = 10;
        vm.scroll_down();

        assert_eq!(vm.scroll_offset, 1);
    }

    #[test]
    fn scroll_down_at_max_is_noop() {
        let mut vm = TuiViewModel::new();
        vm.last_max_scroll = 5;
        vm.scroll_offset = 5;
        vm.auto_scroll = false;
        vm.scroll_down();

        assert_eq!(vm.scroll_offset, 5);
        // Reaching the bottom re-enables auto-scroll.
        assert!(vm.auto_scroll);
    }

    #[test]
    fn scroll_down_reaching_bottom_reenables_auto_scroll() {
        let mut vm = TuiViewModel::new();
        vm.last_max_scroll = 3;
        vm.scroll_offset = 2;
        vm.auto_scroll = false;
        vm.scroll_down();

        assert_eq!(vm.scroll_offset, 3);
        assert!(vm.auto_scroll);
    }

    #[test]
    fn scroll_to_bottom_enables_auto_scroll() {
        let mut vm = TuiViewModel::new();
        vm.auto_scroll = false;
        vm.scroll_offset = 10;
        vm.scroll_to_bottom();

        assert!(vm.auto_scroll);
        assert_eq!(vm.scroll_offset, usize::MAX);
    }

    #[test]
    fn scroll_up_from_sentinel_resolves_to_last_max_scroll() {
        let mut vm = TuiViewModel::new();
        vm.scroll_offset = usize::MAX; // "pinned to bottom"
        vm.last_max_scroll = 20;
        vm.scroll_up();

        // Should resolve usize::MAX → 20, then decrement to 19.
        assert_eq!(vm.scroll_offset, 19);
        assert!(!vm.auto_scroll);
    }

    #[test]
    fn scroll_down_from_sentinel_resolves_and_stays() {
        let mut vm = TuiViewModel::new();
        vm.scroll_offset = usize::MAX;
        vm.last_max_scroll = 20;
        vm.auto_scroll = false;
        vm.scroll_down();

        // Should resolve usize::MAX → 20, then stay at 20 (already at max).
        assert_eq!(vm.scroll_offset, 20);
        assert!(vm.auto_scroll); // re-enabled because at bottom
    }

    #[test]
    fn should_auto_scroll_reflects_flag() {
        let mut vm = TuiViewModel::new();
        assert!(vm.should_auto_scroll());

        vm.auto_scroll = false;
        assert!(!vm.should_auto_scroll());
    }

    // ── Input buffer editing ────────────────────────────────────────────

    #[test]
    fn insert_char_at_end() {
        let mut vm = TuiViewModel::new();
        vm.insert_char('a');
        vm.insert_char('b');
        vm.insert_char('c');

        assert_eq!(vm.input_buffer, "abc");
        assert_eq!(vm.input_cursor_position, 3);
    }

    #[test]
    fn insert_char_in_middle() {
        let mut vm = TuiViewModel::new();
        vm.insert_char('a');
        vm.insert_char('c');
        vm.move_cursor_left();
        vm.insert_char('b');

        assert_eq!(vm.input_buffer, "abc");
        assert_eq!(vm.input_cursor_position, 2);
    }

    #[test]
    fn delete_char_removes_before_cursor() {
        let mut vm = TuiViewModel::new();
        vm.insert_char('a');
        vm.insert_char('b');
        vm.insert_char('c');
        vm.delete_char();

        assert_eq!(vm.input_buffer, "ab");
        assert_eq!(vm.input_cursor_position, 2);
    }

    #[test]
    fn delete_char_at_start_is_noop() {
        let mut vm = TuiViewModel::new();
        vm.insert_char('a');
        vm.move_cursor_left();
        vm.delete_char();

        assert_eq!(vm.input_buffer, "a");
        assert_eq!(vm.input_cursor_position, 0);
    }

    #[test]
    fn move_cursor_left_and_right() {
        let mut vm = TuiViewModel::new();
        vm.insert_char('a');
        vm.insert_char('b');
        vm.insert_char('c');

        vm.move_cursor_left();
        assert_eq!(vm.input_cursor_position, 2);
        vm.move_cursor_left();
        assert_eq!(vm.input_cursor_position, 1);
        vm.move_cursor_right();
        assert_eq!(vm.input_cursor_position, 2);
    }

    #[test]
    fn move_cursor_boundaries() {
        let mut vm = TuiViewModel::new();
        vm.insert_char('x');

        // Already at end — right should stay.
        vm.move_cursor_right();
        assert_eq!(vm.input_cursor_position, 1);

        // Move to start — left should stay.
        vm.move_cursor_left();
        vm.move_cursor_left();
        assert_eq!(vm.input_cursor_position, 0);
    }

    #[test]
    fn take_input_drains_buffer() {
        let mut vm = TuiViewModel::new();
        vm.insert_char('h');
        vm.insert_char('i');

        let taken = vm.take_input();
        assert_eq!(taken, "hi");
        assert!(vm.input_buffer.is_empty());
        assert_eq!(vm.input_cursor_position, 0);
    }

    // ── Multi-byte UTF-8 handling ───────────────────────────────────────

    #[test]
    fn multibyte_insert_and_delete() {
        let mut vm = TuiViewModel::new();
        vm.insert_char('é');
        vm.insert_char('ñ');

        assert_eq!(vm.input_buffer, "éñ");
        assert_eq!(vm.input_cursor_position, 4); // é=2 bytes, ñ=2 bytes

        vm.delete_char();
        assert_eq!(vm.input_buffer, "é");
        assert_eq!(vm.input_cursor_position, 2);
    }

    #[test]
    fn multibyte_cursor_movement() {
        let mut vm = TuiViewModel::new();
        vm.insert_char('日');
        vm.insert_char('本');

        vm.move_cursor_left();
        assert_eq!(vm.input_cursor_position, 3); // 日=3 bytes
        vm.move_cursor_left();
        assert_eq!(vm.input_cursor_position, 0);
        vm.move_cursor_right();
        assert_eq!(vm.input_cursor_position, 3);
    }

    // ── Default trait ───────────────────────────────────────────────────

    #[test]
    fn default_is_consistent_with_new() {
        let from_new = TuiViewModel::new();
        let from_default = TuiViewModel::default();

        assert_eq!(
            from_new.message_history.len(),
            from_default.message_history.len()
        );
        assert_eq!(from_new.scroll_offset, from_default.scroll_offset);
        assert_eq!(from_new.auto_scroll, from_default.auto_scroll);
        assert_eq!(from_new.last_max_scroll, from_default.last_max_scroll);
        assert_eq!(from_new.input_buffer, from_default.input_buffer);
        assert_eq!(
            from_new.input_cursor_position,
            from_default.input_cursor_position
        );
        assert_eq!(from_new.status_line, from_default.status_line);
        assert_eq!(from_new.spinner_tick, from_default.spinner_tick);
    }
}
