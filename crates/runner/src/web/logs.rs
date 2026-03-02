use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use types::{
    LOG_TAIL_MAX, LogFormat, LogRole, LogSource, LogStream, RunnerControl,
    RunnerControlLogsRequest, RunnerControlLogsResponse, RunnerControlResponse, RunnerLogEntry,
};

use super::response::{ApiError, ErrorResponse, ok_response};
use super::state::WebState;
use crate::{extract_timestamp, read_log_file_tail, send_control_to_daemon_async, strip_timestamp};

#[derive(Debug, Default, Deserialize)]
pub struct LogsQuery {
    role: Option<String>,
    stream: Option<String>,
    tail: Option<usize>,
    since: Option<String>,
    format: Option<String>,
}

/// `GET /api/v1/logs/{user_id}` — retrieve logs from daemon or workspace files.
pub async fn get_logs(
    State(state): State<Arc<WebState>>,
    AxumPath(user_id): AxumPath<String>,
    Query(query): Query<LogsQuery>,
) -> impl IntoResponse {
    let user_id = match ensure_registered_user(&state, &user_id) {
        Ok(user_id) => user_id,
        Err(error) => return error.into_response(),
    };
    let request = match query.into_request() {
        Ok(request) => request,
        Err(error) => return error.into_response(),
    };

    let socket_path = state.control_socket_path(&user_id);
    if socket_path.exists() {
        match send_control_to_daemon_async(&socket_path, &RunnerControl::Logs(request.clone()))
            .await
        {
            Ok(RunnerControlResponse::Logs(response)) => {
                return ok_response(response).into_response();
            }
            Ok(RunnerControlResponse::Error(error)) => {
                return daemon_command_failed(format!(
                    "daemon logs request failed for user `{user_id}`: {}",
                    error.message
                ))
                .into_response();
            }
            Ok(_) => {
                return daemon_command_failed(format!(
                    "unexpected daemon response while fetching logs for user `{user_id}`"
                ))
                .into_response();
            }
            Err(error) if error.is_connection_refused() => {
                // Fall through to file-based log snapshot.
            }
            Err(error) => {
                return daemon_command_failed(format!(
                    "failed to fetch daemon logs for user `{user_id}`: {error}"
                ))
                .into_response();
            }
        }
    }

    let response = collect_logs_from_workspace(&state, &user_id, &request);
    ok_response(response).into_response()
}

impl LogsQuery {
    fn into_request(self) -> Result<RunnerControlLogsRequest, ErrorResponse> {
        if let Some(tail) = self.tail
            && tail == 0
        {
            return Err(invalid_request("`tail` must be greater than 0"));
        }

        let role = parse_role(self.role.as_deref())?;
        let stream = parse_stream(self.stream.as_deref())?;
        let format = parse_format(self.format.as_deref())?;
        let since = self.since.and_then(|since| {
            let trimmed = since.trim().to_owned();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        });

        Ok(RunnerControlLogsRequest {
            role,
            stream,
            tail: self.tail,
            since,
            format,
        })
    }
}

fn collect_logs_from_workspace(
    state: &WebState,
    user_id: &str,
    request: &RunnerControlLogsRequest,
) -> RunnerControlLogsResponse {
    let mut entries = Vec::new();
    let mut warnings = Vec::new();
    let mut any_file_truncated = false;
    let tail = request.effective_tail().min(LOG_TAIL_MAX);
    let log_dir = workspace_log_dir(state, user_id);

    for role_label in role_labels(request.role) {
        for stream_name in stream_labels(request.stream) {
            let path = log_dir.join(format!("{role_label}.{stream_name}.log"));
            if !path.exists() {
                continue;
            }

            match read_log_file_tail(&path, tail, request.since.as_deref()) {
                Ok((lines, file_truncated)) => {
                    if file_truncated {
                        any_file_truncated = true;
                    }
                    for line in lines {
                        entries.push(RunnerLogEntry {
                            timestamp: extract_timestamp(&line),
                            source: LogSource::ProcessFile,
                            role: role_label.to_owned(),
                            stream: stream_name.to_owned(),
                            message: strip_timestamp(&line),
                        });
                    }
                }
                Err(error) => warnings.push(format!("failed to read {}: {error}", path.display())),
            }
        }
    }

    let global_truncated = entries.len() > tail;
    if global_truncated {
        let skip = entries.len() - tail;
        entries.drain(..skip);
    }

    if entries.is_empty() && warnings.is_empty() {
        warnings.push(format!(
            "no log entries found under `{}` for user `{user_id}`",
            log_dir.display()
        ));
    }

    RunnerControlLogsResponse {
        entries,
        truncated: any_file_truncated || global_truncated,
        warnings,
    }
}

