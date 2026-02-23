//! Input event handling: bridges crossterm terminal events to [`AppAction`]s.
//!
//! [`EventReader`] spawns a dedicated blocking thread that calls
//! [`crossterm::event::read`] in a loop, maps raw terminal events to
//! [`AppAction`] variants, and sends them into a `tokio::sync::mpsc` channel.
//!
//! Tick events (for spinner animation and periodic health checks) are *not*
//! produced here — the main application loop drives those via
//! `tokio::time::interval`.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tokio::sync::mpsc;

/// Application-level action produced from raw terminal input.
///
/// The main loop matches on these to update [`TuiViewModel`] or dispatch
/// protocol actions through the [`TuiChannelAdapter`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppAction {
    /// A printable character typed by the user (including `'\n'` for newlines
    /// inserted via Alt+Enter).
    Char(char),
    /// Backspace key — delete the character before the cursor.
    Backspace,
    /// Delete key — delete the character at the cursor.
    Delete,
    /// Left arrow — move cursor one character left.
    CursorLeft,
    /// Right arrow — move cursor one character right.
    CursorRight,
    /// Home key — move cursor to the start of input.
    Home,
    /// End key — move cursor to the end of input.
    End,
    /// Enter key — submit the current input as a prompt.
    ///
    /// Alt+Enter inserts a literal newline ([`AppAction::Char('\n')`]) instead
    /// of submitting, enabling multi-line prompts.
    Submit,
    /// Scroll message pane up by one line (arrow up or mouse scroll up).
    ScrollUp,
    /// Scroll message pane down by one line (arrow down or mouse scroll down).
    ScrollDown,
    /// Page up — scroll message pane up by one page.
    PageUp,
    /// Page down — scroll message pane down by one page.
    PageDown,
    /// Ctrl+C — cancel the active turn or exit the application.
    Cancel,
    /// Ctrl+D or Ctrl+Q — quit the application unconditionally.
    Quit,
    /// Terminal window was resized to the given dimensions (columns, rows).
    Resize(u16, u16),
}

/// Reads terminal events on a dedicated blocking thread and forwards
/// [`AppAction`]s into an async mpsc channel.
///
/// The blocking thread is necessary because [`crossterm::event::read`] is a
/// synchronous blocking call. Using `std::thread::spawn` avoids compatibility
/// issues with tokio's runtime (e.g. `EventStream` requires the `event-stream`
/// feature and has edge cases with signal handling).
///
/// # Lifecycle
///
/// The reader thread runs until either:
/// - The receiving end of the channel is dropped (main loop exits).
/// - A fatal I/O error occurs reading from the terminal.
///
/// The thread is not explicitly joined — it will be cleaned up when the
/// process exits. The `JoinHandle` is retained so callers can optionally
/// join or check for panics.
pub struct EventReader {
    /// Handle to the blocking reader thread, retained for lifecycle management.
    _thread_handle: std::thread::JoinHandle<()>,
    /// Receiving end of the action channel.
    receiver: mpsc::Receiver<AppAction>,
}

impl EventReader {
    /// Spawn the blocking reader thread and return an `EventReader`.
    ///
    /// `buffer` controls the mpsc channel capacity. A reasonable default is
    /// 64 — large enough to absorb bursts of paste input without
    /// back-pressuring the terminal, small enough to bound memory.
    pub fn spawn(buffer: usize) -> Self {
        let (sender, receiver) = mpsc::channel(buffer.max(1));
        let thread_handle = std::thread::spawn(move || {
            reader_thread(sender);
        });
        Self {
            _thread_handle: thread_handle,
            receiver,
        }
    }

    /// Returns a mutable reference to the action receiver.
    ///
    /// Use `receiver.recv().await` in a `tokio::select!` branch.
    pub fn receiver_mut(&mut self) -> &mut mpsc::Receiver<AppAction> {
        &mut self.receiver
    }
}

