use std::sync::Arc;

use axum::extract::{Path, State};
use axum::response::IntoResponse;
use serde::Serialize;

use types::{RunnerControl, RunnerControlResponse};

use super::response::{ApiError, ok_response};
use super::state::WebState;
use crate::{RUNNER_CONTROL_SOCKET_NAME, send_control_to_daemon_async};

#[derive(Serialize)]
struct UserStatus {
    user_id: String,
    daemon_running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    healthy: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sandbox_tier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    shell_available: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    browser_available: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

#[derive(Serialize)]
struct StatusResponse {
    users: Vec<UserStatus>,
}

/// `GET /api/v1/status` — Aggregated status for all registered users.
pub async fn get_status(State(state): State<Arc<WebState>>) -> impl IntoResponse {
    let mut users = Vec::new();

    for user_id in state.global_config.users.keys() {
        let status = probe_user_daemon(&state, user_id).await;
        users.push(status);
    }

    ok_response(StatusResponse { users })
}

/// `GET /api/v1/status/{user_id}` — Detailed status for a specific user.
pub async fn get_user_status(
    State(state): State<Arc<WebState>>,
    Path(user_id): Path<String>,
) -> impl IntoResponse {
    if !state.global_config.users.contains_key(&user_id) {
        return ApiError::with_status(
            axum::http::StatusCode::NOT_FOUND,
            "not_found",
            format!("User `{user_id}` is not registered"),
        )
        .into_response();
    }

    let status = probe_user_daemon(&state, &user_id).await;
    ok_response(status).into_response()
}

/// Try to reach a user's daemon via the control socket and extract status.
async fn probe_user_daemon(state: &WebState, user_id: &str) -> UserStatus {
    let socket_path = control_socket_path(state, user_id);

    if !socket_path.exists() {
        return UserStatus {
            user_id: user_id.to_owned(),
            daemon_running: false,
            healthy: None,
            sandbox_tier: None,
            shell_available: None,
            browser_available: None,
            message: None,
        };
    }

    match send_control_to_daemon_async(&socket_path, &RunnerControl::HealthCheck).await {
        Ok(RunnerControlResponse::HealthStatus(health)) => UserStatus {
            user_id: user_id.to_owned(),
            daemon_running: true,
            healthy: Some(health.healthy),
            sandbox_tier: Some(format!("{:?}", health.sandbox_tier)),
            shell_available: Some(health.shell_available),
            browser_available: Some(health.browser_available),
            message: health.message,
        },
        Ok(_) => UserStatus {
            user_id: user_id.to_owned(),
            daemon_running: true,
            healthy: None,
            sandbox_tier: None,
            shell_available: None,
            browser_available: None,
            message: Some("unexpected response from daemon".to_owned()),
        },
        Err(e) => {
            let is_refused = e.is_connection_refused();
            UserStatus {
                user_id: user_id.to_owned(),
                daemon_running: !is_refused,
                healthy: None,
                sandbox_tier: None,
                shell_available: None,
                browser_available: None,
                message: if is_refused {
                    None
                } else {
                    Some(format!("control socket error: {e}"))
                },
            }
        }
    }
}

/// Compute the control socket path for a user.
fn control_socket_path(state: &WebState, user_id: &str) -> std::path::PathBuf {
    state
        .workspace_root
        .join(user_id)
        .join("ipc")
        .join(RUNNER_CONTROL_SOCKET_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;
    use types::RunnerUserRegistration;

    #[tokio::test]
    async fn status_shows_no_users_when_empty() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("runner.toml");
        let config = types::RunnerGlobalConfig::default();
        let state = Arc::new(WebState::new(config, config_path));
        let app = crate::web::build_router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["data"]["users"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn status_shows_daemon_not_running() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("runner.toml");
        let mut users = std::collections::BTreeMap::new();
        users.insert(
            "alice".to_owned(),
            RunnerUserRegistration {
                config_path: "alice.toml".to_owned(),
            },
        );
        let config = types::RunnerGlobalConfig {
            workspace_root: dir.path().join("workspaces").display().to_string(),
            users,
            ..Default::default()
        };
        let state = Arc::new(WebState::new(config, config_path));
        let app = crate::web::build_router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let users = json["data"]["users"].as_array().unwrap();
        assert_eq!(users.len(), 1);
        assert_eq!(users[0]["user_id"], "alice");
        assert_eq!(users[0]["daemon_running"], false);
    }

    #[tokio::test]
    async fn user_status_returns_not_found_for_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("runner.toml");
        let config = types::RunnerGlobalConfig::default();
        let state = Arc::new(WebState::new(config, config_path));
        let app = crate::web::build_router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/status/unknown")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
