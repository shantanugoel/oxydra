//! TUI widget definitions for the interactive chat interface.
//!
//! Provides [`MessagePane`], [`InputBar`], and [`StatusBar`] widgets,
//! plus the [`render_app`] layout function that composes them into a
//! vertical split: message pane (fill), input bar (3 rows), status bar (1 row).

use ratatui::{
    Frame,
    buffer::Buffer,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph, Widget, Wrap},
};
use unicode_width::UnicodeWidthStr;

use crate::channel_adapter::TuiUiState;
use crate::ui_model::{ChatMessage, ConnectionState, TuiViewModel};

// ---------------------------------------------------------------------------
// MessagePane
// ---------------------------------------------------------------------------

/// Renders the chat message history with per-role styling inside a bordered
/// block.
///
/// - User messages: cyan, prefixed with `you: `
/// - Assistant messages: default color, no prefix
/// - Error messages: red, prefixed with `error: `
/// - System messages: dim yellow, prefixed with `--- `
///
/// When streaming is active (the adapter has an `active_turn_id` and the
/// latest message is an `Assistant` variant), a `|` cursor is appended to the
/// last line. Scroll position is respected via [`Paragraph::scroll`].
pub struct MessagePane<'a> {
    messages: &'a [ChatMessage],
    scroll_offset: usize,
    is_streaming: bool,
}

impl<'a> MessagePane<'a> {
    pub fn new(messages: &'a [ChatMessage], scroll_offset: usize, is_streaming: bool) -> Self {
        Self {
            messages,
            scroll_offset,
            is_streaming,
        }
    }
}

impl Widget for MessagePane<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let block = Block::bordered().title(" Messages ");
        let inner = block.inner(area);

        let lines = build_message_lines(self.messages, self.is_streaming);

        // Compute effective scroll. `usize::MAX` means "pin to bottom".
        let total_lines = wrapped_line_count(&lines, inner.width);
        let visible = inner.height as usize;
        let max_scroll = total_lines.saturating_sub(visible);
        let effective = if self.scroll_offset == usize::MAX {
            max_scroll
        } else {
            self.scroll_offset.min(max_scroll)
        };

        let scroll_y = u16::try_from(effective).unwrap_or(u16::MAX);
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(block)
            .scroll((scroll_y, 0))
            .render(area, buf);
    }
}

/// Approximate the total number of display lines after word-wrapping.
///
/// Each logical line occupies at least one display row; lines wider than
/// `width` wrap into `ceil(line_width / width)` rows.
fn wrapped_line_count(lines: &[Line<'_>], width: u16) -> usize {
    if width == 0 {
        return 0;
    }
    let w = width as usize;
    lines
        .iter()
        .map(|line| {
            let lw = line.width();
            if lw == 0 { 1 } else { lw.div_ceil(w) }
        })
        .sum()
}

/// Build styled [`Line`]s from the message history.
///
/// Messages are separated by blank lines for readability. A trailing `|`
/// streaming cursor is appended to the last assistant message when
/// `is_streaming` is true.
fn build_message_lines(messages: &[ChatMessage], is_streaming: bool) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let count = messages.len();

    for (i, msg) in messages.iter().enumerate() {
        if i > 0 {
            lines.push(Line::default());
        }
        let is_last = i + 1 == count;

        match msg {
            ChatMessage::User(text) => {
                let style = Style::new().fg(Color::Cyan);
                push_prefixed_lines(&mut lines, "you: ", text, style);
            }
            ChatMessage::Assistant(text) => {
                if is_streaming && is_last {
                    push_streaming_lines(&mut lines, text);
                } else {
                    push_content_lines(&mut lines, text, Style::default());
                }
            }
            ChatMessage::Error(text) => {
                let style = Style::new().fg(Color::Red);
                push_prefixed_lines(&mut lines, "error: ", text, style);
            }
            ChatMessage::System(text) => {
                let style = Style::new().fg(Color::Yellow).add_modifier(Modifier::DIM);
                push_prefixed_lines(&mut lines, "--- ", text, style);
            }
        }
    }

    lines
}

