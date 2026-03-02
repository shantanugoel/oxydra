use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{Path as AxumPath, State, rejection::JsonRejection};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use types::{
    RunnerControl, RunnerControlHealthStatus, RunnerControlResponse, RunnerControlShutdownStatus,
};

use super::response::{ApiError, ErrorResponse, ok_response};
use super::state::WebState;
use crate::{GATEWAY_ENDPOINT_MARKER_FILE, send_control_to_daemon_async};

const START_TIMEOUT: Duration = Duration::from_secs(30);
const STOP_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(200);

#[derive(Debug, Deserialize, Default)]
pub struct StartUserRequest {
    #[serde(default)]
    pub insecure: bool,
}

#[derive(Debug, Serialize)]
struct StartUserResponse {
    user_id: String,
    daemon_pid: Option<u32>,
    gateway_endpoint: String,
    status: RunnerControlHealthStatus,
}

#[derive(Debug, Serialize)]
struct StopUserResponse {
    status: RunnerControlShutdownStatus,
}

#[derive(Debug, Serialize)]
struct RestartUserResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    stop_status: Option<RunnerControlShutdownStatus>,
    start: StartUserResponse,
}

/// `POST /api/v1/control/{user_id}/start` — spawn a detached daemon process.
pub async fn start_user_daemon(
    State(state): State<Arc<WebState>>,
    AxumPath(user_id): AxumPath<String>,
    payload: Result<Json<StartUserRequest>, JsonRejection>,
) -> impl IntoResponse {
    let start_request = match parse_start_payload(payload) {
        Ok(request) => request,
        Err(error) => return error.into_response(),
    };
    let user_id = match ensure_registered_user(&state, &user_id) {
        Ok(user_id) => user_id,
        Err(error) => return error.into_response(),
    };

    match start_user_daemon_inner(&state, &user_id, start_request.insecure).await {
        Ok(response) => ok_response(response).into_response(),
        Err(error) => error.into_response(),
    }
}

/// `POST /api/v1/control/{user_id}/stop` — stop a running daemon via control socket.
pub async fn stop_user_daemon(
    State(state): State<Arc<WebState>>,
    AxumPath(user_id): AxumPath<String>,
) -> impl IntoResponse {
    let user_id = match ensure_registered_user(&state, &user_id) {
        Ok(user_id) => user_id,
        Err(error) => return error.into_response(),
    };

    match stop_user_daemon_inner(&state, &user_id).await {
        Ok(status) => ok_response(StopUserResponse { status }).into_response(),
        Err(error) => error.into_response(),
    }
}

/// `POST /api/v1/control/{user_id}/restart` — stop current daemon (if running), then start.
pub async fn restart_user_daemon(
    State(state): State<Arc<WebState>>,
    AxumPath(user_id): AxumPath<String>,
    payload: Result<Json<StartUserRequest>, JsonRejection>,
) -> impl IntoResponse {
    let restart_request = match parse_start_payload(payload) {
        Ok(request) => request,
        Err(error) => return error.into_response(),
    };
    let user_id = match ensure_registered_user(&state, &user_id) {
        Ok(user_id) => user_id,
        Err(error) => return error.into_response(),
    };

    let socket_path = state.control_socket_path(&user_id);
    let stop_status = match probe_health_status(&socket_path).await {
        Ok(Some(_)) => match stop_user_daemon_inner(&state, &user_id).await {
            Ok(status) => Some(status),
            Err(error) => return error.into_response(),
        },
        Ok(None) => {
            if socket_path.exists() {
                cleanup_stale_socket(&socket_path);
            }
            None
        }
        Err(error) => return error.into_response(),
    };

    match start_user_daemon_inner(&state, &user_id, restart_request.insecure).await {
        Ok(start) => ok_response(RestartUserResponse { stop_status, start }).into_response(),
        Err(error) => error.into_response(),
    }
}

async fn start_user_daemon_inner(
    state: &WebState,
    user_id: &str,
    insecure: bool,
) -> Result<StartUserResponse, ErrorResponse> {
    let socket_path = state.control_socket_path(user_id);
    if probe_health_status(&socket_path).await?.is_some() {
        return Err(daemon_already_running(format!(
            "daemon for user `{user_id}` is already running"
        )));
    }

    if socket_path.exists() {
        cleanup_stale_socket(&socket_path);
    }

    let daemon_pid = spawn_detached_daemon(state, user_id, insecure)?;
    if let Some(pid) = daemon_pid {
        state.record_spawned_daemon_pid(user_id, pid);
    }

    let status = wait_for_daemon_health(&socket_path, user_id).await?;
    let marker_path = state
        .workspace_root
        .join(user_id)
        .join("tmp")
        .join(GATEWAY_ENDPOINT_MARKER_FILE);
    let gateway_endpoint = wait_for_gateway_endpoint(&marker_path, user_id).await?;

    Ok(StartUserResponse {
        user_id: user_id.to_owned(),
        daemon_pid,
        gateway_endpoint,
        status,
    })
}

