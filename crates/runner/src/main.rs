use std::{
    path::{Path, PathBuf},
    process::ExitCode,
    time::Duration,
};

use clap::{Parser, Subcommand};
use runner::{
    GATEWAY_ENDPOINT_MARKER_FILE, Runner, RunnerControlTransportError, RunnerError,
    RunnerStartRequest, RunnerTuiConnectRequest, catalog::CatalogError, send_control_to_daemon,
    web,
};
use thiserror::Error;
use types::{
    LogFormat, LogRole, LogStream, RunnerControl, RunnerControlLogsRequest, RunnerControlResponse,
    init_tracing,
};

mod update_check;

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
    /// Start the runner daemon (launch sandbox + control socket)
    Start,
    /// Stop a running runner daemon
    Stop,
    /// Show the status of a running runner daemon
    Status,
    /// Restart the runner daemon (stop then start)
    Restart,
    /// Retrieve logs from a running runner daemon
    Logs {
        /// Which guest role to retrieve logs for (runtime, sidecar, all)
        #[arg(long, default_value = "runtime")]
        role: String,
        /// Which output stream to show (stdout, stderr, both)
        #[arg(long, default_value = "both")]
        stream: String,
        /// Number of log lines to retrieve (default 200, max 1000)
        #[arg(long, num_args = 0..=1, default_missing_value = "200")]
        tail: Option<usize>,
        /// Only show logs since this time (RFC 3339 timestamp or duration like 15m, 1h, 30s)
        #[arg(long)]
        since: Option<String>,
        /// Output format (text, json)
        #[arg(long, default_value = "text")]
        format: String,
        /// Continuously poll for new log entries (poll interval ~2s)
        #[arg(long, short = 'f')]
        follow: bool,
    },
    /// Check whether a newer Oxydra release is available on GitHub
    CheckUpdate {
        /// Include pre-release versions in the check
        #[arg(long)]
        pre_release: bool,
    },
    /// Start the web configurator dashboard
    Web {
        /// Override the bind address (e.g. 127.0.0.1:9400)
        #[arg(long)]
        bind: Option<String>,
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
#[command(
    name = "runner",
    about = "Oxydra runner control CLI",
    version,
    long_version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("OXYDRA_GIT_HASH"), ")")
)]
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
    /// Inject an environment variable into the guest container (KEY=VALUE).
    /// Can be repeated. Only effective for Container and MicroVM tiers.
    #[arg(short = 'e', long = "env")]
    env_vars: Vec<String>,
    /// Read environment variables from a file (one KEY=VALUE per line).
    /// Lines starting with '#' and blank lines are ignored.
    /// Only effective for Container and MicroVM tiers.
    #[arg(long = "env-file")]
    env_file: Option<PathBuf>,
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
    #[error("invalid --env value `{value}`: expected KEY=VALUE format")]
    InvalidEnvVar { value: String },
    #[error("web server error: {0}")]
    WebServer(#[from] runner::web::WebServerError),
    #[error("failed to read --env-file `{path}`: {source}")]
    EnvFileRead {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("no running server found for user `{user_id}`; start one with `runner start`")]
    ServerNotRunning { user_id: String },
    #[error(
        "timed out waiting for server to stop for user `{user_id}`; \
         the control socket at `{socket_path}` still exists after {timeout_secs}s"
    )]
    ServerStopTimeout {
        user_id: String,
        socket_path: PathBuf,
        timeout_secs: u64,
    },
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

    // Subcommands that don't require a running user session.
    match &args.command {
        Some(CliCommand::Catalog { action }) => return handle_catalog_action(action.clone()),
        Some(CliCommand::Start) => return handle_lifecycle(LifecycleAction::Start, &args),
        Some(CliCommand::Stop) => return handle_lifecycle(LifecycleAction::Stop, &args),
        Some(CliCommand::Status) => return handle_lifecycle(LifecycleAction::Status, &args),
        Some(CliCommand::Restart) => return handle_lifecycle(LifecycleAction::Restart, &args),
        Some(CliCommand::Logs {
            role,
            stream,
            tail,
            since,
            format,
            follow,
        }) => {
            return handle_logs(
                &args,
                role.clone(),
                stream.clone(),
                *tail,
                since.clone(),
                format.clone(),
                *follow,
            );
        }
        Some(CliCommand::CheckUpdate { pre_release }) => {
            return handle_check_update(*pre_release, &args.config_path);
        }
        Some(CliCommand::Web { bind }) => {
            return handle_web(&args.config_path, bind.clone());
        }
        None => {}
    }

    let runner = Runner::from_global_config_path(&args.config_path)?;
    let user_id = resolve_user_id(args.user_id, &runner)?;

    if args.tui {
        let connection = runner.connect_tui(RunnerTuiConnectRequest::new(&user_id))?;

        if args.probe {
            println!("mode=tui");
            println!("user_id={}", connection.user_id);
            println!("gateway_endpoint={}", connection.gateway_endpoint);
            println!("session_id={}", connection.session_id);
            println!("workspace_root={}", connection.workspace.root.display());
            return Ok(());
        }

        return launch_tui_binary(&connection.gateway_endpoint, &connection.user_id);
    }

    let extra_env = parse_extra_env_vars(&args.env_vars, args.env_file.as_deref())?;
    let mut startup = runner.start_user(RunnerStartRequest {
        user_id: user_id.clone(),
        insecure: args.insecure,
        extra_env,
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
        run_daemon(&mut startup)?;
    }

    Ok(())
}

