//! Catalog endpoints for the web configurator.
//!
//! Provides browsable model catalog data and a refresh mechanism.

use std::sync::Arc;

use axum::extract::State;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};

use super::response::{ApiError, ok_response};
use super::state::WebState;

use types::ModelCatalog;

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

/// Simplified model entry for the UI.
#[derive(Debug, Clone, Serialize)]
pub struct CatalogModelEntry {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    pub reasoning: bool,
    pub tool_call: bool,
    pub attachment: bool,
    pub modalities: Vec<String>,
    pub cost_input: f64,
    pub cost_output: f64,
    pub context_window: u32,
    pub max_output: u32,
}

/// Simplified provider entry for the UI.
#[derive(Debug, Clone, Serialize)]
pub struct CatalogProviderEntry {
    pub id: String,
    pub name: String,
    pub models: Vec<CatalogModelEntry>,
}

/// Response for `GET /api/v1/catalog`.
#[derive(Debug, Clone, Serialize)]
pub struct CatalogResponse {
    pub providers: Vec<CatalogProviderEntry>,
}

/// Response for `GET /api/v1/catalog/status`.
#[derive(Debug, Clone, Serialize)]
pub struct CatalogStatusResponse {
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<String>,
    pub provider_count: usize,
    pub model_count: usize,
}

/// Request body for `POST /api/v1/catalog/refresh`.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct CatalogRefreshRequest {
    #[serde(default)]
    pub pinned: bool,
}

