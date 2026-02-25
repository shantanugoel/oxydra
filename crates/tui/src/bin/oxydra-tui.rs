//! Standalone TUI binary entry point.
//!
//! Parses CLI arguments, resolves the gateway endpoint, generates a unique
//! connection identifier, and runs the interactive TUI application inside a
//! multi-threaded tokio runtime.
//!
//! ## Usage
//!
//! Connect directly to a known gateway endpoint:
//!
//! ```text
//! oxydra-tui --gateway-endpoint ws://127.0.0.1:9090/ws --user alice
//! ```
//!
//! When `--gateway-endpoint` is omitted a future runner-based discovery
//! mechanism will resolve it automatically (not yet wired).

use std::process::ExitCode;

use clap::Parser;
use tui::TuiApp;
use uuid::Uuid;

/// Interactive terminal client for Oxydra.
#[derive(Parser, Debug)]
#[command(name = "oxydra-tui", about = "Oxydra interactive TUI client")]
struct Cli {
    /// WebSocket URL of the gateway (e.g. ws://127.0.0.1:9090/ws).
    /// When omitted, the runner discovery mechanism is used (not yet wired).
    #[arg(long)]
    gateway_endpoint: Option<String>,

    /// User identifier for the gateway session.
    #[arg(long, default_value = "default")]
    user: String,

    /// Join an existing session by ID instead of creating a new one.
    /// When omitted, a new session is created automatically.
    #[arg(long)]
    session: Option<String>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    let gateway_endpoint = match resolve_gateway_endpoint(&cli) {
        Ok(endpoint) => endpoint,
        Err(msg) => {
            eprintln!("error: {msg}");
            return ExitCode::FAILURE;
        }
    };

    let connection_id = Uuid::new_v4().to_string();

    let mut app = if let Some(session_id) = cli.session {
        TuiApp::with_session_id(&gateway_endpoint, &cli.user, &connection_id, session_id)
    } else {
        TuiApp::new(&gateway_endpoint, &cli.user, &connection_id)
    };

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: failed to build tokio runtime: {e}");
            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(app.run()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Resolve the gateway WebSocket endpoint from CLI arguments.
///
/// If `--gateway-endpoint` is provided, use it directly. Otherwise, runner-based
/// discovery will be used in a future step (currently returns an error).
fn resolve_gateway_endpoint(cli: &Cli) -> Result<String, String> {
    if let Some(endpoint) = &cli.gateway_endpoint {
        return Ok(endpoint.clone());
    }

    // Runner-based discovery is not yet wired (Step 7).
    // When available, this path will call the runner's connect_tui() or
    // equivalent to discover the gateway endpoint automatically.
    Err("--gateway-endpoint is required (runner-based discovery is not yet wired)".to_owned())
}