fn handle_web(config_path: &Path, bind_override: Option<String>) -> Result<(), CliError> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|source| RunnerError::AsyncRuntimeInit { source })?;
    rt.block_on(async {
        web::run_web_server(config_path, bind_override)
            .await
            .map_err(CliError::from)
    })
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

fn handle_check_update(include_prerelease: bool, config_path: &Path) -> Result<(), CliError> {
    // Always fetch live data for an explicit check-update (use_cache = false).
    match update_check::run_check(false, include_prerelease) {
        Some(outcome) => {
            println!("Current version: {}", outcome.current_version);
            println!("Latest version:  {}", outcome.latest_version);
            if outcome.update_available {
                println!(
                    "Update available! Run: install-release.sh --tag v{}",
                    outcome.latest_version
                );
            } else {
                println!("You are up to date.");
            }
        }
        None => {
            eprintln!(
                "warning: could not reach GitHub to check for updates \
                 (network unavailable or rate limited)"
            );
        }
    }

    // Binary-vs-config image tag mismatch check (best-effort; load config
    // only if it exists so the command works without a runner.toml too).
    if config_path.exists()
        && let Ok(runner) = Runner::from_global_config_path(config_path)
    {
        emit_image_tag_mismatch_warnings(&runner);
    }

    Ok(())
}

fn format_image_tag_mismatch_warning(mismatch: &update_check::ImageTagMismatch) -> String {
    format!(
        "[oxydra] Warning: Binary version ({}) does not match guest image tag ({}) \
             in runner.toml field `{}` (image: {}).\n\
[oxydra]   Update [guest_images] tags in .oxydra/runner.toml or re-run install-release.sh.",
        mismatch.binary_version, mismatch.image_tag, mismatch.field, mismatch.image_ref,
    )
}