/// Response for `POST /api/v1/catalog/refresh`.
#[derive(Debug, Clone, Serialize)]
pub struct CatalogRefreshResponse {
    pub success: bool,
    pub source: String,
    pub provider_count: usize,
    pub model_count: usize,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `GET /api/v1/catalog` — Returns the resolved model catalog.
pub async fn get_catalog(State(_state): State<Arc<WebState>>) -> impl IntoResponse {
    let catalog = load_resolved_catalog();
    let response = catalog_to_response(&catalog);
    ok_response(response)
}

/// `GET /api/v1/catalog/status` — Returns catalog source info and summary.
pub async fn get_catalog_status(State(_state): State<Arc<WebState>>) -> impl IntoResponse {
    let (source, last_modified) = catalog_source_info();
    let catalog = load_resolved_catalog();
    let model_count: usize = catalog.providers.values().map(|p| p.models.len()).sum();

    ok_response(CatalogStatusResponse {
        source,
        last_modified,
        provider_count: catalog.providers.len(),
        model_count,
    })
}

/// `POST /api/v1/catalog/refresh` — Triggers a catalog fetch.
pub async fn refresh_catalog(
    State(_state): State<Arc<WebState>>,
    body: Option<axum::Json<CatalogRefreshRequest>>,
) -> impl IntoResponse {
    let pinned = body.map(|b| b.pinned).unwrap_or(false);

    // Load the effective agent config to check for a pinned URL override.
    let pinned_url =
        crate::bootstrap::load_agent_config(None, crate::bootstrap::CliOverrides::default())
            .ok()
            .and_then(|config| config.catalog.pinned_url);

    // Run the blocking catalog fetch on a dedicated thread pool.
    let result = tokio::task::spawn_blocking(move || {
        crate::catalog::run_fetch(pinned, pinned_url.as_deref())
    })
    .await;

    match result {
        Ok(Ok(())) => {
            // Reload the catalog to get the updated summary.
            let catalog = load_resolved_catalog();
            let model_count: usize = catalog.providers.values().map(|p| p.models.len()).sum();
            let source = if pinned {
                "pinned snapshot"
            } else {
                "models.dev"
            };
            ok_response(CatalogRefreshResponse {
                success: true,
                source: source.to_owned(),
                provider_count: catalog.providers.len(),
                model_count,
            })
            .into_response()
        }
        Ok(Err(e)) => ApiError::with_status(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "catalog_refresh_failed",
            format!("Failed to refresh catalog: {e}"),
        )
        .into_response(),
        Err(e) => ApiError::with_status(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "catalog_refresh_failed",
            format!("Catalog refresh task panicked: {e}"),
        )
        .into_response(),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Loads the resolved catalog (cache → pinned snapshot).
fn load_resolved_catalog() -> ModelCatalog {
    ModelCatalog::load_from_cache()
        .unwrap_or_else(|| ModelCatalog::from_pinned_snapshot().unwrap_or_default())
}

/// Determines the catalog source and last-modified time.
fn catalog_source_info() -> (String, Option<String>) {
    if let Some(user_path) = ModelCatalog::user_cache_path()
        && user_path.is_file()
    {
        let mtime = std::fs::metadata(&user_path)
            .ok()
            .and_then(|m| m.modified().ok())
            .map(format_system_time);
        return (user_path.display().to_string(), mtime);
    }

    let workspace_path = ModelCatalog::workspace_cache_path();
    if workspace_path.is_file() {
        let mtime = std::fs::metadata(&workspace_path)
            .ok()
            .and_then(|m| m.modified().ok())
            .map(format_system_time);
        return (workspace_path.display().to_string(), mtime);
    }

    ("compiled-in pinned snapshot".to_owned(), None)
}

fn format_system_time(time: std::time::SystemTime) -> String {
    let duration = time
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = duration.as_secs();
    // Simple ISO-ish format without chrono dependency.
    // Format: seconds-since-epoch (frontend can format as needed).
    format!("{secs}")
}

fn catalog_to_response(catalog: &ModelCatalog) -> CatalogResponse {
    let mut providers = Vec::new();
    for (provider_id, provider) in &catalog.providers {
        let models: Vec<CatalogModelEntry> = provider
            .models
            .values()
            .map(|m| CatalogModelEntry {
                id: m.id.clone(),
                name: m.name.clone(),
                family: m.family.clone(),
                reasoning: m.reasoning,
                tool_call: m.tool_call,
                attachment: m.attachment,
                modalities: m.modalities.input.clone(),
                cost_input: m.cost.input,
                cost_output: m.cost.output,
                context_window: m.limit.context,
                max_output: m.limit.output,
            })
            .collect();

        providers.push(CatalogProviderEntry {
            id: provider_id.clone(),
            name: provider.name.clone(),
            models,
        });
    }
    CatalogResponse { providers }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn test_state() -> Arc<WebState> {
        let config = types::RunnerGlobalConfig::default();
        let config_path = std::path::PathBuf::from("/tmp/test-runner.toml");
        Arc::new(WebState::new(
            config,
            config_path,
            "127.0.0.1:9400".to_owned(),
        ))
    }

    #[tokio::test]
    async fn catalog_endpoint_returns_provider_data() {
        let state = test_state();
        let app = crate::web::build_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/catalog")
                    .header("host", "127.0.0.1:9400")
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
        assert!(json["data"]["providers"].is_array());
    }

    #[tokio::test]
    async fn catalog_status_endpoint_returns_source_info() {
        let state = test_state();
        let app = crate::web::build_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/catalog/status")
                    .header("host", "127.0.0.1:9400")
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
        assert!(json["data"]["source"].is_string());
        assert!(json["data"]["provider_count"].is_number());
        assert!(json["data"]["model_count"].is_number());
    }

    #[test]
    fn catalog_to_response_converts_correctly() {
        use std::collections::BTreeMap;
        use types::{CatalogProvider, ModelDescriptor};

        let mut models = BTreeMap::new();
        models.insert(
            "gpt-4o".to_owned(),
            ModelDescriptor {
                id: "gpt-4o".to_owned(),
                name: "GPT-4o".to_owned(),
                family: Some("gpt-4o".to_owned()),
                attachment: true,
                reasoning: false,
                tool_call: true,
                interleaved: None,
                structured_output: true,
                temperature: true,
                knowledge: None,
                release_date: None,
                last_updated: None,
                modalities: types::Modalities {
                    input: vec!["image".to_owned()],
                    output: vec![],
                },
                open_weights: false,
                cost: types::ModelCost {
                    input: 2.5,
                    output: 10.0,
                    cache_read: None,
                    cache_write: None,
                },
                limit: types::ModelLimits {
                    context: 128000,
                    output: 16384,
                },
            },
        );

        let mut providers = BTreeMap::new();
        providers.insert(
            "openai".to_owned(),
            CatalogProvider {
                id: "openai".to_owned(),
                name: "OpenAI".to_owned(),
                env: vec![],
                api: None,
                doc: None,
                models,
            },
        );

        let catalog = ModelCatalog::new(providers);
        let response = catalog_to_response(&catalog);

        assert_eq!(response.providers.len(), 1);
        assert_eq!(response.providers[0].id, "openai");
        assert_eq!(response.providers[0].models.len(), 1);
        let model = &response.providers[0].models[0];
        assert_eq!(model.id, "gpt-4o");
        assert!(model.tool_call);
        assert!(model.attachment);
        assert_eq!(model.context_window, 128000);
    }

    #[test]
    fn load_resolved_catalog_returns_some_data() {
        let catalog = load_resolved_catalog();
        // Should at least get the pinned snapshot
        assert!(
            !catalog.providers.is_empty(),
            "resolved catalog should have at least pinned snapshot providers"
        );
    }
}
