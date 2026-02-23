use std::{path::PathBuf, process::ExitCode, time::Duration};

use clap::{Parser, Subcommand};
use runner::{
    GATEWAY_ENDPOINT_MARKER_FILE, Runner, RunnerControlTransportError, RunnerError,
    RunnerStartRequest, RunnerTuiConnectRequest, catalog::CatalogError,
};
use thiserror::Error;
use types::init_tracing;

const DEFAULT_RUNNER_CONFIG_PATH: &str = ".oxydra/runner.toml";
const TUI_BINARY_NAME: &str = "oxydra-tui";
const GATEWAY_ENDPOINT_WAIT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Subcommand, PartialEq, Eq)]
enum CliCommand {
    /// Model catalog governance commands
    Catalog {
        #[command(subcommand)]
        action: CatalogAction,
    },
}

#[derive(Debug, Clone, Subcommand, PartialEq, Eq)]
enum CatalogAction {
    /// Fetch model catalog and write to user cache
    Fetch {
        /// Fetch from the pinned snapshot URL instead of models.dev
        #[arg(long)]
        pinned: bool,
    },
    /// Verify that the resolved catalog (cached or pinned) has a valid schema
    Verify,
    /// Display summary of pinned catalog
    Show,
}

#[derive(Debug, Clone, Parser, PartialEq, Eq)]
#[command(name = "runner", about = "Oxydra runner control CLI")]
struct CliArgs {
    #[arg(short = 'c', long = "config", default_value = DEFAULT_RUNNER_CONFIG_PATH)]
    config_path: PathBuf,
    #[arg(short = 'u', long = "user")]
    user_id: Option<String>,
    #[arg(long = "insecure")]
    insecure: bool,
    #[arg(long = "tui")]
    tui: bool,
    /// Print gateway connection metadata and exit without launching the
    /// interactive TUI. Only meaningful with --tui.
    #[arg(long = "probe")]
    probe: bool,
    #[arg(long = "daemon")]
    daemon: bool,
    #[command(subcommand)]
    command: Option<CliCommand>,
}

#[derive(Debug, Error)]
enum CliError {
    #[error("{0}")]
    Arguments(String),
    #[error(transparent)]
    Runner(#[from] RunnerError),
    #[error(transparent)]
    ControlTransport(#[from] RunnerControlTransportError),
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    #[error(
        "`{binary}` was not found in PATH. \
         Install it with `cargo install --path crates/tui` or ensure it is on your PATH."
    )]
    TuiBinaryNotFound { binary: String },
    #[error("failed to launch `{binary}`: {source}")]
    TuiLaunchFailed {
        binary: String,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "timed out waiting for gateway endpoint marker at `{path}` after {timeout_secs}s; \
         the oxydra-vm process may not have started correctly"
    )]
    GatewayEndpointTimeout { path: PathBuf, timeout_secs: u64 },
}

