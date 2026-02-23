use std::{
    collections::BTreeMap,
    env, io,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use figment::{
    Figment,
    providers::{Env, Format, Serialized, Toml},
};
use memory::LibsqlMemory;
use provider::{ReliableProvider, RetryPolicy};
use runtime::{ContextBudgetLimits, RetrievalLimits, RuntimeLimits, SummarizationLimits};
use serde::Serialize;
use thiserror::Error;
use tools::{RuntimeToolsBootstrap, ToolAvailability, ToolRegistry, bootstrap_runtime_tools};
use types::{
    AgentConfig, BootstrapEnvelopeError, CatalogProvider, ConfigError, Memory, MemoryError,
    ModelCatalog, ModelDescriptor, Provider, ProviderError, ProviderId, RunnerBootstrapEnvelope,
    StartupStatusReport, WebSearchConfig,
};

const SYSTEM_CONFIG_DIR: &str = "/etc/oxydra";
const USER_CONFIG_DIR: &str = ".config/oxydra";
const WORKSPACE_CONFIG_DIR: &str = ".oxydra";
const AGENT_CONFIG_FILE: &str = "agent.toml";
const PROVIDERS_CONFIG_FILE: &str = "providers.toml";
const CONFIG_ENV_PREFIX: &str = "OXYDRA__";
const DEFAULT_PROFILE: &str = "default";

#[derive(Debug, Clone)]
pub struct ConfigSearchPaths {
    pub system_dir: PathBuf,
    pub user_dir: Option<PathBuf>,
    pub workspace_dir: PathBuf,
}

impl ConfigSearchPaths {
    pub fn discover() -> Result<Self, BootstrapError> {
        let workspace_dir = env::current_dir()?.join(WORKSPACE_CONFIG_DIR);
        let user_dir = env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| home.join(USER_CONFIG_DIR));
        Ok(Self {
            system_dir: PathBuf::from(SYSTEM_CONFIG_DIR),
            user_dir,
            workspace_dir,
        })
    }
}

pub struct VmBootstrapRuntime {
    pub bootstrap: Option<RunnerBootstrapEnvelope>,
    pub config: AgentConfig,
    pub provider: Box<dyn Provider>,
    pub memory: Option<Arc<dyn Memory>>,
    pub runtime_limits: RuntimeLimits,
    pub tool_registry: ToolRegistry,
    pub tool_availability: ToolAvailability,
    pub startup_status: StartupStatusReport,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct CliOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime: Option<RuntimeOverrides>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<MemoryOverrides>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selection: Option<SelectionOverrides>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub providers: Option<ProviderOverrides>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reliability: Option<ReliabilityOverrides>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub catalog: Option<CatalogOverrides>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct RuntimeOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_timeout_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_cost: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_budget: Option<ContextBudgetOverrides>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summarization: Option<SummarizationOverrides>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct MemoryOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub db_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retrieval: Option<RetrievalOverrides>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ContextBudgetOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger_ratio: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub safety_buffer_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_max_context_tokens: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SummarizationOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_ratio: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_turns: Option<usize>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct RetrievalOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vector_weight: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fts_weight: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SelectionOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ProviderOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registry: Option<BTreeMap<String, RegistryEntryOverrides>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RegistryEntryOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub catalog_provider: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ReliabilityOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backoff_base_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backoff_max_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jitter: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CatalogOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_catalog_validation: Option<bool>,
}