fn workspace_log_dir(state: &WebState, user_id: &str) -> PathBuf {
    state.workspace_root.join(user_id).join("logs")
}

fn role_labels(role: LogRole) -> Vec<&'static str> {
    match role {
        LogRole::Runtime => vec!["oxydra-vm"],
        LogRole::Sidecar => vec!["shell-vm"],
        LogRole::All => vec!["oxydra-vm", "shell-vm"],
    }
}

fn stream_labels(stream: LogStream) -> Vec<&'static str> {
    match stream {
        LogStream::Stdout => vec!["stdout"],
        LogStream::Stderr => vec!["stderr"],
        LogStream::Both => vec!["stdout", "stderr"],
    }
}

fn parse_role(raw: Option<&str>) -> Result<LogRole, ErrorResponse> {
    match raw.unwrap_or("runtime") {
        "runtime" => Ok(LogRole::Runtime),
        "sidecar" => Ok(LogRole::Sidecar),
        "all" => Ok(LogRole::All),
        other => Err(invalid_request(format!(
            "invalid `role` value `{other}`; expected runtime, sidecar, or all"
        ))),
    }
}

fn parse_stream(raw: Option<&str>) -> Result<LogStream, ErrorResponse> {
    match raw.unwrap_or("both") {
        "stdout" => Ok(LogStream::Stdout),
        "stderr" => Ok(LogStream::Stderr),
        "both" => Ok(LogStream::Both),
        other => Err(invalid_request(format!(
            "invalid `stream` value `{other}`; expected stdout, stderr, or both"
        ))),
    }
}

fn parse_format(raw: Option<&str>) -> Result<LogFormat, ErrorResponse> {
    match raw.unwrap_or("text") {
        "text" => Ok(LogFormat::Text),
        "json" => Ok(LogFormat::Json),
        other => Err(invalid_request(format!(
            "invalid `format` value `{other}`; expected text or json"
        ))),
    }
}

fn ensure_registered_user(state: &WebState, user_id: &str) -> Result<String, ErrorResponse> {
    let global_config = state.latest_global_config_or_cached();
    if !global_config.users.contains_key(user_id) {
        return Err(not_found(format!("User `{user_id}` is not registered")));
    }
    Ok(user_id.to_owned())
}

fn invalid_request(message: impl Into<String>) -> ErrorResponse {
    ApiError::with_status(StatusCode::BAD_REQUEST, "invalid_request", message)
}

fn not_found(message: impl Into<String>) -> ErrorResponse {
    ApiError::with_status(StatusCode::NOT_FOUND, "not_found", message)
}

fn daemon_command_failed(message: impl Into<String>) -> ErrorResponse {
    ApiError::with_status(StatusCode::BAD_GATEWAY, "daemon_command_failed", message)
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
        builder.header("host", "127.0.0.1:9400")
    }

    #[tokio::test]
    async fn logs_returns_not_found_for_unknown_user() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_user(dir.path(), "alice");
        let app = crate::web::build_router(state);

        let response = app
            .oneshot(
                with_api_headers(Request::builder().uri("/api/v1/logs/bob"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn logs_fall_back_to_workspace_files() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_user(dir.path(), "alice");
        let app = crate::web::build_router(state.clone());

        let log_file = state
            .workspace_root
            .join("alice")
            .join("logs")
            .join("oxydra-vm.stdout.log");
        std::fs::create_dir_all(log_file.parent().unwrap()).unwrap();
        std::fs::write(
            &log_file,
            "2026-03-02T11:00:00Z first line\n2026-03-02T11:01:00Z second line\n",
        )
        .unwrap();

        let response = app
            .oneshot(
                with_api_headers(
                    Request::builder().uri("/api/v1/logs/alice?role=runtime&stream=stdout&tail=5"),
                )
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let entries = json["data"]["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1]["message"], "second line");
    }

    #[tokio::test]
    async fn logs_reject_invalid_stream_query() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_user(dir.path(), "alice");
        let app = crate::web::build_router(state);

        let response = app
            .oneshot(
                with_api_headers(Request::builder().uri("/api/v1/logs/alice?stream=invalid"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"]["code"], "invalid_request");
    }
}