/// Push styled lines with a prefix on the first line.
fn push_prefixed_lines(lines: &mut Vec<Line<'static>>, prefix: &str, text: &str, style: Style) {
    let mut first = true;
    for segment in text.lines() {
        let content = if first {
            first = false;
            format!("{prefix}{segment}")
        } else {
            segment.to_owned()
        };
        lines.push(Line::from(Span::styled(content, style)));
    }
    // Empty text -- still show the prefix.
    if first {
        lines.push(Line::from(Span::styled(prefix.to_owned(), style)));
    }
}

/// Push content lines without a prefix, applying the given style.
fn push_content_lines(lines: &mut Vec<Line<'static>>, text: &str, style: Style) {
    if text.is_empty() {
        lines.push(Line::default());
        return;
    }
    for segment in text.lines() {
        lines.push(Line::from(Span::styled(segment.to_owned(), style)));
    }
}

/// Push assistant-message lines with a trailing `|` streaming cursor.
fn push_streaming_lines(lines: &mut Vec<Line<'static>>, text: &str) {
    let display = format!("{text}|");
    for segment in display.lines() {
        lines.push(Line::from(Span::raw(segment.to_owned())));
    }
}

// ---------------------------------------------------------------------------
// InputBar
// ---------------------------------------------------------------------------

/// Single-line text input with a bordered block.
///
/// The block title reflects prompt status (`" Prompt "` or `" Waiting... "`),
/// and the border color turns gray when input is disabled (active turn in
/// progress or connection lost).
pub struct InputBar<'a> {
    content: &'a str,
    cursor_byte_position: usize,
    is_disabled: bool,
    has_active_turn: bool,
}

impl<'a> InputBar<'a> {
    pub fn new(
        content: &'a str,
        cursor_byte_position: usize,
        is_disabled: bool,
        has_active_turn: bool,
    ) -> Self {
        Self {
            content,
            cursor_byte_position,
            is_disabled,
            has_active_turn,
        }
    }

    /// Compute the terminal cursor coordinates for
    /// [`Frame::set_cursor_position`].
    ///
    /// Returns `None` when input is disabled (the cursor should be hidden).
    /// Uses `unicode-width` to translate the byte-index cursor into a
    /// display-column offset.
    pub fn cursor_position(&self, area: Rect) -> Option<(u16, u16)> {
        if self.is_disabled {
            return None;
        }
        let prefix = &self.content[..self.cursor_byte_position];
        let display_offset = UnicodeWidthStr::width(prefix);
        // +1 for the left border; clamp so the cursor stays inside the area.
        let inner_width = area.width.saturating_sub(2);
        let cx = area.x.saturating_add(1).saturating_add(
            u16::try_from(display_offset)
                .unwrap_or(inner_width)
                .min(inner_width),
        );
        let cy = area.y.saturating_add(1); // +1 for the top border
        Some((cx, cy))
    }
}

impl Widget for InputBar<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let title = if self.has_active_turn {
            " Waiting... "
        } else {
            " Prompt "
        };
        let border_style = if self.is_disabled {
            Style::new().fg(Color::DarkGray)
        } else {
            Style::default()
        };

        let block = Block::bordered().title(title).border_style(border_style);

        Paragraph::new(self.content).block(block).render(area, buf);
    }
}

// ---------------------------------------------------------------------------
// StatusBar
// ---------------------------------------------------------------------------

/// Single-row status bar showing connection state, session info, and key
/// hints.
///
/// The connection indicator uses ASCII `o` (green when connected, red when
/// disconnected, yellow when reconnecting). The rightmost section shows
/// `[Ctrl+C to cancel]` during an active turn or `[Ctrl+C to exit]` when
/// idle.
pub struct StatusBar<'a> {
    connection_state: &'a ConnectionState,
    session_id: Option<&'a str>,
    model_name: Option<&'a str>,
    active_turn_id: Option<&'a str>,
}

