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

/// Compute the number of visual lines a raw string occupies when rendered
/// with character-level wrapping at `inner_width` columns.
///
/// Each logical line (separated by `\n`) occupies at least one visual line.
/// Lines wider than `inner_width` wrap into `ceil(display_width / inner_width)` rows.
fn visual_line_count_for_text(content: &str, inner_width: usize) -> usize {
    if inner_width == 0 {
        return 1;
    }
    content
        .split('\n')
        .map(|line| {
            let w = UnicodeWidthStr::width(line);
            if w == 0 { 1 } else { w.div_ceil(inner_width) }
        })
        .sum()
}

/// Compute the visual row of the cursor within input content.
///
/// This accounts for both logical newlines and visual wrapping of lines
/// that exceed `inner_width`. Each logical line before the cursor's line
/// contributes `ceil(display_width / inner_width)` visual rows (minimum 1).
/// The cursor's own logical line contributes `display_col / inner_width`
/// additional rows (integer division, since the cursor sits at a specific
/// column within the current visual row).
fn cursor_visual_row(content: &str, cursor_byte_position: usize, inner_width: usize) -> usize {
    if inner_width == 0 {
        return 0;
    }
    let prefix = &content[..cursor_byte_position];
    let logical_lines: Vec<&str> = prefix.split('\n').collect();

    // Visual rows from complete logical lines (all except the last).
    let mut visual_row: usize = 0;
    for line in &logical_lines[..logical_lines.len() - 1] {
        let w = UnicodeWidthStr::width(*line);
        visual_row += if w == 0 { 1 } else { w.div_ceil(inner_width) };
    }

    // Wrapped row offset within the last (current) logical line.
    let last_line = logical_lines.last().copied().unwrap_or("");
    let display_col = UnicodeWidthStr::width(last_line);
    visual_row += display_col / inner_width;

    visual_row
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

/// Single-line or multi-line text input with a bordered block.
///
/// The block title reflects prompt status (`" Prompt "`, `" Waiting... "`, or
/// a runtime activity message when one is available), and the border color
/// turns gray when input is disabled (active turn in progress or connection
/// lost).
///
/// Multi-line input is supported: pressing Alt+Enter inserts a newline into
/// the buffer, and the widget grows vertically to accommodate the content.
/// The layout in [`render_app`] computes the input area height dynamically
/// from the buffer's line count.
pub struct InputBar<'a> {
    content: &'a str,
    cursor_byte_position: usize,
    is_disabled: bool,
    has_active_turn: bool,
    /// Optional activity description shown in the title while a turn is
    /// running, e.g. `"[1/8] Executing tools: file_read"`.  When `None`
    /// and a turn is active, the title falls back to `" Waiting... "`.
    activity_message: Option<&'a str>,
    /// Vertical scroll offset (in visual lines) applied to the input content
    /// so the cursor line is always visible within the clamped height.
    input_scroll: u16,
}

impl<'a> InputBar<'a> {
    pub fn new(
        content: &'a str,
        cursor_byte_position: usize,
        is_disabled: bool,
        has_active_turn: bool,
        activity_message: Option<&'a str>,
        input_scroll: u16,
    ) -> Self {
        Self {
            content,
            cursor_byte_position,
            is_disabled,
            has_active_turn,
            activity_message,
            input_scroll,
        }
    }

    /// Compute the terminal cursor coordinates for
    /// [`Frame::set_cursor_position`].
    ///
    /// Returns `None` when input is disabled (the cursor should be hidden).
    ///
    /// For multi-line content, the method accounts for logical newlines and
    /// for visual line-wrapping within the inner widget width. Lines before
    /// the cursor's logical line that wrap into multiple visual rows are
    /// counted correctly. The result is adjusted by `input_scroll` so the
    /// cursor stays within the visible portion of the input area.
    pub fn cursor_position(&self, area: Rect) -> Option<(u16, u16)> {
        if self.is_disabled {
            return None;
        }
        let inner_width = area.width.saturating_sub(2) as usize;
        if inner_width == 0 {
            return None;
        }

        let visual_row = cursor_visual_row(self.content, self.cursor_byte_position, inner_width);

        // Cursor column within the current visual row.
        let prefix = &self.content[..self.cursor_byte_position];
        let last_logical_line = prefix.rsplit('\n').next().unwrap_or("");
        let display_col = UnicodeWidthStr::width(last_logical_line);
        let cursor_col_on_screen = display_col % inner_width;

        // Adjust for input scroll offset.
        let adjusted_row = visual_row.saturating_sub(self.input_scroll as usize);

        // +1 for the left border.
        let cx = area
            .x
            .saturating_add(1)
            .saturating_add(u16::try_from(cursor_col_on_screen).unwrap_or(u16::MAX));
        // +1 for the top border.
        let cy = area
            .y
            .saturating_add(1)
            .saturating_add(u16::try_from(adjusted_row).unwrap_or(u16::MAX));
        Some((cx, cy))
    }
}

