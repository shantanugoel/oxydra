use std::{path::PathBuf, process::ExitCode};

use clap::Parser;
use runner::{Runner, RunnerError, RunnerStartRequest, RunnerTuiConnectRequest};
use thiserror::Error;

const DEFAULT_RUNNER_CONFIG_PATH: &str = ".oxydra/runner.toml";

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
}

#[derive(Debug, Error)]
enum CliError {
    #[error("{0}")]
    Arguments(String),
    #[error(transparent)]
    Runner(#[from] RunnerError),
}

fn main() -> ExitCode {
    if let Err(error) = run() {
        eprintln!("runner error: {error}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

fn run() -> Result<(), CliError> {
    let args = CliArgs::parse();

    let runner = Runner::from_global_config_path(&args.config_path)?;
    let user_id = resolve_user_id(args.user_id, &runner)?;

    if args.tui {
        let connection = runner.connect_tui(RunnerTuiConnectRequest::new(&user_id))?;
        println!("mode=tui");
        println!("user_id={}", connection.user_id);
        println!("gateway_endpoint={}", connection.gateway_endpoint);
        println!("runtime_session_id={}", connection.runtime_session_id);
        println!("workspace_root={}", connection.workspace.root.display());
        return Ok(());
    }

    let startup = runner.start_user(RunnerStartRequest {
        user_id: user_id.clone(),
        insecure: args.insecure,
    })?;
    println!("mode=start");
    println!("user_id={}", startup.user_id);
    println!("sandbox_tier={:?}", startup.sandbox_tier);
    println!("workspace_root={}", startup.workspace.root.display());
    println!("shell_available={}", startup.shell_available);
    println!("browser_available={}", startup.browser_available);
    for warning in startup.warnings {
        println!("warning={warning}");
    }

    Ok(())
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
    }

    #[test]
    fn parse_cli_args_accepts_tui_and_user_flags() {
        let args = CliArgs::try_parse_from(["runner", "--tui", "--user", "alice"])
            .expect("args should parse");
        assert!(args.tui);
        assert_eq!(args.user_id.as_deref(), Some("alice"));
    }

    #[test]
    fn parse_cli_args_rejects_missing_flag_value() {
        assert!(
            CliArgs::try_parse_from(["runner", "--config"]).is_err(),
            "missing value should fail clap parsing"
        );
    }
}