async fn stop_user_daemon_inner(
    state: &WebState,
    user_id: &str,
) -> Result<RunnerControlShutdownStatus, ErrorResponse> {
    let socket_path = state.control_socket_path(user_id);
    if !socket_path.exists() {
        return Err(daemon_not_running(format!(
            "daemon for user `{user_id}` is not running"
        )));
    }

    let response = send_control_to_daemon_async(
        &socket_path,
        &RunnerControl::ShutdownUser {
            user_id: user_id.to_owned(),
        },
    )
    .await
    .map_err(|error| {
        if error.is_connection_refused() {
            daemon_not_running(format!("daemon for user `{user_id}` is not running"))
        } else {
            daemon_command_failed(format!(
                "failed to send shutdown request for user `{user_id}`: {error}"
            ))
        }
    })?;

    match response {
        RunnerControlResponse::ShutdownStatus(status) => {
            wait_for_socket_removal(&socket_path, user_id).await?;
            let _ = state.remove_spawned_daemon_pid(user_id);
            Ok(status)
        }
        RunnerControlResponse::Error(error) => Err(daemon_command_failed(format!(
            "daemon shutdown failed for user `{user_id}`: {}",
            error.message
        ))),
        _ => Err(daemon_command_failed(format!(
            "unexpected response while stopping daemon for user `{user_id}`"
        ))),
    }
}

fn parse_start_payload(
    payload: Result<Json<StartUserRequest>, JsonRejection>,
) -> Result<StartUserRequest, ErrorResponse> {
    payload
        .map(|Json(request)| request)
        .map_err(|error| invalid_request(format!("invalid lifecycle payload: {error}")))
}

fn ensure_registered_user(state: &WebState, user_id: &str) -> Result<String, ErrorResponse> {
    let user_id = validate_user_id(user_id).map_err(invalid_request)?;
    let global_config = state.latest_global_config_or_cached();
    if !global_config.users.contains_key(user_id) {
        return Err(not_found(format!("User `{user_id}` is not registered")));
    }
    Ok(user_id.to_owned())
}

async fn probe_health_status(
    socket_path: &Path,
) -> Result<Option<RunnerControlHealthStatus>, ErrorResponse> {
    if !socket_path.exists() {
        return Ok(None);
    }

    match send_control_to_daemon_async(socket_path, &RunnerControl::HealthCheck).await {
        Ok(RunnerControlResponse::HealthStatus(status)) => Ok(Some(status)),
        Ok(RunnerControlResponse::Error(error)) => Err(daemon_command_failed(format!(
            "daemon health check failed: {}",
            error.message
        ))),
        Ok(_) => Err(daemon_command_failed(
            "unexpected response to daemon health-check request",
        )),
        Err(error) if error.is_connection_refused() => Ok(None),
        Err(error) => Err(daemon_command_failed(format!(
            "daemon health check transport error: {error}"
        ))),
    }
}

fn spawn_detached_daemon(
    state: &WebState,
    user_id: &str,
    insecure: bool,
) -> Result<Option<u32>, ErrorResponse> {
    let mut command = Command::new(state.runner_executable());
    command
        .arg("--config")
        .arg(&state.config_path)
        .arg("--user")
        .arg(user_id);
    if insecure {
        command.arg("--insecure");
    }
    command
        .arg("start")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }

    let child = command.spawn().map_err(|error| {
        daemon_command_failed(format!(
            "failed to spawn detached daemon process for user `{user_id}`: {error}"
        ))
    })?;
    let pid = child.id();

    tracing::info!(
        user_id = %user_id,
        daemon_pid = pid,
        executable = %state.runner_executable().display(),
        "spawned detached runner daemon process from web control endpoint"
    );
    Ok(Some(pid))
}