fn emit_image_tag_mismatch_warnings(runner: &Runner) {
    for mismatch in update_check::check_image_tag_mismatches(&runner.global_config().guest_images) {
        eprintln!("{}", format_image_tag_mismatch_warning(&mismatch));
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

// ---------------------------------------------------------------------------
//  Lifecycle subcommand handlers (start / stop / status / restart)
// ---------------------------------------------------------------------------

/// Internal enum to dispatch lifecycle actions without duplicating the
/// `CliCommand` variant names in function signatures.
enum LifecycleAction {
    Start,
    Stop,
    Status,
    Restart,
}

fn handle_lifecycle(action: LifecycleAction, args: &CliArgs) -> Result<(), CliError> {
    let runner = Runner::from_global_config_path(&args.config_path)?;
    let user_id = resolve_user_id(args.user_id.clone(), &runner)?;

    match action {
        LifecycleAction::Start => server_start(&runner, &user_id, args),
        LifecycleAction::Stop => server_stop(&runner, &user_id),
        LifecycleAction::Status => server_status(&runner, &user_id),
        LifecycleAction::Restart => server_restart(&runner, &user_id, args),
    }
}

fn server_start(runner: &Runner, user_id: &str, args: &CliArgs) -> Result<(), CliError> {
    emit_image_tag_mismatch_warnings(runner);

    // Non-blocking update notice: use cached result so startup is never delayed.
    // A separate thread performs the check to avoid blocking the main path.
    std::thread::spawn(|| {
        if let Some(outcome) = update_check::run_check(true, false)
            && outcome.update_available
        {
            eprintln!(
                "[oxydra] Update available: v{} (current: v{}). \
                     Run `runner check-update` for details.",
                outcome.latest_version, outcome.current_version,
            );
        }
    });

    let extra_env = parse_extra_env_vars(&args.env_vars, args.env_file.as_deref())?;
    let mut startup = runner.start_user(RunnerStartRequest {
        user_id: user_id.to_owned(),
        insecure: args.insecure,
        extra_env,
    })?;

    print_startup_info(&startup);

    match wait_for_gateway_endpoint(&startup.workspace.ipc, GATEWAY_ENDPOINT_WAIT_TIMEOUT) {
        Ok(gateway_endpoint) => {
            println!("gateway_endpoint={gateway_endpoint}");
            tracing::info!(
                %gateway_endpoint,
                user_id = %startup.user_id,
                "runner started guest"
            );
        }
        Err(error) => {
            tracing::warn!(error = %error, "could not read gateway endpoint marker");
        }
    }

    run_daemon(&mut startup)
}

fn server_stop(runner: &Runner, user_id: &str) -> Result<(), CliError> {
    let workspace = runner.provision_user_workspace(user_id)?;
    let socket_path = workspace.control_socket_path();

    if !socket_path.exists() {
        return Err(CliError::ServerNotRunning {
            user_id: user_id.to_owned(),
        });
    }

    let response = send_control_to_daemon(
        &socket_path,
        &RunnerControl::ShutdownUser {
            user_id: user_id.to_owned(),
        },
    )
    .map_err(|source| {
        if source.is_connection_refused() {
            CliError::ServerNotRunning {
                user_id: user_id.to_owned(),
            }
        } else {
            CliError::ControlTransport(source)
        }
    })?;

    match response {
        RunnerControlResponse::ShutdownStatus(status) => {
            println!("user_id={}", status.user_id);
            println!("shutdown={}", status.shutdown);
            if status.already_stopped {
                println!("already_stopped=true");
            }
            if let Some(ref message) = status.message {
                println!("message={message}");
            }
            Ok(())
        }
        RunnerControlResponse::Error(error) => Err(CliError::Arguments(format!(
            "server reported error: {}",
            error.message
        ))),
        _ => Err(CliError::Arguments(
            "unexpected response to shutdown request".to_owned(),
        )),
    }
}

fn server_status(runner: &Runner, user_id: &str) -> Result<(), CliError> {
    let workspace = runner.provision_user_workspace(user_id)?;
    let socket_path = workspace.control_socket_path();

    if !socket_path.exists() {
        return Err(CliError::ServerNotRunning {
            user_id: user_id.to_owned(),
        });
    }

    let response =
        send_control_to_daemon(&socket_path, &RunnerControl::HealthCheck).map_err(|source| {
            if source.is_connection_refused() {
                CliError::ServerNotRunning {
                    user_id: user_id.to_owned(),
                }
            } else {
                CliError::ControlTransport(source)
            }
        })?;

    match response {
        RunnerControlResponse::HealthStatus(status) => {
            println!("user_id={}", status.user_id);
            println!("healthy={}", status.healthy);
            println!("sandbox_tier={:?}", status.sandbox_tier);
            println!("shell_available={}", status.shell_available);
            println!("browser_available={}", status.browser_available);
            println!(
                "sidecar_available={}",
                status.startup_status.sidecar_available
            );
            println!("shutdown={}", status.shutdown);
            if let Some(ref message) = status.message {
                println!("message={message}");
            }
            if let Some(ref log_dir) = status.log_dir {
                println!("log_dir={log_dir}");
            }
            if let Some(pid) = status.runtime_pid {
                println!("runtime_pid={pid}");
            }
            if let Some(ref container) = status.runtime_container_name {
                println!("runtime_container_name={container}");
            }
            for reason in &status.startup_status.degraded_reasons {
                println!("degraded_reason={:?}:{}", reason.code, reason.detail);
            }
            Ok(())
        }
        RunnerControlResponse::Error(error) => Err(CliError::Arguments(format!(
            "server reported error: {}",
            error.message
        ))),
        _ => Err(CliError::Arguments(
            "unexpected response to health check request".to_owned(),
        )),
    }
}

fn server_restart(runner: &Runner, user_id: &str, args: &CliArgs) -> Result<(), CliError> {
    let workspace = runner.provision_user_workspace(user_id)?;
    let socket_path = workspace.control_socket_path();

    if socket_path.exists() {
        match send_control_to_daemon(
            &socket_path,
            &RunnerControl::ShutdownUser {
                user_id: user_id.to_owned(),
            },
        ) {
            Ok(RunnerControlResponse::ShutdownStatus(status)) => {
                if status.shutdown {
                    println!("stopped previous server for user `{}`", status.user_id);
                }
            }
            Ok(RunnerControlResponse::Error(error)) => {
                eprintln!(
                    "warning: shutdown reported error: {}; proceeding with restart",
                    error.message
                );
            }
            Err(error) => {
                if !error.is_connection_refused() {
                    eprintln!(
                        "warning: could not connect to running server: {error}; \
                         proceeding with restart"
                    );
                }
            }
            _ => {}
        }

        wait_for_socket_removal(&socket_path, user_id)?;
    }

    server_start(runner, user_id, args)
}

fn handle_logs(
    args: &CliArgs,
    role: String,
    stream: String,
    tail: Option<usize>,
    since: Option<String>,
    format: String,
    follow: bool,
) -> Result<(), CliError> {
    let runner = Runner::from_global_config_path(&args.config_path)?;
    let user_id = resolve_user_id(args.user_id.clone(), &runner)?;

    let log_role = match role.as_str() {
        "runtime" => LogRole::Runtime,
        "sidecar" => LogRole::Sidecar,
        "all" => LogRole::All,
        other => {
            return Err(CliError::Arguments(format!(
                "invalid --role value `{other}`; expected runtime, sidecar, or all"
            )));
        }
    };

    let log_stream = match stream.as_str() {
        "stdout" => LogStream::Stdout,
        "stderr" => LogStream::Stderr,
        "both" => LogStream::Both,
        other => {
            return Err(CliError::Arguments(format!(
                "invalid --stream value `{other}`; expected stdout, stderr, or both"
            )));
        }
    };

    let log_format = match format.as_str() {
        "text" => LogFormat::Text,
        "json" => LogFormat::Json,
        other => {
            return Err(CliError::Arguments(format!(
                "invalid --format value `{other}`; expected text or json"
            )));
        }
    };

    let request = RunnerControlLogsRequest {
        role: log_role,
        stream: log_stream,
        tail,
        since,
        format: log_format,
    };

    if follow {
        server_logs_follow(&runner, &user_id, request)
    } else {
        server_logs(&runner, &user_id, request)
    }
}

fn server_logs(
    runner: &Runner,
    user_id: &str,
    request: RunnerControlLogsRequest,
) -> Result<(), CliError> {
    let workspace = runner.provision_user_workspace(user_id)?;
    let socket_path = workspace.control_socket_path();

    if !socket_path.exists() {
        return Err(CliError::ServerNotRunning {
            user_id: user_id.to_owned(),
        });
    }

    let format = request.format;

    let response =
        send_control_to_daemon(&socket_path, &RunnerControl::Logs(request)).map_err(|source| {
            if source.is_connection_refused() {
                CliError::ServerNotRunning {
                    user_id: user_id.to_owned(),
                }
            } else {
                CliError::ControlTransport(source)
            }
        })?;

    match response {
        RunnerControlResponse::Logs(logs) => {
            for warning in &logs.warnings {
                eprintln!("warning: {warning}");
            }

            for entry in &logs.entries {
                match format {
                    LogFormat::Text => println!("{}", entry.to_text_line()),
                    LogFormat::Json => {
                        if let Ok(json) = serde_json::to_string(entry) {
                            println!("{json}");
                        }
                    }
                }
            }

            if logs.truncated {
                eprintln!("(output truncated; use --tail to adjust)");
            }

            Ok(())
        }
        RunnerControlResponse::Error(error) => Err(CliError::Arguments(format!(
            "server reported error: {}",
            error.message
        ))),
        _ => Err(CliError::Arguments(
            "unexpected response to logs request".to_owned(),
        )),
    }
}

/// Follow mode: polls the daemon for log snapshots every ~2s, printing only
/// new entries. De-duplicates by tracking a cursor key derived from each
/// entry's content. Runs until interrupted (Ctrl+C) or connection loss.
fn server_logs_follow(
    runner: &Runner,
    user_id: &str,
    request: RunnerControlLogsRequest,
) -> Result<(), CliError> {
    use std::collections::HashSet;

    let workspace = runner.provision_user_workspace(user_id)?;
    let socket_path = workspace.control_socket_path();

    if !socket_path.exists() {
        return Err(CliError::ServerNotRunning {
            user_id: user_id.to_owned(),
        });
    }

    let format = request.format;
    let mut seen: HashSet<u64> = HashSet::new();

    // Print the initial snapshot (respecting tail), then switch to polling
    // for new entries only.
    let initial_response =
        send_control_to_daemon(&socket_path, &RunnerControl::Logs(request.clone())).map_err(
            |source| {
                if source.is_connection_refused() {
                    CliError::ServerNotRunning {
                        user_id: user_id.to_owned(),
                    }
                } else {
                    CliError::ControlTransport(source)
                }
            },
        )?;

    if let RunnerControlResponse::Logs(logs) = initial_response {
        for warning in &logs.warnings {
            eprintln!("warning: {warning}");
        }
        for entry in &logs.entries {
            let key = entry_hash(entry);
            seen.insert(key);
            print_log_entry(entry, format);
        }
    }

    // Poll loop: request the most recent entries and print only unseen ones.
    // Use a fixed tail of 100 per poll to keep requests small.
    let poll_request = RunnerControlLogsRequest {
        tail: Some(100),
        since: None, // rely on dedup, not timestamp filtering
        ..request
    };

    loop {
        std::thread::sleep(Duration::from_secs(2));

        if !socket_path.exists() {
            eprintln!("(daemon exited)");
            break;
        }

        let response = match send_control_to_daemon(
            &socket_path,
            &RunnerControl::Logs(poll_request.clone()),
        ) {
            Ok(r) => r,
            Err(source) => {
                if source.is_connection_refused() {
                    eprintln!("(daemon exited)");
                    break;
                }
                eprintln!("warning: poll failed: {source}");
                continue;
            }
        };

        if let RunnerControlResponse::Logs(logs) = response {
            for entry in &logs.entries {
                let key = entry_hash(entry);
                if seen.insert(key) {
                    print_log_entry(entry, format);
                }
            }
        }

        // Prevent the seen set from growing without bound — if it gets large,
        // keep only the most recent hashes by rebuilding from the last batch.
        if seen.len() > 10_000 {
            seen.clear();
        }
    }

    Ok(())
}

fn print_log_entry(entry: &types::RunnerLogEntry, format: LogFormat) {
    match format {
        LogFormat::Text => println!("{}", entry.to_text_line()),
        LogFormat::Json => {
            if let Ok(json) = serde_json::to_string(entry) {
                println!("{json}");
            }
        }
    }
}

/// Produce a hash key for deduplication. Uses a simple FNV-1a-style hash of
/// all entry fields so we don't need to pull in a hashing crate.
fn entry_hash(entry: &types::RunnerLogEntry) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    entry.timestamp.hash(&mut hasher);
    entry.role.hash(&mut hasher);
    entry.stream.hash(&mut hasher);
    entry.message.hash(&mut hasher);
    hasher.finish()
}

/// Runs the daemon loop: binds a control socket and serves health-check and
/// shutdown requests until the user session is shut down or Ctrl+C is received.
fn run_daemon(startup: &mut runner::RunnerStartup) -> Result<(), CliError> {
    let control_socket_path = startup.workspace.control_socket_path();
    let _ = std::fs::remove_file(&control_socket_path);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|source| RunnerError::AsyncRuntimeInit { source })?;

    rt.block_on(async {
        let listener = tokio::net::UnixListener::bind(&control_socket_path).map_err(|source| {
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
                startup.shutdown_async().await?;
            }
        }

        let _ = std::fs::remove_file(&control_socket_path);
        Ok::<(), CliError>(())
    })?;

    Ok(())
}

