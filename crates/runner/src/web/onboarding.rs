use std::sync::Arc;

use axum::extract::State;
use axum::response::IntoResponse;
use serde::Serialize;

use super::response::ok_response;
use super::state::WebState;

#[derive(Serialize)]
struct OnboardingStatus {
    needs_setup: bool,
    checks: OnboardingChecks,
}

#[derive(Serialize)]
struct OnboardingChecks {
    /// Whether runner.toml exists and is parseable.
    runner_config: bool,
    /// Whether at least one user is registered.
    has_users: bool,
    /// Whether agent.toml exists.
    agent_config: bool,
    /// Whether at least one provider has an API key configured.
    has_provider: bool,
}

/// `GET /api/v1/onboarding/status` — Detect first-run setup needs.
pub async fn get_onboarding_status(State(state): State<Arc<WebState>>) -> impl IntoResponse {
    let runner_config_exists = state.config_path.exists();
    let has_users = !state.global_config.users.is_empty();

    let config_dir = state.config_dir();
    let agent_path = config_dir.join("agent.toml");
    let agent_config_exists = agent_path.exists();

    let has_provider = check_provider_configured(&agent_path);

    let checks = OnboardingChecks {
        runner_config: runner_config_exists,
        has_users,
        agent_config: agent_config_exists,
        has_provider,
    };

    let needs_setup =
        !checks.runner_config || !checks.has_users || !checks.agent_config || !checks.has_provider;

    ok_response(OnboardingStatus {
        needs_setup,
        checks,
    })
}

/// Check whether at least one provider in the agent config has an API key
/// (either inline or via an env var that resolves to a non-empty value).
fn check_provider_configured(agent_path: &std::path::Path) -> bool {
    let Ok(contents) = std::fs::read_to_string(agent_path) else {
        return false;
    };
    let Ok(config) = toml::from_str::<types::AgentConfig>(&contents) else {
        return false;
    };

    for entry in config.providers.registry.values() {
        // Check inline api_key.
        if entry.api_key.as_ref().is_some_and(|k| !k.trim().is_empty()) {
            return true;
        }
        // Check api_key_env resolves to a non-empty value.
        if let Some(ref env_name) = entry.api_key_env
            && let Ok(value) = std::env::var(env_name)
            && !value.trim().is_empty()
        {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[tokio::test]
    async fn onboarding_needs_setup_when_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("runner.toml");
        // Don't create the file — simulates fresh install
        let config = types::RunnerGlobalConfig::default();
        let state = Arc::new(WebState::new(config, config_path));
        let app = crate::web::build_router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/onboarding/status")
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
        assert_eq!(json["data"]["needs_setup"], true);
        assert_eq!(json["data"]["checks"]["runner_config"], false);
        assert_eq!(json["data"]["checks"]["has_users"], false);
        assert_eq!(json["data"]["checks"]["agent_config"], false);
        assert_eq!(json["data"]["checks"]["has_provider"], false);
    }

    #[tokio::test]
    async fn onboarding_detects_existing_config() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("runner.toml");
        std::fs::write(
            &config_path,
            r#"
config_version = "1.0.1"
workspace_root = "workspaces"

[users.alice]
config_path = "alice.toml"
"#,
        )
        .unwrap();

        // Create agent.toml
        std::fs::write(
            dir.path().join("agent.toml"),
            r#"
config_version = "1.0.0"

[providers.registry.openai]
provider_type = "openai"
api_key = "sk-test-key"
"#,
        )
        .unwrap();

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
                    .uri("/api/v1/onboarding/status")
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
        assert_eq!(json["data"]["needs_setup"], false);
        assert_eq!(json["data"]["checks"]["runner_config"], true);
        assert_eq!(json["data"]["checks"]["has_users"], true);
        assert_eq!(json["data"]["checks"]["agent_config"], true);
        assert_eq!(json["data"]["checks"]["has_provider"], true);
    }
}
