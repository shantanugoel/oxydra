use std::sync::Arc;

use axum::extract::{Path, State};
use axum::response::IntoResponse;
use serde::Serialize;
use serde_json::Value;

use super::masking::mask_secrets;
use super::response::{ApiError, ok_response};
use super::state::WebState;
use crate::{load_runner_global_config, load_runner_user_config};

#[derive(Serialize)]
struct ConfigResponse {
    config: Value,
    file_exists: bool,
    file_path: String,
}

#[derive(Serialize)]
struct UserSummary {
    user_id: String,
    config_path: String,
}

#[derive(Serialize)]
struct UserListResponse {
    users: Vec<UserSummary>,
}

/// `GET /api/v1/config/runner` — Return the runner global config with secrets masked.
pub async fn get_runner_config(State(state): State<Arc<WebState>>) -> impl IntoResponse {
    let path = &state.config_path;
    if !path.exists() {
        let config = types::RunnerGlobalConfig::default();
        let mut json = serde_json::to_value(&config).unwrap_or_default();
        mask_secrets(&mut json);
        return ok_response(ConfigResponse {
            config: json,
            file_exists: false,
            file_path: path.display().to_string(),
        })
        .into_response();
    }

    match load_runner_global_config(path) {
        Ok(config) => {
            let mut json = serde_json::to_value(&config).unwrap_or_default();
            mask_secrets(&mut json);
            ok_response(ConfigResponse {
                config: json,
                file_exists: true,
                file_path: path.display().to_string(),
            })
            .into_response()
        }
        Err(e) => ApiError::with_status(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "config_read_failed",
            format!("Failed to load runner config: {e}"),
        )
        .into_response(),
    }
}

/// `GET /api/v1/config/agent` — Return the workspace agent config with secrets masked.
pub async fn get_agent_config(State(state): State<Arc<WebState>>) -> impl IntoResponse {
    let config_dir = state.config_dir();
    let agent_path = config_dir.join("agent.toml");

    if !agent_path.exists() {
        let config = types::AgentConfig::default();
        let mut json = serde_json::to_value(&config).unwrap_or_default();
        mask_secrets(&mut json);
        return ok_response(ConfigResponse {
            config: json,
            file_exists: false,
            file_path: agent_path.display().to_string(),
        })
        .into_response();
    }

    match std::fs::read_to_string(&agent_path) {
        Ok(contents) => match toml::from_str::<types::AgentConfig>(&contents) {
            Ok(config) => {
                let mut json = serde_json::to_value(&config).unwrap_or_default();
                mask_secrets(&mut json);
                ok_response(ConfigResponse {
                    config: json,
                    file_exists: true,
                    file_path: agent_path.display().to_string(),
                })
                .into_response()
            }
            Err(e) => ApiError::with_status(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "config_read_failed",
                format!("Failed to parse agent config: {e}"),
            )
            .into_response(),
        },
        Err(e) => ApiError::with_status(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "config_read_failed",
            format!("Failed to read agent config: {e}"),
        )
        .into_response(),
    }
}

/// `GET /api/v1/config/agent/effective` — Return the effective agent config
/// (loaded through figment with all layers merged).
pub async fn get_agent_config_effective(State(state): State<Arc<WebState>>) -> impl IntoResponse {
    // Try to load the fully merged config using the bootstrap path discovery.
    match crate::bootstrap::load_agent_config(None, crate::bootstrap::CliOverrides::default()) {
        Ok(config) => {
            let mut json = serde_json::to_value(&config).unwrap_or_default();
            mask_secrets(&mut json);
            ok_response(ConfigResponse {
                config: json,
                file_exists: true,
                file_path: state.config_dir().join("agent.toml").display().to_string(),
            })
            .into_response()
        }
        Err(e) => {
            // Fall back to the file-based config.
            tracing::warn!(error = %e, "failed to load effective agent config, falling back to file");
            get_agent_config(State(state)).await.into_response()
        }
    }
}