async fn wait_for_daemon_health(
    socket_path: &Path,
    user_id: &str,
) -> Result<RunnerControlHealthStatus, ErrorResponse> {
    let started = Instant::now();
    loop {
        if let Some(status) = probe_health_status(socket_path).await? {
            return Ok(status);
        }
        if started.elapsed() >= START_TIMEOUT {
            return Err(daemon_command_timeout(format!(
                "timed out waiting {}s for daemon `{user_id}` to start",
                START_TIMEOUT.as_secs()
            )));
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn wait_for_gateway_endpoint(
    marker_path: &Path,
    user_id: &str,
) -> Result<String, ErrorResponse> {
    let started = Instant::now();
    loop {
        if marker_path.exists() {
            match std::fs::read_to_string(marker_path) {
                Ok(contents) => {
                    let endpoint = contents.trim();
                    if !endpoint.is_empty() {
                        return Ok(endpoint.to_owned());
                    }
                }
                Err(error) => {
                    tracing::debug!(
                        path = %marker_path.display(),
                        error = %error,
                        "gateway marker read failed while waiting for startup"
                    );
                }
            }
        }

        if started.elapsed() >= START_TIMEOUT {
            return Err(daemon_command_timeout(format!(
                "timed out waiting {}s for gateway endpoint marker `{}` for user `{user_id}`",
                START_TIMEOUT.as_secs(),
                marker_path.display()
            )));
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn wait_for_socket_removal(socket_path: &Path, user_id: &str) -> Result<(), ErrorResponse> {
    let started = Instant::now();
    while socket_path.exists() {
        if started.elapsed() >= STOP_TIMEOUT {
            return Err(daemon_command_timeout(format!(
                "timed out waiting {}s for daemon `{user_id}` to stop",
                STOP_TIMEOUT.as_secs()
            )));
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    Ok(())
}

fn cleanup_stale_socket(socket_path: &Path) {
    if let Err(error) = std::fs::remove_file(socket_path)
        && socket_path.exists()
    {
        tracing::warn!(
            path = %socket_path.display(),
            error = %error,
            "failed to remove stale control socket"
        );
    }
}

fn validate_user_id(user_id: &str) -> Result<&str, String> {
    let trimmed = user_id.trim();
    if trimmed.is_empty()
        || trimmed.contains('/')
        || trimmed.contains('\\')
        || trimmed.contains("..")
    {
        return Err(format!(
            "invalid user_id `{trimmed}`: value must not be empty and cannot contain `/`, `\\`, or `..`"
        ));
    }
    Ok(trimmed)
}

fn invalid_request(message: impl Into<String>) -> ErrorResponse {
    ApiError::with_status(StatusCode::BAD_REQUEST, "invalid_request", message)
}

fn not_found(message: impl Into<String>) -> ErrorResponse {
    ApiError::with_status(StatusCode::NOT_FOUND, "not_found", message)
}

fn daemon_not_running(message: impl Into<String>) -> ErrorResponse {
    ApiError::with_status(StatusCode::CONFLICT, "daemon_not_running", message)
}

fn daemon_already_running(message: impl Into<String>) -> ErrorResponse {
    ApiError::with_status(StatusCode::CONFLICT, "daemon_already_running", message)
}

fn daemon_command_failed(message: impl Into<String>) -> ErrorResponse {
    ApiError::with_status(StatusCode::BAD_GATEWAY, "daemon_command_failed", message)
}

fn daemon_command_timeout(message: impl Into<String>) -> ErrorResponse {
    ApiError::with_status(
        StatusCode::GATEWAY_TIMEOUT,
        "daemon_command_timeout",
        message,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use std::collections::BTreeMap;
    use tower::ServiceExt;
    use types::{RunnerGlobalConfig, RunnerUserRegistration};

    fn state_with_user(dir: &std::path::Path, user_id: &str) -> Arc<WebState> {
        let mut users = BTreeMap::new();
        users.insert(
            user_id.to_owned(),
            RunnerUserRegistration {
                config_path: format!("users/{user_id}.toml"),
            },
        );
        let config = RunnerGlobalConfig {
            workspace_root: dir.join("workspaces").display().to_string(),
            users,
            ..RunnerGlobalConfig::default()
        };
        Arc::new(WebState::new(
            config,
            dir.join("runner.toml"),
            "127.0.0.1:9400".to_owned(),
        ))
    }

    fn with_api_headers(builder: axum::http::request::Builder) -> axum::http::request::Builder {
        builder
            .header("host", "127.0.0.1:9400")
            .header("content-type", "application/json")
    }

    #[tokio::test]
    async fn start_returns_not_found_for_unknown_user() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_user(dir.path(), "alice");
        let app = crate::web::build_router(state);
        let request = with_api_headers(
            Request::builder()
                .method("POST")
                .uri("/api/v1/control/bob/start"),
        )
        .body(Body::from("{}"))
        .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn stop_returns_daemon_not_running_when_socket_missing() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_user(dir.path(), "alice");
        let app = crate::web::build_router(state);
        let request = with_api_headers(
            Request::builder()
                .method("POST")
                .uri("/api/v1/control/alice/stop"),
        )
        .body(Body::from("{}"))
        .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"]["code"], "daemon_not_running");
    }

    #[tokio::test]
    async fn restart_returns_not_found_for_unknown_user() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_user(dir.path(), "alice");
        let app = crate::web::build_router(state);
        let request = with_api_headers(
            Request::builder()
                .method("POST")
                .uri("/api/v1/control/unknown/restart"),
        )
        .body(Body::from("{\"insecure\":false}"))
        .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