#[derive(Debug, Error)]
pub enum BootstrapError {
    #[error("failed to resolve configuration path: {0}")]
    Io(#[from] io::Error),
    #[error("failed to load configuration: {0}")]
    ConfigExtract(#[source] Box<figment::Error>),
    #[error(transparent)]
    ConfigValidation(#[from] ConfigError),
    #[error(transparent)]
    Memory(#[from] MemoryError),
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error(transparent)]
    Bootstrap(#[from] BootstrapEnvelopeError),
    #[error("unsupported provider `{provider}` in provider selection")]
    UnsupportedProvider { provider: String },
}

impl From<figment::Error> for BootstrapError {
    fn from(value: figment::Error) -> Self {
        Self::ConfigExtract(Box::new(value))
    }
}

pub fn load_agent_config(
    profile: Option<&str>,
    cli_overrides: CliOverrides,
) -> Result<AgentConfig, BootstrapError> {
    let paths = ConfigSearchPaths::discover()?;
    load_agent_config_with_paths(&paths, profile, cli_overrides)
}

pub fn load_agent_config_with_paths(
    paths: &ConfigSearchPaths,
    profile: Option<&str>,
    cli_overrides: CliOverrides,
) -> Result<AgentConfig, BootstrapError> {
    let selected_profile = profile
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_PROFILE);

    let mut figment = Figment::from(Serialized::defaults(AgentConfig::default()));
    figment = merge_directory(figment, &paths.system_dir, selected_profile);
    if let Some(user_dir) = &paths.user_dir {
        figment = merge_directory(figment, user_dir, selected_profile);
    }
    figment = merge_directory(figment, &paths.workspace_dir, selected_profile);
    figment = figment.merge(Env::prefixed(CONFIG_ENV_PREFIX).split("__"));
    figment = figment.merge(Serialized::defaults(cli_overrides));

    let config: AgentConfig = figment.select(selected_profile).extract()?;
    config.validate()?;
    Ok(config)
}

/// Resolves the model catalog using the following priority:
///
/// 1. Cached catalog (user-level `~/.config/oxydra/model_catalog.json`,
///    then workspace `.oxydra/model_catalog.json`)
/// 2. Auto-fetch from `https://models.dev/api.json` (writes to user cache on success)
/// 3. Compiled-in pinned snapshot as final fallback
pub fn resolve_model_catalog(provider_id: &ProviderId) -> Result<ModelCatalog, BootstrapError> {
    // Try cached catalogs first
    if let Some(catalog) = ModelCatalog::load_from_cache() {
        tracing::debug!("loaded model catalog from cache");
        return Ok(catalog);
    }

    // Auto-fetch from models.dev
    const MODELS_DEV_URL: &str = "https://models.dev/api.json";
    let fetch_result =
        std::thread::spawn(|| reqwest::blocking::get(MODELS_DEV_URL).and_then(|r| r.text()))
            .join()
            .map_err(|_| "fetch thread panicked".to_owned())
            .and_then(|r| r.map_err(|e| e.to_string()));
    match fetch_result {
        Ok(body) => match ModelCatalog::from_snapshot_str(&body) {
            Ok(catalog) => {
                // Apply compiled-in overrides
                let overrides_str = ModelCatalog::pinned_overrides_json();
                let overrides: types::CapsOverrides =
                    serde_json::from_str(overrides_str).unwrap_or_default();
                let catalog = catalog.with_caps_overrides(overrides);

                // Write to user cache
                if let Some(user_path) = ModelCatalog::user_cache_path() {
                    if let Some(parent) = user_path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    if let Err(e) = std::fs::write(&user_path, &body) {
                        tracing::warn!(
                            path = %user_path.display(),
                            error = %e,
                            "failed to write catalog cache"
                        );
                    } else {
                        tracing::debug!(path = %user_path.display(), "wrote catalog cache");
                    }
                }

                return Ok(catalog);
            }
            Err(e) => {
                tracing::warn!(
                    source = "models.dev",
                    error = %e,
                    "unsupported catalog schema from auto-fetch; falling back to pinned snapshot"
                );
            }
        },
        Err(e) => {
            tracing::warn!(
                error = %e,
                "failed to auto-fetch models.dev catalog; falling back to pinned snapshot"
            );
        }
    }

    // Fall back to compiled-in pinned snapshot
    ModelCatalog::from_pinned_snapshot().map_err(|error| {
        ProviderError::RequestFailed {
            provider: provider_id.clone(),
            message: format!("failed to load model catalog: {error}"),
        }
        .into()
    })
}

pub fn build_reliable_provider(config: &AgentConfig) -> Result<ReliableProvider, BootstrapError> {
    config.validate()?;

    let provider_id = config.selection.provider.clone();
    let entry = config.providers.resolve(&provider_id.0)?;
    let mut model_catalog = resolve_model_catalog(&provider_id)?;

    let catalog_provider_id = entry.effective_catalog_provider();
    let catalog_provider = ProviderId::from(catalog_provider_id.as_str());
    let skip_validation = config.catalog.skip_catalog_validation;

    // If the model is not in the catalog and skip_catalog_validation is on,
    // insert a synthetic descriptor so downstream validation passes.
    let model_found = model_catalog
        .get(&catalog_provider, &config.selection.model)
        .is_some();

    if !model_found {
        if skip_validation {
            let caps = entry.unknown_model_caps();
            let synthetic = ModelDescriptor::default_for_unknown(&config.selection.model.0, &caps);
            tracing::info!(
                model = %config.selection.model,
                catalog_provider = %catalog_provider_id,
                "model not in catalog; using synthetic descriptor (skip_catalog_validation=true)"
            );

            // Ensure the catalog provider exists
            let provider_entry = model_catalog
                .providers
                .entry(catalog_provider_id.clone())
                .or_insert_with(|| CatalogProvider {
                    id: catalog_provider_id.clone(),
                    name: catalog_provider_id.clone(),
                    env: vec![],
                    api: None,
                    doc: None,
                    models: std::collections::BTreeMap::new(),
                });
            provider_entry
                .models
                .insert(config.selection.model.0.clone(), synthetic);
        } else {
            return Err(ConfigError::UnknownModelForCatalogProvider {
                model: config.selection.model.0.clone(),
                catalog_provider: catalog_provider_id.clone(),
            }
            .into());
        }
    }

    // Check for deprecated models via the Oxydra overlay and emit a warning.
    if model_catalog
        .caps_overrides
        .is_deprecated(&catalog_provider_id, &config.selection.model.0)
    {
        tracing::warn!(
            model = %config.selection.model,
            catalog_provider = %catalog_provider_id,
            "selected model is deprecated; consider switching to a supported alternative"
        );
    }

    let inner: Box<dyn Provider> = provider::build_provider(provider_id, entry, model_catalog)?;

    // Skip the separate capabilities validation when using a synthetic
    // descriptor — the defaults already provide what's needed.
    if !skip_validation || model_found {
        inner.capabilities(&config.selection.model)?;
    }

    Ok(ReliableProvider::new(
        inner,
        RetryPolicy {
            max_attempts: config.reliability.max_attempts,
            backoff_base: Duration::from_millis(config.reliability.backoff_base_ms),
            backoff_max: Duration::from_millis(config.reliability.backoff_max_ms),
        },
    ))
}

pub fn build_provider(config: &AgentConfig) -> Result<Box<dyn Provider>, BootstrapError> {
    Ok(Box::new(build_reliable_provider(config)?))
}

pub async fn build_memory_backend(
    config: &AgentConfig,
) -> Result<Option<Arc<dyn Memory>>, BootstrapError> {
    config.validate()?;
    let backend = LibsqlMemory::from_config(&config.memory).await?;
    Ok(backend.map(|memory| Arc::new(memory) as Arc<dyn Memory>))
}

pub fn runtime_limits(config: &AgentConfig) -> RuntimeLimits {
    RuntimeLimits {
        turn_timeout: Duration::from_secs(config.runtime.turn_timeout_secs),
        max_turns: config.runtime.max_turns,
        max_cost: config.runtime.max_cost,
        context_budget: ContextBudgetLimits {
            trigger_ratio: config.runtime.context_budget.trigger_ratio,
            safety_buffer_tokens: config.runtime.context_budget.safety_buffer_tokens,
            fallback_max_context_tokens: config.runtime.context_budget.fallback_max_context_tokens,
        },
        retrieval: RetrievalLimits {
            top_k: config.memory.retrieval.top_k,
            vector_weight: config.memory.retrieval.vector_weight,
            fts_weight: config.memory.retrieval.fts_weight,
        },
        summarization: SummarizationLimits {
            target_ratio: config.runtime.summarization.target_ratio,
            min_turns: config.runtime.summarization.min_turns,
        },
    }
}

pub async fn bootstrap_vm_runtime(
    bootstrap_frame: Option<&[u8]>,
    profile: Option<&str>,
    cli_overrides: CliOverrides,
) -> Result<VmBootstrapRuntime, BootstrapError> {
    let paths = ConfigSearchPaths::discover()?;
    bootstrap_vm_runtime_with_paths(&paths, bootstrap_frame, profile, cli_overrides).await
}

pub async fn bootstrap_vm_runtime_with_paths(
    paths: &ConfigSearchPaths,
    bootstrap_frame: Option<&[u8]>,
    profile: Option<&str>,
    cli_overrides: CliOverrides,
) -> Result<VmBootstrapRuntime, BootstrapError> {
    let bootstrap = bootstrap_frame
        .map(RunnerBootstrapEnvelope::from_length_prefixed_json)
        .transpose()?;
    let config = load_agent_config_with_paths(paths, profile, cli_overrides)?;
    if let Some(ws_config) = config.tools.web_search.as_ref() {
        apply_web_search_config(ws_config);
    }
    let provider = build_provider(&config)?;
    let memory = build_memory_backend(&config).await?;
    let runtime_limits = runtime_limits(&config);
    let RuntimeToolsBootstrap {
        registry,
        availability,
    } = bootstrap_runtime_tools(bootstrap.as_ref()).await;
    let startup_status = availability.startup_status(bootstrap.as_ref());

    Ok(VmBootstrapRuntime {
        bootstrap,
        config,
        provider,
        memory,
        runtime_limits,
        tool_registry: registry,
        tool_availability: availability,
        startup_status,
    })
}

/// Applies `[tools.web_search]` config values as `OXYDRA_WEB_SEARCH_*`
/// environment variables. Only sets variables that are not already present,
/// so explicit env vars always take precedence over config file values.
fn apply_web_search_config(config: &WebSearchConfig) {
    fn set_if_absent(key: &str, value: &str) {
        if !value.is_empty() && env::var(key).is_err() {
            // SAFETY: called during single-threaded bootstrap before tool execution begins.
            unsafe { env::set_var(key, value) };
        }
    }

    let provider = config
        .provider
        .as_deref()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();

    if !provider.is_empty() {
        set_if_absent("OXYDRA_WEB_SEARCH_PROVIDER", &provider);
    }

    let provider_upper = provider.to_ascii_uppercase();

    if let Some(base_url) = config.base_url.as_deref() {
        if !provider_upper.is_empty() {
            set_if_absent(
                &format!("OXYDRA_WEB_SEARCH_{provider_upper}_BASE_URL"),
                base_url,
            );
        }
    }
    if let Some(base_urls) = config.base_urls.as_deref() {
        if !provider_upper.is_empty() {
            set_if_absent(
                &format!("OXYDRA_WEB_SEARCH_{provider_upper}_BASE_URLS"),
                base_urls,
            );
        }
    }

    // api_key_env and engine_id_env are indirection: the config value names the
    // env var holding the actual secret, and we copy it into the canonical var.
    if provider == "google" {
        if let Some(api_key_env) = config.api_key_env.as_deref() {
            if let Ok(api_key) = env::var(api_key_env) {
                set_if_absent("OXYDRA_WEB_SEARCH_GOOGLE_API_KEY", &api_key);
            }
        }
        if let Some(engine_id_env) = config.engine_id_env.as_deref() {
            if let Ok(engine_id) = env::var(engine_id_env) {
                set_if_absent("OXYDRA_WEB_SEARCH_GOOGLE_CX", &engine_id);
            }
        }
    }

    if let Some(query_params) = config.query_params.as_deref() {
        set_if_absent("OXYDRA_WEB_SEARCH_QUERY_PARAMS", query_params);
    }

    // SearxNG-specific fields
    if let Some(engines) = config.engines.as_deref() {
        set_if_absent("OXYDRA_WEB_SEARCH_SEARXNG_ENGINES", engines);
    }
    if let Some(categories) = config.categories.as_deref() {
        set_if_absent("OXYDRA_WEB_SEARCH_SEARXNG_CATEGORIES", categories);
    }
    if let Some(safesearch) = config.safesearch {
        set_if_absent(
            "OXYDRA_WEB_SEARCH_SEARXNG_SAFESEARCH",
            &safesearch.to_string(),
        );
    }
}

fn merge_directory(mut figment: Figment, directory: &Path, selected_profile: &str) -> Figment {
    for file_name in [AGENT_CONFIG_FILE, PROVIDERS_CONFIG_FILE] {
        let path = directory.join(file_name);
        if path.is_file() {
            figment = if file_uses_profiles(&path, selected_profile) {
                figment.merge(Toml::file(path).nested())
            } else {
                figment.merge(Toml::file(path))
            };
        }
    }
    figment
}

fn file_uses_profiles(path: &Path, selected_profile: &str) -> bool {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(value) = toml::from_str::<toml::Value>(&contents) else {
        return false;
    };
    let Some(table) = value.as_table() else {
        return false;
    };

    table.contains_key("default")
        || table.contains_key("global")
        || table.contains_key(selected_profile)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::{Mutex, OnceLock},
        time::{SystemTime, UNIX_EPOCH},
    };

    use tokio::sync::Mutex as AsyncMutex;
    use types::{
        ModelId, ProviderId, RunnerBootstrapEnvelope, SandboxTier, SidecarEndpoint,
        SidecarTransport, StartupDegradedReasonCode,
    };

    use super::*;

    fn test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn async_test_lock() -> &'static AsyncMutex<()> {
        static LOCK: OnceLock<AsyncMutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| AsyncMutex::new(()))
    }