/// `GET /api/v1/config/users` — List all registered users.
pub async fn list_users(State(state): State<Arc<WebState>>) -> impl IntoResponse {
    let users: Vec<UserSummary> = state
        .global_config
        .users
        .iter()
        .map(|(user_id, reg)| UserSummary {
            user_id: user_id.clone(),
            config_path: reg.config_path.clone(),
        })
        .collect();

    ok_response(UserListResponse { users })
}

/// `GET /api/v1/config/users/{user_id}` — Return a specific user's config with secrets masked.
pub async fn get_user_config(
    State(state): State<Arc<WebState>>,
    Path(user_id): Path<String>,
) -> impl IntoResponse {
    let Some(registration) = state.global_config.users.get(&user_id) else {
        return ApiError::with_status(
            axum::http::StatusCode::NOT_FOUND,
            "not_found",
            format!("User `{user_id}` is not registered"),
        )
        .into_response();
    };

    let config_dir = state.config_dir();
    let user_config_path = {
        let p = std::path::PathBuf::from(&registration.config_path);
        if p.is_absolute() {
            p
        } else {
            config_dir.join(p)
        }
    };

    if !user_config_path.exists() {
        let config = types::RunnerUserConfig::default();
        let mut json = serde_json::to_value(&config).unwrap_or_default();
        mask_secrets(&mut json);
        return ok_response(ConfigResponse {
            config: json,
            file_exists: false,
            file_path: user_config_path.display().to_string(),
        })
        .into_response();
    }

    match load_runner_user_config(&user_config_path) {
        Ok(config) => {
            let mut json = serde_json::to_value(&config).unwrap_or_default();
            mask_secrets(&mut json);
            ok_response(ConfigResponse {
                config: json,
                file_exists: true,
                file_path: user_config_path.display().to_string(),
            })
            .into_response()
        }
        Err(e) => ApiError::with_status(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "config_read_failed",
            format!("Failed to load user config: {e}"),
        )
        .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn test_state_with_dir(dir: &std::path::Path) -> Arc<WebState> {
        let config_path = dir.join("runner.toml");
        let config = types::RunnerGlobalConfig::default();
        Arc::new(WebState::new(config, config_path))
    }

    #[tokio::test]
    async fn get_runner_config_returns_defaults_when_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state_with_dir(dir.path());
        let app = crate::web::build_router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/config/runner")
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
        assert_eq!(json["data"]["file_exists"], false);
        assert!(json["data"]["config"].is_object());
    }

    #[tokio::test]
    async fn get_runner_config_returns_file_contents() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("runner.toml");
        std::fs::write(
            &config_path,
            r#"
config_version = "1.0.1"
workspace_root = ".oxydra/workspaces"
"#,
        )
        .unwrap();

        let config = types::RunnerGlobalConfig::default();
        let state = Arc::new(WebState::new(config, config_path));
        let app = crate::web::build_router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/config/runner")
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
        assert_eq!(json["data"]["file_exists"], true);
    }

    #[tokio::test]
    async fn list_users_returns_registered_users() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("runner.toml");
        let mut config = types::RunnerGlobalConfig::default();
        config.users.insert(
            "alice".to_owned(),
            types::RunnerUserRegistration {
                config_path: "alice.toml".to_owned(),
            },
        );
        let state = Arc::new(WebState::new(config, config_path));
        let app = crate::web::build_router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/config/users")
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
    }

    #[tokio::test]
    async fn get_user_config_returns_not_found_for_unknown_user() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state_with_dir(dir.path());
        let app = crate::web::build_router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/config/users/unknown")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"]["code"], "not_found");
    }

    #[tokio::test]
    async fn get_agent_config_returns_defaults_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state_with_dir(dir.path());
        let app = crate::web::build_router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/config/agent")
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
        assert_eq!(json["data"]["file_exists"], false);
    }
}
