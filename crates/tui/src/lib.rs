mod app;
mod channel_adapter;
mod event_loop;
mod ui_model;
mod widgets;

use std::io;
use thiserror::Error;

pub use app::TuiApp;
pub use channel_adapter::*;
pub use ui_model::*;

/// Error type for TUI operations.
///
/// The TUI crate only deals with I/O errors from terminal and WebSocket
/// operations. Bootstrap/config/provider errors live in the `runner` crate.
#[derive(Debug, Error)]
pub enum TuiError {
    #[error(transparent)]
    Io(#[from] io::Error),
}