fn main() -> ExitCode {
    if let Err(error) = run() {
        eprintln!("runner error: {error}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

fn run() -> Result<(), CliError> {
    init_tracing();
    let args = CliArgs::parse();

    // Catalog subcommands don't require a runner config or user id.
    if let Some(command) = args.command {
        return handle_command(command);
    }

    let runner = Runner::from_global_config_path(&args.config_path)?;
    let user_id = resolve_user_id(args.user_id, &runner)?;

    if args.tui {
        let connection = runner.connect_tui(RunnerTuiConnectRequest::new(&user_id))?;

        if args.probe {
            println!("mode=tui");
            println!("user_id={}", connection.user_id);
            println!("gateway_endpoint={}", connection.gateway_endpoint);
            println!("runtime_session_id={}", connection.runtime_session_id);
            println!("workspace_root={}", connection.workspace.root.display());
            return Ok(());
        }

        return launch_tui_binary(&connection.gateway_endpoint, &connection.user_id);
    }

    let mut startup = runner.start_user(RunnerStartRequest {
        user_id: user_id.clone(),
        insecure: args.insecure,
    })?;
    println!("mode=start");
    println!("user_id={}", startup.user_id);
    println!("sandbox_tier={:?}", startup.sandbox_tier);
    println!("workspace_root={}", startup.workspace.root.display());
    println!("shell_available={}", startup.shell_available);
    println!("browser_available={}", startup.browser_available);
    println!(
        "sidecar_available={}",
        startup.startup_status.sidecar_available
    );
    for reason in &startup.startup_status.degraded_reasons {
        println!("degraded_reason={:?}:{}", reason.code, reason.detail);
    }
    for warning in &startup.warnings {
        println!("warning={warning}");
    }
    tracing::info!(
        user_id = %startup.user_id,
        sandbox_tier = ?startup.sandbox_tier,
        shell_available = startup.shell_available,
        browser_available = startup.browser_available,
        "runner user session started"
    );

    // Poll for the gateway endpoint marker written by oxydra-vm and print it
    // so callers can discover the WebSocket URL without manual file inspection.
    match wait_for_gateway_endpoint(&startup.workspace.ipc, GATEWAY_ENDPOINT_WAIT_TIMEOUT) {
        Ok(gateway_endpoint) => {
            println!("gateway_endpoint={gateway_endpoint}");
            tracing::info!(%gateway_endpoint, user_id = %startup.user_id, "runner started guest");
        }
        Err(error) => {
            // Non-fatal: log the warning but continue — some tiers may not
            // expose a gateway endpoint immediately or at all.
            tracing::warn!(error = %error, "could not read gateway endpoint marker");
        }
    }

    if args.daemon {
        let control_socket_path = startup.workspace.ipc.join("runner-control.sock");
        let _ = std::fs::remove_file(&control_socket_path);

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|source| RunnerError::AsyncRuntimeInit { source })?;

        rt.block_on(async {
            let listener =
                tokio::net::UnixListener::bind(&control_socket_path).map_err(|source| {
                    RunnerError::GuestLifecycle {
                        action: "bind_control_socket",
                        role: runner::RunnerGuestRole::OxydraVm,
                        program: "runner".to_owned(),
                        source,
                    }
                })?;

            println!("control_socket={}", control_socket_path.display());
            tracing::info!(
                socket_path = %control_socket_path.display(),
                user_id = %startup.user_id,
                "runner control socket listening"
            );

            let shutdown_signal = tokio::signal::ctrl_c();
            tokio::pin!(shutdown_signal);

            tokio::select! {
                result = startup.serve_control_unix_listener(listener) => {
                    result?;
                }
                _ = &mut shutdown_signal => {
                    startup.shutdown()?;
                }
            }

            let _ = std::fs::remove_file(&control_socket_path);
            Ok::<(), CliError>(())
        })?;
    }

    Ok(())
}

fn handle_command(command: CliCommand) -> Result<(), CliError> {
    match command {
        CliCommand::Catalog { action } => handle_catalog_action(action),
    }
}

fn handle_catalog_action(action: CatalogAction) -> Result<(), CliError> {
    match action {
        CatalogAction::Fetch { pinned } => {
            runner::catalog::run_fetch(pinned, None)?;
            Ok(())
        }
        CatalogAction::Verify => {
            let valid = runner::catalog::run_verify()?;
            if valid {
                Ok(())
            } else {
                Err(CliError::Arguments(
                    "catalog verification failed: schema is invalid or catalog is empty".to_owned(),
                ))
            }
        }
        CatalogAction::Show => {
            runner::catalog::run_show()?;
            Ok(())
        }
    }
}

fn resolve_user_id(user_id: Option<String>, runner: &Runner) -> Result<String, CliError> {
    if let Some(user_id) = user_id {
        return Ok(user_id);
    }

    let configured_users: Vec<&str> = runner
        .global_config()
        .users
        .keys()
        .map(String::as_str)
        .collect();
    match configured_users.as_slice() {
        [only_user] => Ok((*only_user).to_owned()),
        [] => Err(CliError::Arguments(
            "no users are configured in runner config; add [users.<id>] or pass --user".to_owned(),
        )),
        _ => Err(CliError::Arguments(
            "multiple users configured; pass --user <user_id>".to_owned(),
        )),
    }
}

/// Locate the `oxydra-tui` binary in PATH and spawn it with the discovered
/// gateway endpoint and user id. The runner process waits for the child to
/// exit and forwards its exit status.
fn launch_tui_binary(gateway_endpoint: &str, user_id: &str) -> Result<(), CliError> {
    let binary_path = which_tui_binary()?;

    let mut child = std::process::Command::new(&binary_path)
        .arg("--gateway-endpoint")
        .arg(gateway_endpoint)
        .arg("--user")
        .arg(user_id)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .map_err(|source| CliError::TuiLaunchFailed {
            binary: binary_path.display().to_string(),
            source,
        })?;

    let status = child.wait().map_err(|source| CliError::TuiLaunchFailed {
        binary: binary_path.display().to_string(),
        source,
    })?;

    if status.success() {
        Ok(())
    } else {
        // Propagate the child's non-zero exit via a descriptive error.
        Err(CliError::Arguments(format!(
            "{TUI_BINARY_NAME} exited with {}",
            status
                .code()
                .map_or_else(|| "signal".to_owned(), |c| c.to_string()),
        )))
    }
}

/// Search PATH for the `oxydra-tui` binary. Returns an error with an
/// installation hint when the binary is not found.
fn which_tui_binary() -> Result<PathBuf, CliError> {
    // Check PATH entries manually to avoid pulling in an extra crate.
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join(TUI_BINARY_NAME);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }

    Err(CliError::TuiBinaryNotFound {
        binary: TUI_BINARY_NAME.to_owned(),
    })
}

/// Poll the `ipc/` directory for the gateway endpoint marker file written by
/// `oxydra-vm` after it binds the WebSocket listener. Returns the endpoint URL
/// once the marker is readable and non-empty, or an error if the timeout
/// elapses first.
fn wait_for_gateway_endpoint(
    ipc_dir: &std::path::Path,
    timeout: Duration,
) -> Result<String, CliError> {
    use std::time::Instant;
    let marker = ipc_dir.join(GATEWAY_ENDPOINT_MARKER_FILE);
    let started = Instant::now();
    loop {
        if let Ok(content) = std::fs::read_to_string(&marker) {
            let endpoint = content.trim().to_owned();
            if !endpoint.is_empty() {
                return Ok(endpoint);
            }
        }
        if started.elapsed() >= timeout {
            return Err(CliError::GatewayEndpointTimeout {
                path: marker,
                timeout_secs: timeout.as_secs(),
            });
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cli_args_defaults_to_start_mode() {
        let args = CliArgs::try_parse_from(["runner"]).expect("default args should parse");
        assert_eq!(args.config_path, PathBuf::from(DEFAULT_RUNNER_CONFIG_PATH));
        assert_eq!(args.user_id, None);
        assert!(!args.insecure);
        assert!(!args.tui);
        assert!(!args.probe);
        assert!(!args.daemon);
    }

    #[test]
    fn parse_cli_args_accepts_tui_and_user_flags() {
        let args = CliArgs::try_parse_from(["runner", "--tui", "--user", "alice"])
            .expect("args should parse");
        assert!(args.tui);
        assert!(!args.probe);
        assert_eq!(args.user_id.as_deref(), Some("alice"));
    }

    #[test]
    fn parse_cli_args_accepts_tui_with_probe_flag() {
        let args = CliArgs::try_parse_from(["runner", "--tui", "--probe", "--user", "alice"])
            .expect("tui+probe args should parse");
        assert!(args.tui);
        assert!(args.probe);
        assert_eq!(args.user_id.as_deref(), Some("alice"));
    }

    #[test]
    fn parse_cli_args_accepts_probe_without_tui() {
        let args =
            CliArgs::try_parse_from(["runner", "--probe"]).expect("standalone probe should parse");
        assert!(!args.tui);
        assert!(args.probe);
    }

    #[test]
    fn parse_cli_args_accepts_daemon_flag() {
        let args =
            CliArgs::try_parse_from(["runner", "--daemon"]).expect("daemon args should parse");
        assert!(args.daemon);
        assert!(!args.tui);
        assert!(!args.probe);
    }

    #[test]
    fn parse_cli_args_rejects_missing_flag_value() {
        assert!(
            CliArgs::try_parse_from(["runner", "--config"]).is_err(),
            "missing value should fail clap parsing"
        );
    }

    #[test]
    fn which_tui_binary_returns_error_when_not_in_path() {
        // Set PATH to an empty directory so the binary cannot be found.
        let empty_dir = std::env::temp_dir().join("oxydra-empty-path-test");
        let _ = std::fs::create_dir_all(&empty_dir);
        let saved_path = std::env::var_os("PATH");

        // SAFETY: test is single-threaded for this variable scope.
        unsafe { std::env::set_var("PATH", &empty_dir) };
        let result = which_tui_binary();
        if let Some(saved) = saved_path {
            unsafe { std::env::set_var("PATH", saved) };
        }

        assert!(result.is_err(), "should fail when binary is not in PATH");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains(TUI_BINARY_NAME),
            "error should mention the binary name: {err_msg}"
        );
        assert!(
            err_msg.contains("cargo install"),
            "error should suggest installation: {err_msg}"
        );

        let _ = std::fs::remove_dir_all(empty_dir);
    }

    #[test]
    fn tui_binary_not_found_error_message_includes_install_hint() {
        let err = CliError::TuiBinaryNotFound {
            binary: "oxydra-tui".to_owned(),
        };
        let msg = err.to_string();
        assert!(msg.contains("oxydra-tui"), "should mention binary name");
        assert!(
            msg.contains("cargo install"),
            "should include install suggestion"
        );
    }

    #[test]
    fn parse_cli_args_accepts_catalog_show_subcommand() {
        let args = CliArgs::try_parse_from(["runner", "catalog", "show"])
            .expect("catalog show should parse");
        assert_eq!(
            args.command,
            Some(CliCommand::Catalog {
                action: CatalogAction::Show
            })
        );
    }

    #[test]
    fn parse_cli_args_accepts_catalog_fetch_subcommand() {
        let args = CliArgs::try_parse_from(["runner", "catalog", "fetch"])
            .expect("catalog fetch should parse");
        assert_eq!(
            args.command,
            Some(CliCommand::Catalog {
                action: CatalogAction::Fetch { pinned: false }
            })
        );
    }

    #[test]
    fn parse_cli_args_accepts_catalog_fetch_with_pinned() {
        let args = CliArgs::try_parse_from(["runner", "catalog", "fetch", "--pinned"])
            .expect("catalog fetch --pinned should parse");
        assert_eq!(
            args.command,
            Some(CliCommand::Catalog {
                action: CatalogAction::Fetch { pinned: true }
            })
        );
    }

    #[test]
    fn parse_cli_args_accepts_catalog_verify_subcommand() {
        let args = CliArgs::try_parse_from(["runner", "catalog", "verify"])
            .expect("catalog verify should parse");
        assert_eq!(
            args.command,
            Some(CliCommand::Catalog {
                action: CatalogAction::Verify
            })
        );
    }

    #[test]
    fn parse_cli_args_without_subcommand_has_no_command() {
        let args = CliArgs::try_parse_from(["runner"]).expect("no subcommand should parse");
        assert_eq!(args.command, None);
    }
}