/// Body of the blocking reader thread.
///
/// Loops calling `crossterm::event::read()`, maps each event to an
/// [`AppAction`], and sends it on the channel. Exits silently when the
/// channel is closed or on I/O error.
fn reader_thread(sender: mpsc::Sender<AppAction>) {
    loop {
        let event = match crossterm::event::read() {
            Ok(event) => event,
            Err(_) => {
                // Terminal I/O error (e.g. stdin closed). Nothing useful to do.
                return;
            }
        };

        let action = match map_event(event) {
            Some(action) => action,
            None => continue, // Unrecognised / irrelevant event.
        };

        // `blocking_send` is correct here: we are on a std thread, not a
        // tokio task. It blocks if the channel is full, which provides
        // natural back-pressure for extremely fast paste input.
        if sender.blocking_send(action).is_err() {
            // Receiver dropped — main loop has exited.
            return;
        }
    }
}

/// Map a raw crossterm [`Event`] to an [`AppAction`], if applicable.
///
/// Returns `None` for events we do not handle (mouse events without scroll,
/// focus events, key releases, etc.).
fn map_event(event: Event) -> Option<AppAction> {
    match event {
        Event::Key(key) => map_key_event(key),
        Event::Resize(cols, rows) => Some(AppAction::Resize(cols, rows)),
        // Mouse scroll could be mapped here in the future.
        _ => None,
    }
}