impl<'a> StatusBar<'a> {
    pub fn new(
        connection_state: &'a ConnectionState,
        session_id: Option<&'a str>,
        model_name: Option<&'a str>,
        active_turn_id: Option<&'a str>,
    ) -> Self {
        Self {
            connection_state,
            session_id,
            model_name,
            active_turn_id,
        }
    }
}

impl Widget for StatusBar<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let mut spans: Vec<Span<'_>> = Vec::new();

        // Connection indicator: ASCII 'o' colored by state.
        match self.connection_state {
            ConnectionState::Connected => {
                spans.push(Span::styled("o", Style::new().fg(Color::Green)));
                spans.push(Span::raw(" connected"));
            }
            ConnectionState::Disconnected { .. } => {
                spans.push(Span::styled("o", Style::new().fg(Color::Red)));
                spans.push(Span::raw(" disconnected"));
            }
            ConnectionState::Reconnecting { attempt, .. } => {
                spans.push(Span::styled("o", Style::new().fg(Color::Yellow)));
                spans.push(Span::raw(format!(" reconnecting (attempt {attempt})")));
            }
        }

        // Session ID.
        if let Some(sid) = self.session_id {
            spans.push(Span::raw(format!(" | session: {sid}")));
        }

        // Model name (when available).
        if let Some(name) = self.model_name {
            spans.push(Span::raw(format!(" | model: {name}")));
        }

        // Turn state.
        if self.active_turn_id.is_some() {
            spans.push(Span::raw(" | streaming"));
        } else {
            spans.push(Span::raw(" | idle"));
        }

        // Key hint.
        let hint = if self.active_turn_id.is_some() {
            " [Ctrl+C to cancel]"
        } else {
            " [Ctrl+C to exit]"
        };
        spans.push(Span::styled(hint, Style::new().fg(Color::DarkGray)));

        Paragraph::new(Line::from(spans)).render(area, buf);
    }
}

// ---------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------