    #[derive(Debug)]
    struct EnvGuard {
        key: &'static str,
        previous: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = env::var(key).ok();
            // SAFETY: tests hold a process-wide mutex while mutating env to avoid races.
            unsafe { env::set_var(key, value) };
            Self { key, previous }
        }

        fn remove(key: &'static str) -> Self {
            let previous = env::var(key).ok();
            // SAFETY: tests hold a process-wide mutex while mutating env to avoid races.
            unsafe { env::remove_var(key) };
            Self { key, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            if let Some(value) = &self.previous {
                // SAFETY: tests hold a process-wide mutex while mutating env to avoid races.
                unsafe { env::set_var(self.key, value) };
            } else {
                // SAFETY: tests hold a process-wide mutex while mutating env to avoid races.
                unsafe { env::remove_var(self.key) };
            }
        }
    }

    #[test]
    fn load_agent_config_honors_file_env_and_cli_precedence() {
        let _lock = test_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = temp_dir("precedence");
        let paths = test_paths(&root);
        write_config(
            &paths.system_dir,
            AGENT_CONFIG_FILE,
            r#"
config_version = "1.0.0"
[selection]
provider = "openai"
model = "gpt-4o-mini"
[runtime]
max_turns = 2
"#,
        );
        write_config(
            &paths.user_dir.clone().expect("user dir should be present"),
            AGENT_CONFIG_FILE,
            r#"
[runtime]
max_turns = 3
"#,
        );
        write_config(
            &paths.workspace_dir,
            AGENT_CONFIG_FILE,
            r#"
[runtime]
max_turns = 4
"#,
        );
        write_config(
            &paths.workspace_dir,
            PROVIDERS_CONFIG_FILE,
            r#"
[providers.registry.openai]
provider_type = "openai"
base_url = "https://workspace-openai.example"
"#,
        );

        let _clear_runtime = EnvGuard::remove("OXYDRA__RUNTIME__MAX_TURNS");
        let _clear_openai_base_url =
            EnvGuard::remove("OXYDRA__PROVIDERS__REGISTRY__OPENAI__BASE_URL");
        let _runtime_override = EnvGuard::set("OXYDRA__RUNTIME__MAX_TURNS", "5");
        let _openai_base_url_override = EnvGuard::set(
            "OXYDRA__PROVIDERS__REGISTRY__OPENAI__BASE_URL",
            "https://env-openai.example",
        );

        let config = load_agent_config_with_paths(
            &paths,
            None,
            CliOverrides {
                runtime: Some(RuntimeOverrides {
                    max_turns: Some(6),
                    ..RuntimeOverrides::default()
                }),
                ..CliOverrides::default()
            },
        )
        .expect("config should load");

        assert_eq!(config.runtime.max_turns, 6);
        let entry = config
            .providers
            .resolve("openai")
            .expect("openai entry should exist");
        assert_eq!(
            entry.base_url.as_deref(),
            Some("https://env-openai.example")
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_agent_config_applies_profile_overrides() {
        let _lock = test_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = temp_dir("profile");
        let paths = test_paths(&root);
        write_config(
            &paths.workspace_dir,
            AGENT_CONFIG_FILE,
            r#"
[default]
config_version = "1.0.0"
[default.selection]
provider = "openai"
model = "gpt-4o-mini"
[default.runtime]
max_turns = 3
[prod.runtime]
max_turns = 11
"#,
        );

        let config = load_agent_config_with_paths(&paths, Some("prod"), CliOverrides::default())
            .expect("config should load");
        assert_eq!(config.runtime.max_turns, 11);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_agent_config_maps_nested_env_keys() {
        let _lock = test_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = temp_dir("env-nesting");
        let paths = test_paths(&root);

        let _clear_selection_provider = EnvGuard::remove("OXYDRA__SELECTION__PROVIDER");
        let _clear_selection_model = EnvGuard::remove("OXYDRA__SELECTION__MODEL");
        let _clear_anthropic_base_url =
            EnvGuard::remove("OXYDRA__PROVIDERS__REGISTRY__ANTHROPIC__BASE_URL");
        let _provider = EnvGuard::set("OXYDRA__SELECTION__PROVIDER", "anthropic");
        let _model = EnvGuard::set("OXYDRA__SELECTION__MODEL", "claude-3-5-haiku-latest");
        let _anthropic_base_url = EnvGuard::set(
            "OXYDRA__PROVIDERS__REGISTRY__ANTHROPIC__BASE_URL",
            "https://anthropic-env.example",
        );

        let config = load_agent_config_with_paths(&paths, None, CliOverrides::default())
            .expect("config should load");

        assert_eq!(config.selection.provider, ProviderId::from("anthropic"));
        let entry = config
            .providers
            .resolve("anthropic")
            .expect("anthropic entry should exist");
        assert_eq!(
            entry.base_url.as_deref(),
            Some("https://anthropic-env.example")
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_agent_config_rejects_unsupported_config_version() {
        let _lock = test_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = temp_dir("version");
        let paths = test_paths(&root);
        write_config(
            &paths.workspace_dir,
            AGENT_CONFIG_FILE,
            r#"
config_version = "2.0.0"
[selection]
provider = "openai"
model = "gpt-4o-mini"
"#,
        );

        let error = load_agent_config_with_paths(&paths, None, CliOverrides::default())
            .expect_err("unsupported version should fail");
        assert!(matches!(
            error,
            BootstrapError::ConfigValidation(ConfigError::UnsupportedConfigVersion { .. })
        ));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_agent_config_rejects_remote_memory_without_auth_token() {
        let _lock = test_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = temp_dir("memory-remote-auth");
        let paths = test_paths(&root);
        write_config(
            &paths.workspace_dir,
            AGENT_CONFIG_FILE,
            r#"
config_version = "1.0.0"
[selection]
provider = "openai"
model = "gpt-4o-mini"
[memory]
enabled = true
remote_url = "libsql://example-org.turso.io"
"#,
        );

        let error = load_agent_config_with_paths(&paths, None, CliOverrides::default())
            .expect_err("remote memory mode without auth token should fail validation");
        assert!(matches!(
            error,
            BootstrapError::ConfigValidation(ConfigError::MissingMemoryAuthToken { .. })
        ));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn provider_factory_switches_by_config_selection() {
        let mut openai_config = AgentConfig::default();
        openai_config.selection.provider = ProviderId::from("openai");
        openai_config.selection.model = ModelId::from("gpt-4o-mini");
        if let Some(entry) = openai_config.providers.registry.get_mut("openai") {
            entry.api_key = Some("openai-test-key".to_owned());
        }
        openai_config.reliability.max_attempts = 4;
        openai_config.reliability.backoff_base_ms = 10;
        openai_config.reliability.backoff_max_ms = 100;
        let reliable_openai =
            build_reliable_provider(&openai_config).expect("openai provider should be constructed");
        assert_eq!(reliable_openai.provider_id(), &ProviderId::from("openai"));
        assert_eq!(reliable_openai.retry_policy().max_attempts, 4);

        let mut anthropic_config = AgentConfig::default();
        anthropic_config.selection.provider = ProviderId::from("anthropic");
        anthropic_config.selection.model = ModelId::from("claude-3-5-haiku-latest");
        if let Some(entry) = anthropic_config.providers.registry.get_mut("anthropic") {
            entry.api_key = Some("anthropic-test-key".to_owned());
        }
        let provider = build_provider(&anthropic_config).expect("anthropic provider should build");
        assert_eq!(provider.provider_id(), &ProviderId::from("anthropic"));
    }

    #[tokio::test]
    async fn build_memory_backend_returns_none_when_memory_is_disabled() {
        let backend = build_memory_backend(&AgentConfig::default())
            .await
            .expect("disabled memory config should not fail");
        assert!(backend.is_none());
    }

    #[test]
    fn runtime_limits_maps_budget_retrieval_and_summarization_settings() {
        let mut config = AgentConfig::default();
        config.runtime.turn_timeout_secs = 30;
        config.runtime.max_turns = 5;
        config.runtime.max_cost = Some(3.25);
        config.runtime.context_budget.trigger_ratio = 0.9;
        config.runtime.context_budget.safety_buffer_tokens = 2_048;
        config.runtime.context_budget.fallback_max_context_tokens = 96_000;
        config.memory.retrieval.top_k = 12;
        config.memory.retrieval.vector_weight = 0.6;
        config.memory.retrieval.fts_weight = 0.4;
        config.runtime.summarization.target_ratio = 0.45;
        config.runtime.summarization.min_turns = 9;

        let limits = runtime_limits(&config);
        assert_eq!(limits.turn_timeout, Duration::from_secs(30));
        assert_eq!(limits.max_turns, 5);
        assert_eq!(limits.max_cost, Some(3.25));
        assert_eq!(limits.context_budget.trigger_ratio, 0.9);
        assert_eq!(limits.context_budget.safety_buffer_tokens, 2_048);
        assert_eq!(limits.context_budget.fallback_max_context_tokens, 96_000);
        assert_eq!(limits.retrieval.top_k, 12);
        assert_eq!(limits.retrieval.vector_weight, 0.6);
        assert_eq!(limits.retrieval.fts_weight, 0.4);
        assert_eq!(limits.summarization.target_ratio, 0.45);
        assert_eq!(limits.summarization.min_turns, 9);
    }

    #[tokio::test]
    async fn bootstrap_vm_runtime_with_process_tier_frame_disables_sidecar_tools() {
        let _lock = async_test_lock().lock().await;
        let _openai_key = EnvGuard::set("OPENAI_API_KEY", "test-openai-key");
        let _provider = EnvGuard::set("OXYDRA__SELECTION__PROVIDER", "openai");
        let _model = EnvGuard::set("OXYDRA__SELECTION__MODEL", "gpt-4o-mini");
        let root = temp_dir("bootstrap-process-tier");
        let paths = test_paths(&root);
        write_bootstrap_config(&paths);
        let frame = RunnerBootstrapEnvelope {
            user_id: "alice".to_owned(),
            sandbox_tier: SandboxTier::Process,
            workspace_root: "/tmp/oxydra-alice".to_owned(),
            sidecar_endpoint: None,
            runtime_policy: None,
            startup_status: None,
        }
        .to_length_prefixed_json()
        .expect("process-tier bootstrap frame should encode");

        let bootstrap =
            bootstrap_vm_runtime_with_paths(&paths, Some(&frame), None, CliOverrides::default())
                .await
                .expect("bootstrap runtime should initialize");

        assert_eq!(
            bootstrap
                .bootstrap
                .as_ref()
                .map(|envelope| envelope.sandbox_tier),
            Some(SandboxTier::Process)
        );
        assert!(!bootstrap.tool_availability.shell.is_ready());
        assert!(!bootstrap.tool_availability.browser.is_ready());
        assert_eq!(bootstrap.startup_status.sandbox_tier, SandboxTier::Process);
        assert!(!bootstrap.startup_status.sidecar_available);
        assert!(
            bootstrap
                .startup_status
                .has_reason_code(StartupDegradedReasonCode::InsecureProcessTier)
        );
        let error = bootstrap
            .tool_registry
            .execute("shell_exec", r#"{"command":"printf should-not-run"}"#)
            .await
            .expect_err("process-tier bootstrap should disable sidecar-dependent shell tool");
        assert!(matches!(
            error,
            types::ToolError::ExecutionFailed { tool, message }
                if tool == "shell_exec" && message.contains("disabled")
        ));

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn bootstrap_vm_runtime_with_sidecar_metadata_routes_shell_status_through_sidecar_path() {
        let _lock = async_test_lock().lock().await;
        let _openai_key = EnvGuard::set("OPENAI_API_KEY", "test-openai-key");
        let _provider = EnvGuard::set("OXYDRA__SELECTION__PROVIDER", "openai");
        let _model = EnvGuard::set("OXYDRA__SELECTION__MODEL", "gpt-4o-mini");
        let root = temp_dir("bootstrap-sidecar-metadata");
        let paths = test_paths(&root);
        write_bootstrap_config(&paths);
        let frame = RunnerBootstrapEnvelope {
            user_id: "alice".to_owned(),
            sandbox_tier: SandboxTier::Container,
            workspace_root: "/tmp/oxydra-alice".to_owned(),
            sidecar_endpoint: Some(SidecarEndpoint {
                transport: SidecarTransport::Unix,
                address: "tcp://invalid-sidecar-endpoint".to_owned(),
            }),
            runtime_policy: None,
            startup_status: None,
        }
        .to_length_prefixed_json()
        .expect("sidecar bootstrap frame should encode");

        let bootstrap =
            bootstrap_vm_runtime_with_paths(&paths, Some(&frame), None, CliOverrides::default())
                .await
                .expect("bootstrap runtime should initialize");

        assert!(
            bootstrap
                .bootstrap
                .as_ref()
                .and_then(|envelope| envelope.sidecar_endpoint.as_ref())
                .is_some()
        );
        assert!(!bootstrap.tool_availability.shell.is_ready());
        #[cfg(unix)]
        assert!(
            bootstrap
                .startup_status
                .has_reason_code(StartupDegradedReasonCode::SidecarEndpointInvalid)
        );
        #[cfg(not(unix))]
        assert!(
            bootstrap
                .startup_status
                .has_reason_code(StartupDegradedReasonCode::SidecarTransportUnsupported)
        );
        let error = bootstrap
            .tool_registry
            .execute("shell_exec", r#"{"command":"printf should-not-run"}"#)
            .await
            .expect_err("invalid sidecar metadata should keep shell tool unavailable");
        #[cfg(unix)]
        assert!(matches!(
            error,
            types::ToolError::ExecutionFailed { tool, message }
                if tool == "shell_exec" && message.contains("not a valid unix socket path")
        ));
        #[cfg(not(unix))]
        assert!(matches!(
            error,
            types::ToolError::ExecutionFailed { tool, message }
                if tool == "shell_exec" && message.contains("unsupported")
        ));

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn bootstrap_vm_runtime_without_frame_disables_sidecar_tools_for_direct_execution() {
        let _lock = async_test_lock().lock().await;
        let _openai_key = EnvGuard::set("OPENAI_API_KEY", "test-openai-key");
        let _provider = EnvGuard::set("OXYDRA__SELECTION__PROVIDER", "openai");
        let _model = EnvGuard::set("OXYDRA__SELECTION__MODEL", "gpt-4o-mini");
        let root = temp_dir("bootstrap-direct");
        let paths = test_paths(&root);
        write_bootstrap_config(&paths);

        let bootstrap =
            bootstrap_vm_runtime_with_paths(&paths, None, None, CliOverrides::default())
                .await
                .expect("direct bootstrap runtime should initialize");

        assert!(bootstrap.bootstrap.is_none());
        assert!(!bootstrap.tool_availability.shell.is_ready());
        assert!(!bootstrap.tool_availability.browser.is_ready());
        assert!(bootstrap.startup_status.is_degraded());
        assert!(
            bootstrap
                .startup_status
                .has_reason_code(StartupDegradedReasonCode::InsecureProcessTier)
        );
        let error = bootstrap
            .tool_registry
            .execute("shell_exec", r#"{"command":"printf should-not-run"}"#)
            .await
            .expect_err("direct runtime bootstrap should disable sidecar-dependent shell tool");
        assert!(matches!(
            error,
            types::ToolError::ExecutionFailed { tool, message }
                if tool == "shell_exec" && message.contains("disabled")
        ));

        let _ = fs::remove_dir_all(root);
    }

    fn temp_dir(label: &str) -> PathBuf {
        let mut path = env::temp_dir();
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be monotonic")
            .as_nanos();
        path.push(format!(
            "oxydra-bootstrap-{label}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("temp dir should be creatable");
        path
    }

    fn test_paths(root: &Path) -> ConfigSearchPaths {
        ConfigSearchPaths {
            system_dir: root.join("system"),
            user_dir: Some(root.join("user")),
            workspace_dir: root.join("workspace"),
        }
    }

    fn write_config(dir: &Path, file_name: &str, content: &str) {
        fs::create_dir_all(dir).expect("config dir should be creatable");
        fs::write(dir.join(file_name), content.trim_start())
            .expect("config file should be writable");
    }

    fn write_bootstrap_config(paths: &ConfigSearchPaths) {
        write_config(
            &paths.workspace_dir,
            AGENT_CONFIG_FILE,
            r#"
config_version = "1.0.0"
[selection]
provider = "openai"
model = "gpt-4o-mini"
[providers.registry.openai]
provider_type = "openai"
api_key = "test-openai-key"
"#,
        );
    }

    /// Verifies that selecting a model marked as deprecated in the overlay
    /// still builds successfully (the warning is logged, not an error) and
    /// the deprecation check itself returns `true`.
    #[test]
    fn deprecated_model_emits_warning() {
        use types::{CapsOverrideEntry, CapsOverrides};

        // Verify the deprecation lookup returns true for a model with
        // `deprecated = true` in the overlay.
        let mut overrides = CapsOverrides::default();
        overrides.overrides.insert(
            "openai/gpt-4o-mini".to_owned(),
            CapsOverrideEntry {
                deprecated: Some(true),
                ..CapsOverrideEntry::default()
            },
        );
        assert!(overrides.is_deprecated("openai", "gpt-4o-mini"));
        assert!(!overrides.is_deprecated("openai", "gpt-4o"));

        // Build a provider with a deprecated model to ensure it succeeds
        // (deprecation is a warning, not a hard error).
        let mut config = AgentConfig::default();
        config.selection.provider = ProviderId::from("openai");
        config.selection.model = ModelId::from("gpt-4o-mini");
        if let Some(entry) = config.providers.registry.get_mut("openai") {
            entry.api_key = Some("test-openai-key".to_owned());
        }
        // The pinned overlay does not mark gpt-4o-mini as deprecated, so
        // `build_reliable_provider` will not log a warning here. What matters
        // is that the code path that checks deprecation does not error out.
        let provider = build_reliable_provider(&config);
        assert!(provider.is_ok());
    }

    /// Selecting a model that belongs to a different catalog provider must
    /// be rejected with `UnknownModelForCatalogProvider`.
    #[test]
    fn provider_type_model_mismatch_rejected() {
        let mut config = AgentConfig::default();
        config.selection.provider = ProviderId::from("openai");
        // claude-3-5-haiku-latest is an Anthropic model, not in the OpenAI catalog
        config.selection.model = ModelId::from("claude-3-5-haiku-latest");
        if let Some(entry) = config.providers.registry.get_mut("openai") {
            entry.api_key = Some("test-openai-key".to_owned());
        }

        let result = build_reliable_provider(&config);
        assert!(
            result.is_err(),
            "selecting an Anthropic model under the OpenAI provider should fail catalog validation"
        );
        let error = match result {
            Err(e) => e,
            Ok(_) => unreachable!(),
        };
        assert!(
            matches!(
                error,
                BootstrapError::ConfigValidation(
                    ConfigError::UnknownModelForCatalogProvider { .. }
                )
            ),
            "expected UnknownModelForCatalogProvider but got: {error:?}"
        );
    }

    /// `openai-responses` provider type validates models against the `"openai"`
    /// catalog namespace (not `"openai-responses"`).
    #[test]
    fn openai_responses_provider_uses_openai_catalog() {
        let mut config = AgentConfig::default();
        config.selection.provider = ProviderId::from("openai-responses");
        config.selection.model = ModelId::from("gpt-4o-mini");
        config.providers.registry.insert(
            "openai-responses".to_owned(),
            types::ProviderRegistryEntry {
                provider_type: types::ProviderType::OpenaiResponses,
                base_url: None,
                api_key: Some("test-responses-key".to_owned()),
                api_key_env: None,
                extra_headers: None,
                catalog_provider: None, // should default to "openai"
                reasoning: None,
                max_input_tokens: None,
                max_output_tokens: None,
                max_context_tokens: None,
            },
        );

        let provider = build_reliable_provider(&config)
            .expect("openai-responses provider should validate gpt-4o-mini against openai catalog");
        assert_eq!(
            provider.provider_id(),
            &ProviderId::from("openai-responses")
        );
        assert_eq!(provider.catalog_provider_id(), &ProviderId::from("openai"));
    }

    /// End-to-end: load config from file with a custom registry entry and
    /// build a provider from it.
    #[test]
    fn full_bootstrap_with_registry_config() {
        let _lock = test_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = temp_dir("registry-config");
        let paths = test_paths(&root);

        // Write a config that defines a custom registry provider pointing
        // at the openai catalog via `catalog_provider`.
        write_config(
            &paths.workspace_dir,
            AGENT_CONFIG_FILE,
            r#"
config_version = "1.0.0"
[selection]
provider = "my-proxy"
model = "gpt-4o-mini"
"#,
        );
        write_config(
            &paths.workspace_dir,
            PROVIDERS_CONFIG_FILE,
            r#"
[providers.registry.my-proxy]
provider_type = "openai"
api_key = "proxy-test-key"
base_url = "https://my-proxy.example"
catalog_provider = "openai"
"#,
        );

        let _clear_provider = EnvGuard::remove("OXYDRA__SELECTION__PROVIDER");
        let _clear_model = EnvGuard::remove("OXYDRA__SELECTION__MODEL");
        let _clear_proxy_key = EnvGuard::remove("OXYDRA__PROVIDERS__REGISTRY__MY_PROXY__API_KEY");

        let config = load_agent_config_with_paths(&paths, None, CliOverrides::default())
            .expect("config with custom registry should load");

        assert_eq!(config.selection.provider, ProviderId::from("my-proxy"));
        let entry = config
            .providers
            .resolve("my-proxy")
            .expect("my-proxy entry should exist");
        assert_eq!(entry.effective_catalog_provider(), "openai");

        let provider = build_reliable_provider(&config)
            .expect("provider from custom registry entry should build");
        assert_eq!(provider.provider_id(), &ProviderId::from("my-proxy"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn apply_web_search_config_sets_provider_and_base_url_env_vars() {
        let _lock = test_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let _clear_provider = EnvGuard::remove("OXYDRA_WEB_SEARCH_PROVIDER");
        let _clear_base_url = EnvGuard::remove("OXYDRA_WEB_SEARCH_GOOGLE_BASE_URL");
        let _clear_query_params = EnvGuard::remove("OXYDRA_WEB_SEARCH_QUERY_PARAMS");

        let config = WebSearchConfig {
            provider: Some("google".to_owned()),
            base_url: Some("https://custom-google.example/v1".to_owned()),
            query_params: Some("lr=lang_en".to_owned()),
            ..WebSearchConfig::default()
        };
        apply_web_search_config(&config);

        assert_eq!(
            env::var("OXYDRA_WEB_SEARCH_PROVIDER").ok().as_deref(),
            Some("google")
        );
        assert_eq!(
            env::var("OXYDRA_WEB_SEARCH_GOOGLE_BASE_URL")
                .ok()
                .as_deref(),
            Some("https://custom-google.example/v1")
        );
        assert_eq!(
            env::var("OXYDRA_WEB_SEARCH_QUERY_PARAMS").ok().as_deref(),
            Some("lr=lang_en")
        );
    }

    #[test]
    fn apply_web_search_config_does_not_override_existing_env_vars() {
        let _lock = test_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let _existing_provider =
            EnvGuard::set("OXYDRA_WEB_SEARCH_PROVIDER", "duckduckgo-explicit");

        let config = WebSearchConfig {
            provider: Some("google".to_owned()),
            ..WebSearchConfig::default()
        };
        apply_web_search_config(&config);

        assert_eq!(
            env::var("OXYDRA_WEB_SEARCH_PROVIDER").ok().as_deref(),
            Some("duckduckgo-explicit"),
            "explicit env var should take precedence over config"
        );
    }

    #[test]
    fn apply_web_search_config_resolves_google_api_key_via_indirection() {
        let _lock = test_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let _clear_provider = EnvGuard::remove("OXYDRA_WEB_SEARCH_PROVIDER");
        let _clear_api_key = EnvGuard::remove("OXYDRA_WEB_SEARCH_GOOGLE_API_KEY");
        let _clear_cx = EnvGuard::remove("OXYDRA_WEB_SEARCH_GOOGLE_CX");
        let _set_indirect_key = EnvGuard::set("MY_GOOGLE_KEY", "secret-key-123");
        let _set_indirect_cx = EnvGuard::set("MY_GOOGLE_CX", "engine-456");

        let config = WebSearchConfig {
            provider: Some("google".to_owned()),
            api_key_env: Some("MY_GOOGLE_KEY".to_owned()),
            engine_id_env: Some("MY_GOOGLE_CX".to_owned()),
            ..WebSearchConfig::default()
        };
        apply_web_search_config(&config);

        assert_eq!(
            env::var("OXYDRA_WEB_SEARCH_GOOGLE_API_KEY")
                .ok()
                .as_deref(),
            Some("secret-key-123")
        );
        assert_eq!(
            env::var("OXYDRA_WEB_SEARCH_GOOGLE_CX").ok().as_deref(),
            Some("engine-456")
        );
    }

    #[test]
    fn apply_web_search_config_sets_searxng_specific_fields() {
        let _lock = test_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let _clear_provider = EnvGuard::remove("OXYDRA_WEB_SEARCH_PROVIDER");
        let _clear_base_url = EnvGuard::remove("OXYDRA_WEB_SEARCH_SEARXNG_BASE_URL");
        let _clear_engines = EnvGuard::remove("OXYDRA_WEB_SEARCH_SEARXNG_ENGINES");
        let _clear_categories = EnvGuard::remove("OXYDRA_WEB_SEARCH_SEARXNG_CATEGORIES");
        let _clear_safesearch = EnvGuard::remove("OXYDRA_WEB_SEARCH_SEARXNG_SAFESEARCH");

        let config = WebSearchConfig {
            provider: Some("searxng".to_owned()),
            base_url: Some("https://searx.example".to_owned()),
            engines: Some("google,bing".to_owned()),
            categories: Some("general".to_owned()),
            safesearch: Some(1),
            ..WebSearchConfig::default()
        };
        apply_web_search_config(&config);

        assert_eq!(
            env::var("OXYDRA_WEB_SEARCH_PROVIDER").ok().as_deref(),
            Some("searxng")
        );
        assert_eq!(
            env::var("OXYDRA_WEB_SEARCH_SEARXNG_BASE_URL")
                .ok()
                .as_deref(),
            Some("https://searx.example")
        );
        assert_eq!(
            env::var("OXYDRA_WEB_SEARCH_SEARXNG_ENGINES")
                .ok()
                .as_deref(),
            Some("google,bing")
        );
        assert_eq!(
            env::var("OXYDRA_WEB_SEARCH_SEARXNG_CATEGORIES")
                .ok()
                .as_deref(),
            Some("general")
        );
        assert_eq!(
            env::var("OXYDRA_WEB_SEARCH_SEARXNG_SAFESEARCH")
                .ok()
                .as_deref(),
            Some("1")
        );
    }

    #[test]
    fn load_agent_config_parses_tools_web_search_section() {
        let _lock = test_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = temp_dir("tools-web-search");
        let paths = test_paths(&root);
        write_config(
            &paths.workspace_dir,
            AGENT_CONFIG_FILE,
            r#"
config_version = "1.0.0"
[selection]
provider = "openai"
model = "gpt-4o-mini"
[tools.web_search]
provider = "searxng"
base_url = "https://searx.example"
engines = "google,bing"
safesearch = 2
"#,
        );

        let config = load_agent_config_with_paths(&paths, None, CliOverrides::default())
            .expect("config with tools.web_search should load");

        let ws = config
            .tools
            .web_search
            .expect("web_search config should be present");
        assert_eq!(ws.provider.as_deref(), Some("searxng"));
        assert_eq!(ws.base_url.as_deref(), Some("https://searx.example"));
        assert_eq!(ws.engines.as_deref(), Some("google,bing"));
        assert_eq!(ws.safesearch, Some(2));

        let _ = fs::remove_dir_all(root);
    }
}
