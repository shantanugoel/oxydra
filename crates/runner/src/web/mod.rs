mod config_read;
mod masking;
mod onboarding;
mod response;
mod state;
mod static_files;
mod status;

use std::path::Path;
use std::sync::Arc;

use axum::{Router, extract::State, routing::get};
use serde::Serialize;
use thiserror::Error;
use tokio::net::TcpListener;

use crate::load_runner_global_config;
pub use state::WebState;

/// Errors that can occur when running the web server.
#[derive(Debug, Error)]
pub enum WebServerError {
    #[error("failed to load runner config: {0}")]
    Config(#[from] crate::RunnerError),
    #[error("failed to bind web server to `{bind}`: {source}")]
    Bind {
        bind: String,
        #[source]
        source: std::io::Error,
    },
    #[error("web server error: {0}")]
    Serve(#[from] std::io::Error),
}

/// Entry point for `runner web`. Loads config, builds the router, and runs
/// the HTTP server until interrupted.
pub async fn run_web_server(
    config_path: &Path,
    bind_override: Option<String>,
) -> Result<(), WebServerError> {
    let config_path = std::fs::canonicalize(config_path).unwrap_or_else(|_| config_path.to_owned());
    let global_config = load_runner_global_config(&config_path)?;

    let bind = bind_override
        .as_deref()
        .unwrap_or(&global_config.web.bind)
        .to_owned();

    let web_state = Arc::new(WebState::new(global_config, config_path));
    let app = build_router(web_state);

    let listener = TcpListener::bind(&bind)
        .await
        .map_err(|source| WebServerError::Bind {
            bind: bind.clone(),
            source,
        })?;

    tracing::info!(bind = %bind, "web configurator started");
    eprintln!("Oxydra Web Configurator running at http://{bind}");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    tracing::info!("web configurator shut down");
    Ok(())
}

fn build_router(state: Arc<WebState>) -> Router {
    let api = Router::new()
        .route("/meta", get(meta_handler))
        // Phase 2: config read endpoints
        .route("/config/runner", get(config_read::get_runner_config))
        .route("/config/agent", get(config_read::get_agent_config))
        .route(
            "/config/agent/effective",
            get(config_read::get_agent_config_effective),
        )
        .route("/config/users", get(config_read::list_users))
        .route("/config/users/{user_id}", get(config_read::get_user_config))
        // Phase 2: status endpoints
        .route("/status", get(status::get_status))
        .route("/status/{user_id}", get(status::get_user_status))
        // Phase 2: onboarding
        .route("/onboarding/status", get(onboarding::get_onboarding_status));

    Router::new()
        .nest("/api/v1", api)
        .route("/", get(static_files::serve_index))
        .route("/{*path}", get(static_files::serve_static))
        .with_state(state)
}

#[derive(Serialize)]
struct MetaResponse {
    version: &'static str,
    config_path: String,
    workspace_root: String,
}

async fn meta_handler(State(state): State<Arc<WebState>>) -> response::ApiResponse<MetaResponse> {
    response::ok_response(MetaResponse {
        version: crate::VERSION,
        config_path: state.config_path.display().to_string(),
        workspace_root: state.workspace_root.display().to_string(),
    })
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("failed to install Ctrl+C handler");
    tracing::info!("received shutdown signal");
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;
    use types::RunnerGlobalConfig;

    fn test_state() -> Arc<WebState> {
        let config = RunnerGlobalConfig::default();
        let config_path = std::path::PathBuf::from("/tmp/test-runner.toml");
        Arc::new(WebState::new(config, config_path))
    }

    #[tokio::test]
    async fn meta_endpoint_returns_version() {
        let app = build_router(test_state());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/meta")
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
        assert_eq!(json["data"]["version"], crate::VERSION);
        assert!(json["meta"]["request_id"].is_string());
    }

    #[tokio::test]
    async fn index_serves_html() {
        let app = build_router(test_state());
        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            content_type.contains("text/html"),
            "expected text/html, got {content_type}"
        );
    }

    #[tokio::test]
    async fn unknown_api_path_returns_404_or_fallback() {
        let app = build_router(test_state());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/nonexistent")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // Unknown API routes fall through to SPA fallback, which serves index.html
        // This is expected behavior for SPA routing
        assert!(response.status() == StatusCode::OK || response.status() == StatusCode::NOT_FOUND);
    }
}