/// Render the full application layout into the terminal frame.
///
/// The layout splits vertically into three regions:
/// - **Message pane** (fills remaining space) -- scrollable chat history
/// - **Input bar** (3 rows) -- text input with border and status title
/// - **Status bar** (1 row) -- connection / session / turn info and key hints
///
/// The `adapter_state` snapshot drives protocol-aware visuals: the streaming
/// indicator appears when `active_turn_id` is set and the latest message is
/// from the assistant; input is disabled during an active turn or when the
/// connection is not in the [`ConnectionState::Connected`] state.
pub fn render_app(frame: &mut Frame, model: &TuiViewModel, adapter_state: &TuiUiState) {
    let [message_area, input_area, status_area] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    // -- Message pane --------------------------------------------------------

    let is_streaming = adapter_state.active_turn_id.is_some()
        && matches!(
            model.message_history.last(),
            Some(ChatMessage::Assistant(_))
        );

    frame.render_widget(
        MessagePane::new(&model.message_history, model.scroll_offset, is_streaming),
        message_area,
    );

    // -- Input bar -----------------------------------------------------------

    let is_disabled = adapter_state.active_turn_id.is_some()
        || !matches!(model.connection_state, ConnectionState::Connected);

    let input_bar = InputBar::new(
        &model.input_buffer,
        model.input_cursor_position,
        is_disabled,
        adapter_state.active_turn_id.is_some(),
    );
    let cursor_pos = input_bar.cursor_position(input_area);
    frame.render_widget(input_bar, input_area);

    if let Some((cx, cy)) = cursor_pos {
        frame.set_cursor_position((cx, cy));
    }

    // -- Status bar ----------------------------------------------------------

    frame.render_widget(
        StatusBar::new(
            &model.connection_state,
            adapter_state.runtime_session_id.as_deref(),
            None, // Model name not yet available in adapter state.
            adapter_state.active_turn_id.as_deref(),
        ),
        status_area,
    );
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend, layout::Position};

    use super::*;

    // ── Helpers ─────────────────────────────────────────────────────────

    /// Render `render_app` into a `TestBackend` terminal and return the
    /// underlying buffer for cell-level assertions.
    fn render(
        model: &TuiViewModel,
        adapter_state: &TuiUiState,
        width: u16,
        height: u16,
    ) -> ratatui::buffer::Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test terminal should initialize");
        terminal
            .draw(|frame| render_app(frame, model, adapter_state))
            .expect("draw should succeed");
        terminal.backend().buffer().clone()
    }

    /// Extract a row of text from the buffer (trimming trailing spaces).
    fn row_text(buf: &ratatui::buffer::Buffer, y: u16, width: u16) -> String {
        let mut text = String::new();
        for x in 0..width {
            if let Some(cell) = buf.cell(Position::new(x, y)) {
                text.push_str(cell.symbol());
            }
        }
        text.trim_end().to_owned()
    }

    fn default_connected_state() -> TuiUiState {
        TuiUiState {
            connected: true,
            runtime_session_id: Some("rt-1".to_owned()),
            active_turn_id: None,
            ..TuiUiState::default()
        }
    }

    // ── Snapshot tests ──────────────────────────────────────────────────

    #[test]
    fn connected_idle_shows_prompt_and_idle_status() {
        let model = TuiViewModel::new();
        let adapter = default_connected_state();
        let buf = render(&model, &adapter, 80, 24);

        // Status bar is the last row (y=23).
        let status = row_text(&buf, 23, 80);
        assert!(
            status.contains("connected"),
            "status bar should show 'connected', got: {status}"
        );
        assert!(
            status.contains("idle"),
            "status bar should show 'idle', got: {status}"
        );
        assert!(
            status.contains("[Ctrl+C to exit]"),
            "status bar should show exit hint, got: {status}"
        );

        // Input bar title should be " Prompt " (on row y=20, the top border
        // of the 3-row input area).
        let input_title_row = row_text(&buf, 20, 80);
        assert!(
            input_title_row.contains("Prompt"),
            "input bar should show 'Prompt' title, got: {input_title_row}"
        );
    }

    #[test]
    fn connected_streaming_shows_waiting_and_streaming_cursor() {
        let mut model = TuiViewModel::new();
        model
            .message_history
            .push(ChatMessage::User("hello".to_owned()));
        model
            .message_history
            .push(ChatMessage::Assistant("partial".to_owned()));

        let adapter = TuiUiState {
            connected: true,
            runtime_session_id: Some("rt-1".to_owned()),
            active_turn_id: Some("turn-1".to_owned()),
            ..TuiUiState::default()
        };
        let buf = render(&model, &adapter, 80, 24);

        // Status bar should show "streaming" and cancel hint.
        let status = row_text(&buf, 23, 80);
        assert!(
            status.contains("streaming"),
            "status bar should show 'streaming', got: {status}"
        );
        assert!(
            status.contains("[Ctrl+C to cancel]"),
            "status bar should show cancel hint, got: {status}"
        );

        // Input bar should show "Waiting..." title.
        let input_title_row = row_text(&buf, 20, 80);
        assert!(
            input_title_row.contains("Waiting..."),
            "input bar should show 'Waiting...' title, got: {input_title_row}"
        );

        // The assistant message should have a streaming cursor "|" appended.
        // The message pane inner area starts at row 1 (inside the border).
        // Row 1: "you: hello", row 2: blank separator, row 3: "partial|"
        let assistant_row = row_text(&buf, 3, 80);
        assert!(
            assistant_row.contains("partial|"),
            "assistant message should have streaming cursor, got: {assistant_row}"
        );
    }

    #[test]
    fn disconnected_state_renders_correctly() {
        let mut model = TuiViewModel::new();
        model.connection_state = ConnectionState::Disconnected {
            since: std::time::Instant::now(),
        };

        let adapter = TuiUiState {
            connected: false,
            ..TuiUiState::default()
        };
        let buf = render(&model, &adapter, 80, 24);

        let status = row_text(&buf, 23, 80);
        assert!(
            status.contains("disconnected"),
            "status bar should show 'disconnected', got: {status}"
        );
    }

    #[test]
    fn reconnecting_state_shows_attempt_count() {
        let mut model = TuiViewModel::new();
        model.connection_state = ConnectionState::Reconnecting {
            attempt: 3,
            next_retry: std::time::Instant::now(),
        };

        let adapter = TuiUiState {
            connected: false,
            ..TuiUiState::default()
        };
        let buf = render(&model, &adapter, 80, 24);

        let status = row_text(&buf, 23, 80);
        assert!(
            status.contains("reconnecting (attempt 3)"),
            "status bar should show reconnecting with attempt, got: {status}"
        );
    }

    #[test]
    fn error_message_renders_with_prefix() {
        let mut model = TuiViewModel::new();
        model
            .message_history
            .push(ChatMessage::Error("something broke".to_owned()));

        let adapter = default_connected_state();
        let buf = render(&model, &adapter, 80, 24);

        // Error message is in the message pane inner area at row 1.
        let error_row = row_text(&buf, 1, 80);
        assert!(
            error_row.contains("error: something broke"),
            "error message should have 'error: ' prefix, got: {error_row}"
        );
    }

    #[test]
    fn role_prefixes_render_correctly() {
        let mut model = TuiViewModel::new();
        model
            .message_history
            .push(ChatMessage::User("hi there".to_owned()));
        model
            .message_history
            .push(ChatMessage::Assistant("hello back".to_owned()));
        model
            .message_history
            .push(ChatMessage::System("session started".to_owned()));

        let adapter = default_connected_state();
        let buf = render(&model, &adapter, 80, 24);

        // Row 1: user message with "you: " prefix.
        let user_row = row_text(&buf, 1, 80);
        assert!(
            user_row.contains("you: hi there"),
            "user message should have 'you: ' prefix, got: {user_row}"
        );

        // Row 2: blank separator line.

        // Row 3: assistant message (no prefix, just content).
        let assistant_row = row_text(&buf, 3, 80);
        assert!(
            assistant_row.contains("hello back"),
            "assistant message should contain text, got: {assistant_row}"
        );
        // Assistant messages should NOT have a prefix.
        assert!(
            !assistant_row.contains("assistant:"),
            "assistant message should not have a role prefix"
        );

        // Row 4: blank separator.

        // Row 5: system message with "--- " prefix.
        let system_row = row_text(&buf, 5, 80);
        assert!(
            system_row.contains("--- session started"),
            "system message should have '--- ' prefix, got: {system_row}"
        );
    }

    #[test]
    fn input_bar_disabled_during_active_turn() {
        let model = TuiViewModel::new();
        let adapter = TuiUiState {
            connected: true,
            runtime_session_id: Some("rt-1".to_owned()),
            active_turn_id: Some("turn-1".to_owned()),
            ..TuiUiState::default()
        };
        let buf = render(&model, &adapter, 80, 24);

        // Input bar should show "Waiting..." when there's an active turn.
        let input_title_row = row_text(&buf, 20, 80);
        assert!(
            input_title_row.contains("Waiting..."),
            "input bar should show 'Waiting...' when active turn, got: {input_title_row}"
        );
    }

    #[test]
    fn input_bar_disabled_when_disconnected() {
        let mut model = TuiViewModel::new();
        model.connection_state = ConnectionState::Disconnected {
            since: std::time::Instant::now(),
        };
        model.input_buffer = "some text".to_owned();
        model.input_cursor_position = 9;

        let adapter = TuiUiState {
            connected: false,
            ..TuiUiState::default()
        };
        let buf = render(&model, &adapter, 80, 24);

        // Input bar should still show "Prompt" title (no active turn).
        let input_title_row = row_text(&buf, 20, 80);
        assert!(
            input_title_row.contains("Prompt"),
            "input bar should show 'Prompt' when disconnected, got: {input_title_row}"
        );
    }

    #[test]
    fn session_id_shown_in_status_bar() {
        let model = TuiViewModel::new();
        let adapter = TuiUiState {
            connected: true,
            runtime_session_id: Some("my-session-42".to_owned()),
            ..TuiUiState::default()
        };
        let buf = render(&model, &adapter, 80, 24);

        let status = row_text(&buf, 23, 80);
        assert!(
            status.contains("session: my-session-42"),
            "status bar should show session id, got: {status}"
        );
    }
}