fn print_startup_info(startup: &runner::RunnerStartup) {
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
}

/// Waits for the control socket file to be removed by a shutting-down daemon,
/// with a timeout. If the socket still exists after the timeout, removes it
/// forcefully so a new daemon can bind.
fn wait_for_socket_removal(socket_path: &std::path::Path, user_id: &str) -> Result<(), CliError> {
    const SOCKET_REMOVAL_TIMEOUT: Duration = Duration::from_secs(10);
    let started = std::time::Instant::now();
    while socket_path.exists() {
        if started.elapsed() >= SOCKET_REMOVAL_TIMEOUT {
            eprintln!(
                "warning: control socket still exists after {}s; removing it",
                SOCKET_REMOVAL_TIMEOUT.as_secs()
            );
            let _ = std::fs::remove_file(socket_path);
            if socket_path.exists() {
                return Err(CliError::ServerStopTimeout {
                    user_id: user_id.to_owned(),
                    socket_path: socket_path.to_path_buf(),
                    timeout_secs: SOCKET_REMOVAL_TIMEOUT.as_secs(),
                });
            }
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(())
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

/// Parse `--env KEY=VALUE` arguments and `--env-file` contents into a
/// deduplicated list of `KEY=VALUE` strings. CLI `-e` entries take precedence
/// over file entries when the same key appears in both.
fn parse_extra_env_vars(
    cli_env: &[String],
    env_file: Option<&Path>,
) -> Result<Vec<String>, CliError> {
    let mut seen: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();

    // Read env-file first (lower precedence).
    if let Some(path) = env_file {
        let content = std::fs::read_to_string(path).map_err(|source| CliError::EnvFileRead {
            path: path.to_path_buf(),
            source,
        })?;
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let (key, value) = parse_env_entry(trimmed)?;
            seen.insert(key, value);
        }
    }

    // CLI --env entries override file entries.
    for entry in cli_env {
        let (key, value) = parse_env_entry(entry)?;
        seen.insert(key, value);
    }

    Ok(seen.into_iter().map(|(k, v)| format!("{k}={v}")).collect())
}

/// Parse a single `KEY=VALUE` string, returning `(key, value)`.
fn parse_env_entry(entry: &str) -> Result<(String, String), CliError> {
    match entry.split_once('=') {
        Some((key, value)) if !key.is_empty() => Ok((key.to_owned(), value.to_owned())),
        _ => Err(CliError::InvalidEnvVar {
            value: entry.to_owned(),
        }),
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

    #[test]
    fn parse_cli_args_accepts_env_flags() {
        let args = CliArgs::try_parse_from([
            "runner",
            "-e",
            "FOO=bar",
            "-e",
            "BAZ=qux",
            "--env-file",
            "/tmp/env",
        ])
        .expect("env args should parse");
        assert_eq!(args.env_vars, vec!["FOO=bar", "BAZ=qux"]);
        assert_eq!(args.env_file, Some(PathBuf::from("/tmp/env")));
    }

    #[test]
    fn parse_extra_env_vars_from_cli_only() {
        let vars = vec!["KEY1=val1".to_owned(), "KEY2=val2".to_owned()];
        let result = parse_extra_env_vars(&vars, None).expect("should parse CLI env vars");
        assert_eq!(result, vec!["KEY1=val1", "KEY2=val2"]);
    }

    #[test]
    fn parse_extra_env_vars_from_file() {
        let dir = std::env::temp_dir().join("oxydra-env-file-test");
        let _ = std::fs::create_dir_all(&dir);
        let file_path = dir.join("test.env");
        std::fs::write(&file_path, "# comment\nFROM_FILE=hello\n\nANOTHER=world\n")
            .expect("env file should be writable");

        let result = parse_extra_env_vars(&[], Some(&file_path)).expect("should parse env file");
        assert_eq!(result, vec!["ANOTHER=world", "FROM_FILE=hello"]);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn parse_extra_env_vars_cli_overrides_file() {
        let dir = std::env::temp_dir().join("oxydra-env-override-test");
        let _ = std::fs::create_dir_all(&dir);
        let file_path = dir.join("test.env");
        std::fs::write(&file_path, "KEY=from_file\n").expect("env file should be writable");

        let cli = vec!["KEY=from_cli".to_owned()];
        let result =
            parse_extra_env_vars(&cli, Some(&file_path)).expect("should merge env sources");
        assert_eq!(result, vec!["KEY=from_cli"]);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn parse_extra_env_vars_rejects_missing_equals() {
        let vars = vec!["INVALID".to_owned()];
        let err = parse_extra_env_vars(&vars, None).expect_err("should reject missing =");
        assert!(err.to_string().contains("INVALID"));
    }

    #[test]
    fn parse_extra_env_vars_allows_empty_value() {
        let vars = vec!["KEY=".to_owned()];
        let result = parse_extra_env_vars(&vars, None).expect("empty value should be allowed");
        assert_eq!(result, vec!["KEY="]);
    }

    #[test]
    fn parse_extra_env_vars_allows_value_with_equals() {
        let vars = vec!["KEY=val=ue".to_owned()];
        let result = parse_extra_env_vars(&vars, None).expect("value with = should be allowed");
        assert_eq!(result, vec!["KEY=val=ue"]);
    }

    #[test]
    fn parse_cli_args_accepts_start_subcommand() {
        let args = CliArgs::try_parse_from(["runner", "start"]).expect("start should parse");
        assert_eq!(args.command, Some(CliCommand::Start));
    }

    #[test]
    fn parse_cli_args_accepts_stop_subcommand() {
        let args = CliArgs::try_parse_from(["runner", "stop"]).expect("stop should parse");
        assert_eq!(args.command, Some(CliCommand::Stop));
    }

    #[test]
    fn parse_cli_args_accepts_status_subcommand() {
        let args = CliArgs::try_parse_from(["runner", "status"]).expect("status should parse");
        assert_eq!(args.command, Some(CliCommand::Status));
    }

    #[test]
    fn parse_cli_args_accepts_restart_subcommand() {
        let args = CliArgs::try_parse_from(["runner", "restart"]).expect("restart should parse");
        assert_eq!(args.command, Some(CliCommand::Restart));
    }

    #[test]
    fn parse_cli_args_accepts_check_update_subcommand() {
        let args =
            CliArgs::try_parse_from(["runner", "check-update"]).expect("check-update should parse");
        assert_eq!(
            args.command,
            Some(CliCommand::CheckUpdate { pre_release: false })
        );
    }

    #[test]
    fn parse_cli_args_accepts_check_update_with_pre_release() {
        let args = CliArgs::try_parse_from(["runner", "check-update", "--pre-release"])
            .expect("check-update --pre-release should parse");
        assert_eq!(
            args.command,
            Some(CliCommand::CheckUpdate { pre_release: true })
        );
    }

    #[test]
    fn parse_cli_args_stop_with_user_flag() {
        let args = CliArgs::try_parse_from(["runner", "--user", "alice", "stop"])
            .expect("stop with --user should parse");
        assert_eq!(args.user_id.as_deref(), Some("alice"));
        assert_eq!(args.command, Some(CliCommand::Stop));
    }

    #[test]
    fn parse_cli_args_start_with_env_flags() {
        let args = CliArgs::try_parse_from(["runner", "-e", "MY_KEY=val", "--insecure", "start"])
            .expect("start with env flags should parse");
        assert_eq!(args.command, Some(CliCommand::Start));
        assert_eq!(args.env_vars, vec!["MY_KEY=val"]);
        assert!(args.insecure);
    }

    #[test]
    fn image_tag_mismatch_warning_mentions_fix_path() {
        let warning = format_image_tag_mismatch_warning(&update_check::ImageTagMismatch {
            field: "guest_images.oxydra_vm",
            image_ref: "ghcr.io/shantanugoel/oxydra-vm:v0.1.4".to_owned(),
            image_tag: "v0.1.4".to_owned(),
            binary_version: "0.2.0".to_owned(),
        });
        assert!(
            warning.contains("re-run install-release.sh"),
            "warning should suggest reinstall path: {warning}"
        );
        assert!(
            warning.contains(".oxydra/runner.toml"),
            "warning should mention runner.toml path: {warning}"
        );
    }

    #[test]
    fn server_not_running_error_mentions_user_id_and_hint() {
        let err = CliError::ServerNotRunning {
            user_id: "alice".to_owned(),
        };
        let msg = err.to_string();
        assert!(msg.contains("alice"), "error should mention user id: {msg}");
        assert!(
            msg.contains("runner start"),
            "error should suggest `runner start`: {msg}"
        );
    }

    #[test]
    fn parse_cli_args_accepts_logs_subcommand_with_defaults() {
        let args = CliArgs::try_parse_from(["runner", "logs"]).expect("logs should parse");
        match args.command {
            Some(CliCommand::Logs {
                role,
                stream,
                tail,
                since,
                format,
                follow,
            }) => {
                assert_eq!(role, "runtime");
                assert_eq!(stream, "both");
                assert_eq!(tail, None);
                assert!(since.is_none());
                assert_eq!(format, "text");
                assert!(!follow);
            }
            other => panic!("expected Logs subcommand, got {other:?}"),
        }
    }

    #[test]
    fn parse_cli_args_accepts_logs_with_all_flags() {
        let args = CliArgs::try_parse_from([
            "runner", "logs", "--role", "all", "--stream", "stderr", "--tail", "50", "--since",
            "15m", "--format", "json",
        ])
        .expect("logs with all flags should parse");
        match args.command {
            Some(CliCommand::Logs {
                role,
                stream,
                tail,
                since,
                format,
                follow,
            }) => {
                assert_eq!(role, "all");
                assert_eq!(stream, "stderr");
                assert_eq!(tail, Some(50));
                assert_eq!(since, Some("15m".to_owned()));
                assert_eq!(format, "json");
                assert!(!follow);
            }
            other => panic!("expected Logs subcommand, got {other:?}"),
        }
    }

    #[test]
    fn parse_cli_args_accepts_logs_with_user_flag() {
        let args = CliArgs::try_parse_from(["runner", "--user", "alice", "logs"])
            .expect("logs with --user should parse");
        assert_eq!(args.user_id.as_deref(), Some("alice"));
        assert!(matches!(args.command, Some(CliCommand::Logs { .. })));
    }
}