impl Widget for InputBar<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let title = if self.has_active_turn {
            if let Some(status) = self.activity_message {
                format!(" {status} ")
            } else {
                " Waiting... ".to_owned()
            }
        } else {
            " Prompt ".to_owned()
        };
        let border_style = if self.is_disabled {
            Style::new().fg(Color::DarkGray)
        } else {
            Style::default()
        };

        let block = Block::bordered().title(title).border_style(border_style);

        Paragraph::new(self.content)
            .wrap(Wrap { trim: false })
            .block(block)
            .scroll((self.input_scroll, 0))
            .render(area, buf);
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

        // Session ID (shortened for readability).
        if let Some(sid) = self.session_id {
            let display_id = if sid.len() > 13 { &sid[..13] } else { sid };
            spans.push(Span::raw(format!(" | session: {display_id}")));
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
            " [Ctrl+C to exit | /new /sessions /switch /cancel /cancelall]"
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
/// - **Input bar** (3+ rows, grows with content) -- text input with border
/// - **Status bar** (1 row) -- connection / session / turn info and key hints
///
/// The input bar height is computed dynamically from the number of visual
/// lines (accounting for both logical newlines and character-level wrapping),
/// capped at 10 rows total (8 content + 2 borders). When content exceeds the
/// maximum height, the input paragraph scrolls to keep the cursor visible.
///
/// The `adapter_state` snapshot drives protocol-aware visuals: the streaming
/// indicator appears when `active_turn_id` is set and the latest message is
/// from the assistant; input is disabled during an active turn or when the
/// connection is not in the [`ConnectionState::Connected`] state.
pub fn render_app(frame: &mut Frame, model: &mut TuiViewModel, adapter_state: &TuiUiState) {
    // Compute input area height based on visual line count (accounting for
    // wrapping). The inner width is the terminal width minus two border
    // columns. We clamp content lines to 1..=8 to keep the input area
    // bounded while still growing for multi-line and wrapped content.
    let inner_width = frame.area().width.saturating_sub(2) as usize;
    let content_visual_lines = visual_line_count_for_text(&model.input_buffer, inner_width);
    let cursor_row = cursor_visual_row(
        &model.input_buffer,
        model.input_cursor_position,
        inner_width,
    );
    // Ensure the height accommodates the cursor row (which may be one past
    // the last content row when the cursor sits at the end of a full line).
    let visual_lines = content_visual_lines.max(cursor_row + 1).clamp(1, 8);
    let input_height = (visual_lines + 2) as u16; // +2 for top/bottom borders

    // Compute scroll offset to keep the cursor visible within the clamped
    // height. When the cursor's visual row exceeds the visible content rows,
    // scroll down so the cursor line is the last visible row.
    let input_scroll: u16 = if cursor_row >= visual_lines {
        u16::try_from(cursor_row - visual_lines + 1).unwrap_or(u16::MAX)
    } else {
        0
    };

    let [message_area, input_area, status_area] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(input_height),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    // -- Message pane --------------------------------------------------------

    let is_streaming = adapter_state.active_turn_id.is_some()
        && matches!(
            model.message_history.last(),
            Some(ChatMessage::Assistant(_))
        );

    // Compute and store the maximum scroll offset so that scroll_up/scroll_down
    // can resolve the usize::MAX "pin to bottom" sentinel to a real position.
    {
        let block = Block::bordered().title(" Messages ");
        let inner = block.inner(message_area);
        let lines = build_message_lines(&model.message_history, is_streaming);
        let total_lines = wrapped_line_count(&lines, inner.width);
        let visible = inner.height as usize;
        model.last_max_scroll = total_lines.saturating_sub(visible);
    }

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
        adapter_state.activity_status.as_deref(),
        input_scroll,
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
            adapter_state.session_id.as_deref(),
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
        model: &mut TuiViewModel,
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
            session_id: Some("rt-1".to_owned()),
            active_turn_id: None,
            ..TuiUiState::default()
        }
    }

    // ── Snapshot tests ──────────────────────────────────────────────────

    #[test]
    fn connected_idle_shows_prompt_and_idle_status() {
        let mut model = TuiViewModel::new();
        let adapter = default_connected_state();
        let buf = render(&mut model, &adapter, 80, 24);

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
            status.contains("[Ctrl+C to exit"),
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
            session_id: Some("rt-1".to_owned()),
            active_turn_id: Some("turn-1".to_owned()),
            ..TuiUiState::default()
        };
        let buf = render(&mut model, &adapter, 80, 24);

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
        let buf = render(&mut model, &adapter, 80, 24);

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
        let buf = render(&mut model, &adapter, 80, 24);

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
        let buf = render(&mut model, &adapter, 80, 24);

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
        let buf = render(&mut model, &adapter, 80, 24);

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
        let mut model = TuiViewModel::new();
        let adapter = TuiUiState {
            connected: true,
            session_id: Some("rt-1".to_owned()),
            active_turn_id: Some("turn-1".to_owned()),
            ..TuiUiState::default()
        };
        let buf = render(&mut model, &adapter, 80, 24);

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
        let buf = render(&mut model, &adapter, 80, 24);

        // Input bar should still show "Prompt" title (no active turn).
        let input_title_row = row_text(&buf, 20, 80);
        assert!(
            input_title_row.contains("Prompt"),
            "input bar should show 'Prompt' when disconnected, got: {input_title_row}"
        );
    }

    #[test]
    fn activity_message_appears_in_input_bar_title_during_active_turn() {
        let mut model = TuiViewModel::new();
        let adapter = TuiUiState {
            connected: true,
            session_id: Some("rt-1".to_owned()),
            active_turn_id: Some("turn-1".to_owned()),
            activity_status: Some("[2/8] Executing tools: file_read".to_owned()),
            ..TuiUiState::default()
        };
        let buf = render(&mut model, &adapter, 80, 24);

        // Input bar title should show the activity message instead of "Waiting...".
        let input_title_row = row_text(&buf, 20, 80);
        assert!(
            input_title_row.contains("Executing tools"),
            "input bar title should show activity message, got: {input_title_row}"
        );
        assert!(
            !input_title_row.contains("Waiting..."),
            "input bar should not show 'Waiting...' when activity_status is set, got: {input_title_row}"
        );
    }

    #[test]
    fn waiting_title_shown_when_active_turn_has_no_activity_message() {
        let mut model = TuiViewModel::new();
        let adapter = TuiUiState {
            connected: true,
            session_id: Some("rt-1".to_owned()),
            active_turn_id: Some("turn-1".to_owned()),
            activity_status: None,
            ..TuiUiState::default()
        };
        let buf = render(&mut model, &adapter, 80, 24);

        let input_title_row = row_text(&buf, 20, 80);
        assert!(
            input_title_row.contains("Waiting..."),
            "input bar should show 'Waiting...' when no activity_status, got: {input_title_row}"
        );
    }

    #[test]
    fn multiline_input_expands_input_area_height() {
        let mut model = TuiViewModel::new();
        // Two logical lines → input area height should be 4 (2 lines + 2 borders).
        model.input_buffer = "hello\nworld".to_owned();
        model.input_cursor_position = model.input_buffer.len();

        let adapter = TuiUiState {
            connected: true,
            session_id: Some("rt-1".to_owned()),
            ..TuiUiState::default()
        };

        // Render at 80x24. With 4-row input + 1-row status, message pane gets 19 rows.
        // Status bar stays on row 23 regardless.
        let buf = render(&mut model, &adapter, 80, 24);
        let status = row_text(&buf, 23, 80);
        assert!(
            status.contains("connected"),
            "status bar should still be on last row, got: {status}"
        );

        // The input area starts at row 19 (= 24 - 4 - 1) with a 4-row input bar.
        let input_title_row = row_text(&buf, 19, 80);
        assert!(
            input_title_row.contains("Prompt"),
            "input bar top border should be at row 19 for 2-line buffer, got: {input_title_row}"
        );
    }

    #[test]
    fn session_id_shown_in_status_bar() {
        let mut model = TuiViewModel::new();
        let adapter = TuiUiState {
            connected: true,
            session_id: Some("my-session-42-long-uuid".to_owned()),
            ..TuiUiState::default()
        };
        let buf = render(&mut model, &adapter, 80, 24);

        let status = row_text(&buf, 23, 80);
        // Session ID is shortened to first 13 chars for readability.
        assert!(
            status.contains("session: my-session-42"),
            "status bar should show shortened session id, got: {status}"
        );
    }

    // ── visual_line_count_for_text ──────────────────────────────────────

    #[test]
    fn visual_line_count_empty_string() {
        assert_eq!(visual_line_count_for_text("", 78), 1);
    }

    #[test]
    fn visual_line_count_single_short_line() {
        assert_eq!(visual_line_count_for_text("hello", 78), 1);
    }

    #[test]
    fn visual_line_count_two_logical_lines() {
        assert_eq!(visual_line_count_for_text("hello\nworld", 78), 2);
    }

    #[test]
    fn visual_line_count_wrapping_long_line() {
        // A line of 160 characters on a 78-column inner width wraps to 3 visual lines.
        let long_line = "a".repeat(160);
        assert_eq!(visual_line_count_for_text(&long_line, 78), 3);
    }

    #[test]
    fn visual_line_count_exact_width_is_one_line() {
        let exact = "a".repeat(78);
        assert_eq!(visual_line_count_for_text(&exact, 78), 1);
    }

    #[test]
    fn visual_line_count_one_over_width_wraps() {
        let one_over = "a".repeat(79);
        assert_eq!(visual_line_count_for_text(&one_over, 78), 2);
    }

    #[test]
    fn visual_line_count_zero_width_returns_one() {
        assert_eq!(visual_line_count_for_text("hello", 0), 1);
    }

    // ── cursor_visual_row ───────────────────────────────────────────────

    #[test]
    fn cursor_row_at_start() {
        assert_eq!(cursor_visual_row("hello", 0, 78), 0);
    }

    #[test]
    fn cursor_row_end_of_short_line() {
        assert_eq!(cursor_visual_row("hello", 5, 78), 0);
    }

    #[test]
    fn cursor_row_second_logical_line() {
        // "hello\nworld", cursor at end (byte 11)
        assert_eq!(cursor_visual_row("hello\nworld", 11, 78), 1);
    }

    #[test]
    fn cursor_row_after_wrapped_prior_line() {
        // First line is 160 chars (wraps to 3 visual lines on 78-wide),
        // cursor is on the second logical line.
        let content = format!("{}\nhi", "a".repeat(160));
        let cursor_pos = 160 + 1 + 2; // 160 'a's + '\n' + "hi"
        assert_eq!(cursor_visual_row(&content, cursor_pos, 78), 3);
    }

    #[test]
    fn cursor_row_within_wrapped_line() {
        // A single 160-char line, cursor at position 100.
        let content = "a".repeat(160);
        // Cursor at byte 100 → display_col 100, 100/78 = 1 → visual row 1
        assert_eq!(cursor_visual_row(&content, 100, 78), 1);
    }

    #[test]
    fn cursor_row_at_exact_wrap_boundary() {
        // Cursor at exactly position 78 in a long line → wraps to row 1
        let content = "a".repeat(160);
        assert_eq!(cursor_visual_row(&content, 78, 78), 1);
    }

    // ── Long-line input wrapping in render_app ──────────────────────────

    #[test]
    fn long_input_line_expands_input_area_for_wrapping() {
        let mut model = TuiViewModel::new();
        // A single line that wraps to 2 visual lines at 78 inner width.
        model.input_buffer = "a".repeat(100);
        model.input_cursor_position = 100;

        let adapter = default_connected_state();

        // Terminal 80 wide → inner width = 78. 100 chars wraps to 2 visual lines.
        // cursor_row = 100/78 = 1, so cursor is on row 1.
        // visual_lines = max(2, 1+1) = 2.
        // input_height = 4 (2 content + 2 borders).
        // Input top border at row 24 - 4 - 1 = 19.
        let buf = render(&mut model, &adapter, 80, 24);

        let input_title_row = row_text(&buf, 19, 80);
        assert!(
            input_title_row.contains("Prompt"),
            "input bar should expand for wrapped long line, got: {input_title_row}"
        );

        // Status bar should still be on the last row.
        let status = row_text(&buf, 23, 80);
        assert!(
            status.contains("connected"),
            "status bar should remain on last row, got: {status}"
        );
    }

    #[test]
    fn very_long_input_clamps_at_max_height_and_scrolls() {
        let mut model = TuiViewModel::new();
        // A single line long enough to wrap to ~10 visual lines (780 chars / 78 cols = 10).
        model.input_buffer = "a".repeat(780);
        model.input_cursor_position = 780;

        let adapter = default_connected_state();

        // visual_lines clamped to 8, input_height = 10.
        // cursor_row = 780/78 = 10, which exceeds visual_lines (8),
        // so input_scroll = 10 - 8 + 1 = 3.
        // Input top border at row 24 - 10 - 1 = 13.
        let buf = render(&mut model, &adapter, 80, 24);

        let input_title_row = row_text(&buf, 13, 80);
        assert!(
            input_title_row.contains("Prompt"),
            "input bar should clamp at max height, got: {input_title_row}"
        );

        let status = row_text(&buf, 23, 80);
        assert!(
            status.contains("connected"),
            "status bar should remain on last row, got: {status}"
        );
    }
}