/// Map a crossterm [`KeyEvent`] to an [`AppAction`].
///
/// Only key-press events are handled; releases and repeats are ignored to
/// avoid duplicate actions on platforms that report them.
fn map_key_event(key: KeyEvent) -> Option<AppAction> {
    // crossterm 0.29 reports Press, Release, and Repeat events. We only
    // care about presses.
    if key.kind != KeyEventKind::Press {
        return None;
    }

    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    match (key.code, ctrl) {
        // -- Quit / Cancel ---------------------------------------------------
        (KeyCode::Char('c'), true) => Some(AppAction::Cancel),
        (KeyCode::Char('d'), true) | (KeyCode::Char('q'), true) => Some(AppAction::Quit),

        // -- Submit / Newline ------------------------------------------------
        // Alt+Enter inserts a literal newline into the input buffer so users
        // can compose multi-line prompts.  Plain Enter (with or without Shift)
        // submits the current prompt.
        (KeyCode::Enter, false) if alt => Some(AppAction::Char('\n')),
        (KeyCode::Enter, _) => Some(AppAction::Submit),

        // -- Text editing ----------------------------------------------------
        (KeyCode::Char(c), false) => Some(AppAction::Char(c)),
        (KeyCode::Backspace, _) => Some(AppAction::Backspace),
        (KeyCode::Delete, _) => Some(AppAction::Delete),

        // -- Cursor movement -------------------------------------------------
        (KeyCode::Left, _) => Some(AppAction::CursorLeft),
        (KeyCode::Right, _) => Some(AppAction::CursorRight),
        (KeyCode::Home, _) => Some(AppAction::Home),
        (KeyCode::End, _) => Some(AppAction::End),

        // -- Scrolling -------------------------------------------------------
        (KeyCode::Up, _) => Some(AppAction::ScrollUp),
        (KeyCode::Down, _) => Some(AppAction::ScrollDown),
        (KeyCode::PageUp, _) => Some(AppAction::PageUp),
        (KeyCode::PageDown, _) => Some(AppAction::PageDown),

        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    use super::*;

    /// Helper to build a press-type key event with given code and modifiers.
    fn press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    /// Helper to build a release-type key event.
    fn release(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: KeyEventState::NONE,
        }
    }

    // -- Character input ─────────────────────────────────────────────────

    #[test]
    fn printable_char_maps_to_char_action() {
        let action = map_key_event(press(KeyCode::Char('a'), KeyModifiers::NONE));
        assert_eq!(action, Some(AppAction::Char('a')));
    }

    #[test]
    fn space_maps_to_char_action() {
        let action = map_key_event(press(KeyCode::Char(' '), KeyModifiers::NONE));
        assert_eq!(action, Some(AppAction::Char(' ')));
    }

    // -- Modifier keys ───────────────────────────────────────────────────

    #[test]
    fn ctrl_c_maps_to_cancel() {
        let action = map_key_event(press(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(action, Some(AppAction::Cancel));
    }

    #[test]
    fn ctrl_d_maps_to_quit() {
        let action = map_key_event(press(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert_eq!(action, Some(AppAction::Quit));
    }

    #[test]
    fn ctrl_q_maps_to_quit() {
        let action = map_key_event(press(KeyCode::Char('q'), KeyModifiers::CONTROL));
        assert_eq!(action, Some(AppAction::Quit));
    }

    // -- Editing keys ────────────────────────────────────────────────────

    #[test]
    fn enter_maps_to_submit() {
        let action = map_key_event(press(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(action, Some(AppAction::Submit));
    }

    #[test]
    fn alt_enter_inserts_newline() {
        let action = map_key_event(press(KeyCode::Enter, KeyModifiers::ALT));
        assert_eq!(action, Some(AppAction::Char('\n')));
    }

    #[test]
    fn ctrl_enter_still_submits() {
        // Ctrl+Enter should not insert a newline — plain submit.
        let action = map_key_event(press(KeyCode::Enter, KeyModifiers::CONTROL));
        assert_eq!(action, Some(AppAction::Submit));
    }

    #[test]
    fn backspace_maps_to_backspace() {
        let action = map_key_event(press(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(action, Some(AppAction::Backspace));
    }

    #[test]
    fn delete_maps_to_delete() {
        let action = map_key_event(press(KeyCode::Delete, KeyModifiers::NONE));
        assert_eq!(action, Some(AppAction::Delete));
    }

    // -- Cursor movement ─────────────────────────────────────────────────

    #[test]
    fn left_arrow_maps_to_cursor_left() {
        let action = map_key_event(press(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(action, Some(AppAction::CursorLeft));
    }

    #[test]
    fn right_arrow_maps_to_cursor_right() {
        let action = map_key_event(press(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(action, Some(AppAction::CursorRight));
    }

    #[test]
    fn home_maps_to_home() {
        let action = map_key_event(press(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(action, Some(AppAction::Home));
    }

    #[test]
    fn end_maps_to_end() {
        let action = map_key_event(press(KeyCode::End, KeyModifiers::NONE));
        assert_eq!(action, Some(AppAction::End));
    }

    // -- Scrolling ────────────────────────────────────────────────────────

    #[test]
    fn up_arrow_maps_to_scroll_up() {
        let action = map_key_event(press(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(action, Some(AppAction::ScrollUp));
    }

    #[test]
    fn down_arrow_maps_to_scroll_down() {
        let action = map_key_event(press(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(action, Some(AppAction::ScrollDown));
    }

    #[test]
    fn page_up_maps_to_page_up() {
        let action = map_key_event(press(KeyCode::PageUp, KeyModifiers::NONE));
        assert_eq!(action, Some(AppAction::PageUp));
    }

    #[test]
    fn page_down_maps_to_page_down() {
        let action = map_key_event(press(KeyCode::PageDown, KeyModifiers::NONE));
        assert_eq!(action, Some(AppAction::PageDown));
    }

    // -- Key release filtering ───────────────────────────────────────────

    #[test]
    fn key_release_is_ignored() {
        let action = map_key_event(release(KeyCode::Char('a')));
        assert_eq!(action, None);
    }

    #[test]
    fn ctrl_c_release_is_ignored() {
        let mut key = release(KeyCode::Char('c'));
        key.modifiers = KeyModifiers::CONTROL;
        let action = map_key_event(key);
        assert_eq!(action, None);
    }

    // -- Resize event ────────────────────────────────────────────────────

    #[test]
    fn resize_event_maps_to_resize_action() {
        let action = map_event(Event::Resize(120, 40));
        assert_eq!(action, Some(AppAction::Resize(120, 40)));
    }

    // -- Unrecognised events ─────────────────────────────────────────────

    #[test]
    fn focus_event_returns_none() {
        let action = map_event(Event::FocusGained);
        assert_eq!(action, None);
    }

    #[test]
    fn unknown_key_returns_none() {
        let action = map_key_event(press(KeyCode::F(12), KeyModifiers::NONE));
        assert_eq!(action, None);
    }
}
