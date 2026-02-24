use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{ModelCatalog, ModelId, ProviderId};

pub const SUPPORTED_CONFIG_MAJOR_VERSION: u64 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentConfig {
    #[serde(default = "default_config_version")]
    pub config_version: String,
    #[serde(default)]
    pub runtime: RuntimeConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub selection: ProviderSelection,
    #[serde(default)]
    pub providers: ProviderConfigs,
    #[serde(default)]
    pub reliability: ReliabilityConfig,
    #[serde(default)]
    pub catalog: CatalogConfig,
    #[serde(default)]
    pub tools: ToolsConfig,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            config_version: default_config_version(),
            runtime: RuntimeConfig::default(),
            memory: MemoryConfig::default(),
            selection: ProviderSelection::default(),
            providers: ProviderConfigs::default(),
            reliability: ReliabilityConfig::default(),
            catalog: CatalogConfig::default(),
            tools: ToolsConfig::default(),
        }
    }
}

impl AgentConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        validate_config_version(&self.config_version)?;

        // Resolve provider via registry — replaces the old hardcoded match.
        self.providers.resolve(&self.selection.provider.0)?;

        if self.selection.model.0.trim().is_empty() {
            return Err(ConfigError::EmptyModelForProvider {
                provider: self.selection.provider.0.clone(),
            });
        }

        self.runtime.validate()?;

        if self.reliability.max_attempts == 0 {
            return Err(ConfigError::InvalidReliabilityAttempts { attempts: 0 });
        }

        if self.reliability.backoff_base_ms == 0
            || self.reliability.backoff_max_ms == 0
            || self.reliability.backoff_base_ms > self.reliability.backoff_max_ms
        {
            return Err(ConfigError::InvalidReliabilityBackoff {
                base_ms: self.reliability.backoff_base_ms,
                max_ms: self.reliability.backoff_max_ms,
            });
        }

        self.memory.validate()?;

        Ok(())
    }

    /// Validates that the selected model exists in the catalog under the
    /// resolved registry entry's catalog provider namespace.
    pub fn validate_model_in_catalog(&self, catalog: &ModelCatalog) -> Result<(), ConfigError> {
        let entry = self.providers.resolve(&self.selection.provider.0)?;
        let catalog_provider_id = entry.effective_catalog_provider();
        let catalog_provider = ProviderId::from(catalog_provider_id.as_str());
        if catalog
            .get(&catalog_provider, &self.selection.model)
            .is_none()
        {
            return Err(ConfigError::UnknownModelForCatalogProvider {
                model: self.selection.model.0.clone(),
                catalog_provider: catalog_provider_id,
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeConfig {
    #[serde(default = "default_turn_timeout_secs")]
    pub turn_timeout_secs: u64,
    #[serde(default = "default_max_turns")]
    pub max_turns: usize,
    #[serde(default)]
    pub max_cost: Option<f64>,
    #[serde(default)]
    pub context_budget: ContextBudgetConfig,
    #[serde(default)]
    pub summarization: SummarizationConfig,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            turn_timeout_secs: default_turn_timeout_secs(),
            max_turns: default_max_turns(),
            max_cost: None,
            context_budget: ContextBudgetConfig::default(),
            summarization: SummarizationConfig::default(),
        }
    }
}

impl RuntimeConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if self.turn_timeout_secs == 0 {
            return Err(ConfigError::InvalidRuntimeLimit {
                field: "turn_timeout_secs",
                value: 0,
            });
        }

        if self.max_turns == 0 {
            return Err(ConfigError::InvalidRuntimeLimit {
                field: "max_turns",
                value: 0,
            });
        }

        self.context_budget.validate()?;
        self.summarization.validate()?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextBudgetConfig {
    #[serde(default = "default_context_budget_trigger_ratio")]
    pub trigger_ratio: f64,
    #[serde(default = "default_context_safety_buffer_tokens")]
    pub safety_buffer_tokens: u64,
    #[serde(default = "default_context_fallback_max_context_tokens")]
    pub fallback_max_context_tokens: u32,
}

impl Default for ContextBudgetConfig {
    fn default() -> Self {
        Self {
            trigger_ratio: default_context_budget_trigger_ratio(),
            safety_buffer_tokens: default_context_safety_buffer_tokens(),
            fallback_max_context_tokens: default_context_fallback_max_context_tokens(),
        }
    }
}

impl ContextBudgetConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if !is_ratio(self.trigger_ratio) {
            return Err(ConfigError::InvalidContextBudgetRatio {
                value: self.trigger_ratio,
            });
        }
        if self.safety_buffer_tokens == 0 {
            return Err(ConfigError::InvalidContextSafetyBufferTokens { value: 0 });
        }
        if self.fallback_max_context_tokens == 0 {
            return Err(ConfigError::InvalidContextFallbackMaxContextTokens { value: 0 });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SummarizationConfig {
    #[serde(default = "default_summarization_target_ratio")]
    pub target_ratio: f64,
    #[serde(default = "default_summarization_min_turns")]
    pub min_turns: usize,
}

impl Default for SummarizationConfig {
    fn default() -> Self {
        Self {
            target_ratio: default_summarization_target_ratio(),
            min_turns: default_summarization_min_turns(),
        }
    }
}

impl SummarizationConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if !is_ratio(self.target_ratio) {
            return Err(ConfigError::InvalidSummarizationTargetRatio {
                value: self.target_ratio,
            });
        }
        if self.min_turns == 0 {
            return Err(ConfigError::InvalidSummarizationMinTurns { value: 0 });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MemoryConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<String>,
    #[serde(default)]
    pub retrieval: RetrievalConfig,
}

impl MemoryConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        self.retrieval.validate()?;

        if !self.enabled {
            return Ok(());
        }

        let remote_url = self
            .remote_url
            .as_deref()
            .map(str::trim)
            .filter(|url| !url.is_empty());
        if let Some(remote_url) = remote_url {
            if self
                .auth_token
                .as_deref()
                .map(str::trim)
                .is_none_or(str::is_empty)
            {
                return Err(ConfigError::MissingMemoryAuthToken {
                    remote_url: remote_url.to_owned(),
                });
            }
            return Ok(());
        }

        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetrievalConfig {
    #[serde(default = "default_retrieval_top_k")]
    pub top_k: usize,
    #[serde(default = "default_retrieval_vector_weight")]
    pub vector_weight: f64,
    #[serde(default = "default_retrieval_fts_weight")]
    pub fts_weight: f64,
}

impl Default for RetrievalConfig {
    fn default() -> Self {
        Self {
            top_k: default_retrieval_top_k(),
            vector_weight: default_retrieval_vector_weight(),
            fts_weight: default_retrieval_fts_weight(),
        }
    }
}

impl RetrievalConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        const WEIGHT_SUM_EPSILON: f64 = 1e-6;

        if self.top_k == 0 {
            return Err(ConfigError::InvalidRetrievalTopK { value: 0 });
        }
        if !is_ratio(self.vector_weight) {
            return Err(ConfigError::InvalidRetrievalWeight {
                field: "vector_weight",
                value: self.vector_weight,
            });
        }
        if !is_ratio(self.fts_weight) {
            return Err(ConfigError::InvalidRetrievalWeight {
                field: "fts_weight",
                value: self.fts_weight,
            });
        }
        let weight_sum = self.vector_weight + self.fts_weight;
        if (weight_sum - 1.0).abs() > WEIGHT_SUM_EPSILON {
            return Err(ConfigError::InvalidRetrievalWeightSum {
                vector_weight: self.vector_weight,
                fts_weight: self.fts_weight,
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderSelection {
    #[serde(default = "default_provider_id")]
    pub provider: ProviderId,
    #[serde(default = "default_model_id")]
    pub model: ModelId,
}

impl Default for ProviderSelection {
    fn default() -> Self {
        Self {
            provider: default_provider_id(),
            model: default_model_id(),
        }
    }
}

// ---------------------------------------------------------------------------
// Provider registry types
// ---------------------------------------------------------------------------

/// Discriminant for the underlying provider implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderType {
    Openai,
    Anthropic,
    Gemini,
    OpenaiResponses,
}

/// A single entry in the named provider registry.
///
/// Each entry maps a user-chosen name (e.g. `"openai"`, `"my-proxy"`) to a
/// concrete provider type with connection details.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderRegistryEntry {
    pub provider_type: ProviderType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_headers: Option<BTreeMap<String, String>>,
    /// Which catalog provider namespace to validate models against.
    /// Defaults by `provider_type`: `Openai` → `"openai"`,
    /// `Anthropic` → `"anthropic"`, `Gemini` → `"google"`,
    /// `OpenaiResponses` → `"openai"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_provider: Option<String>,
    /// Per-entry capability overrides for unknown models (used only when
    /// `skip_catalog_validation` is enabled and the model is not in the catalog).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_input_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_context_tokens: Option<u32>,
}

impl ProviderRegistryEntry {
    /// Returns the catalog provider ID for model validation.
    ///
    /// Uses the explicit `catalog_provider` if set, otherwise defaults
    /// based on `provider_type`.
    /// Extracts capability overrides for use with unknown model synthesis.
    pub fn unknown_model_caps(&self) -> UnknownModelCaps {
        UnknownModelCaps {
            reasoning: self.reasoning,
            max_input_tokens: self.max_input_tokens,
            max_output_tokens: self.max_output_tokens,
            max_context_tokens: self.max_context_tokens,
        }
    }

    pub fn effective_catalog_provider(&self) -> String {
        if let Some(ref cp) = self.catalog_provider {
            cp.clone()
        } else {
            match self.provider_type {
                ProviderType::Openai => "openai".to_owned(),
                ProviderType::Anthropic => "anthropic".to_owned(),
                ProviderType::Gemini => "google".to_owned(),
                ProviderType::OpenaiResponses => "openai".to_owned(),
            }
        }
    }
}

/// Provider configuration expressed as a named registry.
///
/// Each key in `registry` is a provider instance name (referenced by
/// `ProviderSelection.provider`) that maps to a [`ProviderRegistryEntry`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderConfigs {
    #[serde(default = "default_provider_registry")]
    pub registry: BTreeMap<String, ProviderRegistryEntry>,
}

impl Default for ProviderConfigs {
    fn default() -> Self {
        Self {
            registry: default_provider_registry(),
        }
    }
}

impl ProviderConfigs {
    /// Resolves a provider name to its registry entry.
    ///
    /// Returns `ConfigError::UnsupportedProvider` if the name is not present
    /// in the registry.
    pub fn resolve(&self, provider_name: &str) -> Result<&ProviderRegistryEntry, ConfigError> {
        self.registry
            .get(provider_name)
            .ok_or_else(|| ConfigError::UnsupportedProvider {
                provider: provider_name.to_owned(),
            })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReliabilityConfig {
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
    #[serde(default = "default_backoff_base_ms")]
    pub backoff_base_ms: u64,
    #[serde(default = "default_backoff_max_ms")]
    pub backoff_max_ms: u64,
    #[serde(default)]
    pub jitter: bool,
}

impl Default for ReliabilityConfig {
    fn default() -> Self {
        Self {
            max_attempts: default_max_attempts(),
            backoff_base_ms: default_backoff_base_ms(),
            backoff_max_ms: default_backoff_max_ms(),
            jitter: false,
        }
    }
}

/// Catalog resolution and validation configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogConfig {
    /// When `true`, unknown models (not found in the catalog) are allowed
    /// through with synthetic default capabilities instead of being rejected.
    #[serde(default)]
    pub skip_catalog_validation: bool,
    /// Optional URL for fetching the pinned catalog snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned_url: Option<String>,
}

/// Tool-specific configuration.
///
/// Provides structured alternatives to `OXYDRA_WEB_SEARCH_*` environment
/// variables. Values set here are applied as env vars at bootstrap time,
/// but explicit env vars always take precedence.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolsConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_search: Option<WebSearchConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<ShellConfig>,
}

/// Shell tool security policy configuration.
///
/// Controls which commands the LLM is allowed to execute via `shell_exec`.
/// By default a built-in allowlist is used; `allow` extends and `deny`
/// removes entries from it. Set `replace_defaults = true` to ignore the
/// built-in list entirely.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellConfig {
    /// Additional commands to add to the allowlist.
    /// Supports glob patterns (e.g., `"npm*"`, `"cargo-*"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow: Option<Vec<String>>,
    /// Commands to remove from the allowlist (deny takes precedence over allow).
    /// Supports glob patterns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deny: Option<Vec<String>>,
    /// If `true`, replaces the default allowlist entirely with `allow`.
    /// Default: `false` (extends the built-in list).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replace_defaults: Option<bool>,
    /// If `true`, allow shell control operators (`&&`, `||`, `|`, etc.).
    /// Default: `false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_operators: Option<bool>,
    /// Environment variable names to forward into the shell container.
    ///
    /// Each entry is a bare env var name (e.g. `"MY_TOKEN"`). If the variable
    /// is set in the runner's own environment, it is injected as `KEY=VALUE`
    /// into the shell-vm container. API keys referenced by the agent config
    /// are **not** forwarded to the shell container by default — only keys
    /// listed here are.
    ///
    /// Additionally, CLI `--env` / `--env-file` entries whose key starts with
    /// `SHELL_` are forwarded to the shell container with the prefix stripped
    /// (e.g. `SHELL_NPM_TOKEN` becomes `NPM_TOKEN`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_keys: Option<Vec<String>>,
}

/// Web search provider configuration.
///
/// Maps to `OXYDRA_WEB_SEARCH_*` environment variables at bootstrap time.
/// Explicit env vars take precedence over values set here.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSearchConfig {
    /// Search provider: "duckduckgo" (default), "google", or "searxng".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Override the default base URL for the selected provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Comma-separated fallback base URLs for the selected provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_urls: Option<String>,
    /// Name of the env var holding the Google API key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// Name of the env var holding the Google Custom Search engine ID (cx).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine_id_env: Option<String>,
    /// Extra query parameters as `key=value&key2=value2`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_params: Option<String>,
    /// SearxNG search engines (comma-separated).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engines: Option<String>,
    /// SearxNG categories (comma-separated).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub categories: Option<String>,
    /// SearxNG safesearch level (0-2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub safesearch: Option<u8>,
}

/// Capability overrides specified on a per-registry-entry basis.
///
/// Used when `skip_catalog_validation` is on and the model is not in the
/// catalog, to provide sensible defaults for the synthetic descriptor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UnknownModelCaps {
    pub reasoning: Option<bool>,
    pub max_input_tokens: Option<u32>,
    pub max_output_tokens: Option<u32>,
    pub max_context_tokens: Option<u32>,
}

#[derive(Debug, Error, Clone, PartialEq)]
pub enum ConfigError {
    #[error("unsupported config_version `{version}`; supported major is {supported_major}")]
    UnsupportedConfigVersion {
        version: String,
        supported_major: u64,
    },
    #[error("invalid config_version format `{version}`")]
    InvalidConfigVersionFormat { version: String },
    #[error("unsupported provider `{provider}` in provider selection")]
    UnsupportedProvider { provider: String },
    #[error("selected model is empty for provider `{provider}`")]
    EmptyModelForProvider { provider: String },
    #[error("unknown model `{model}` for catalog provider `{catalog_provider}`")]
    UnknownModelForCatalogProvider {
        model: String,
        catalog_provider: String,
    },
    #[error("runtime limit `{field}` must be greater than zero; got {value}")]
    InvalidRuntimeLimit { field: &'static str, value: u64 },
    #[error("runtime context budget trigger_ratio must be within [0.0, 1.0]; got {value}")]
    InvalidContextBudgetRatio { value: f64 },
    #[error("runtime context budget safety_buffer_tokens must be greater than zero; got {value}")]
    InvalidContextSafetyBufferTokens { value: u64 },
    #[error(
        "runtime context budget fallback_max_context_tokens must be greater than zero; got {value}"
    )]
    InvalidContextFallbackMaxContextTokens { value: u32 },
    #[error("runtime summarization target_ratio must be within [0.0, 1.0]; got {value}")]
    InvalidSummarizationTargetRatio { value: f64 },
    #[error("runtime summarization min_turns must be greater than zero; got {value}")]
    InvalidSummarizationMinTurns { value: usize },
    #[error("memory retrieval top_k must be greater than zero; got {value}")]
    InvalidRetrievalTopK { value: usize },
    #[error("memory retrieval weight `{field}` must be within [0.0, 1.0]; got {value}")]
    InvalidRetrievalWeight { field: &'static str, value: f64 },
    #[error(
        "memory retrieval weights must sum to 1.0; got vector_weight={vector_weight} and fts_weight={fts_weight}"
    )]
    InvalidRetrievalWeightSum { vector_weight: f64, fts_weight: f64 },
    #[error("reliability max_attempts must be greater than zero; got {attempts}")]
    InvalidReliabilityAttempts { attempts: u32 },
    #[error(
        "invalid reliability backoff bounds base={base_ms}ms max={max_ms}ms (both must be >0 and base<=max)"
    )]
    InvalidReliabilityBackoff { base_ms: u64, max_ms: u64 },
    #[error("memory remote mode requires an auth token for `{remote_url}`")]
    MissingMemoryAuthToken { remote_url: String },
}

pub fn validate_config_version(config_version: &str) -> Result<(), ConfigError> {
    let major = parse_major_version(config_version)?;
    if major != SUPPORTED_CONFIG_MAJOR_VERSION {
        return Err(ConfigError::UnsupportedConfigVersion {
            version: config_version.trim().to_owned(),
            supported_major: SUPPORTED_CONFIG_MAJOR_VERSION,
        });
    }
    Ok(())
}

fn parse_major_version(config_version: &str) -> Result<u64, ConfigError> {
    let trimmed = config_version.trim();
    if trimmed.is_empty() {
        return Err(ConfigError::InvalidConfigVersionFormat {
            version: config_version.to_owned(),
        });
    }

    let mut parts = trimmed.split('.');
    let first = parts
        .next()
        .ok_or_else(|| ConfigError::InvalidConfigVersionFormat {
            version: trimmed.to_owned(),
        })?;
    if first.is_empty() || !first.chars().all(|ch| ch.is_ascii_digit()) {
        return Err(ConfigError::InvalidConfigVersionFormat {
            version: trimmed.to_owned(),
        });
    }

    for part in parts {
        if part.is_empty() || !part.chars().all(|ch| ch.is_ascii_digit()) {
            return Err(ConfigError::InvalidConfigVersionFormat {
                version: trimmed.to_owned(),
            });
        }
    }

    first
        .parse::<u64>()
        .map_err(|_| ConfigError::InvalidConfigVersionFormat {
            version: trimmed.to_owned(),
        })
}

fn default_config_version() -> String {
    "1.0.0".to_owned()
}

fn default_provider_id() -> ProviderId {
    ProviderId::from("openai")
}

fn default_model_id() -> ModelId {
    ModelId::from("gpt-4o-mini")
}

fn default_provider_registry() -> BTreeMap<String, ProviderRegistryEntry> {
    let mut registry = BTreeMap::new();
    registry.insert(
        "openai".to_owned(),
        ProviderRegistryEntry {
            provider_type: ProviderType::Openai,
            base_url: None,
            api_key: None,
            api_key_env: Some("OPENAI_API_KEY".to_owned()),
            extra_headers: None,
            catalog_provider: None,
            reasoning: None,
            max_input_tokens: None,
            max_output_tokens: None,
            max_context_tokens: None,
        },
    );
    registry.insert(
        "anthropic".to_owned(),
        ProviderRegistryEntry {
            provider_type: ProviderType::Anthropic,
            base_url: None,
            api_key: None,
            api_key_env: Some("ANTHROPIC_API_KEY".to_owned()),
            extra_headers: None,
            catalog_provider: None,
            reasoning: None,
            max_input_tokens: None,
            max_output_tokens: None,
            max_context_tokens: None,
        },
    );
    registry
}

fn default_turn_timeout_secs() -> u64 {
    60
}

fn default_context_budget_trigger_ratio() -> f64 {
    0.85
}

fn default_context_safety_buffer_tokens() -> u64 {
    1_024
}

fn default_context_fallback_max_context_tokens() -> u32 {
    128_000
}

fn default_summarization_target_ratio() -> f64 {
    0.5
}

fn default_summarization_min_turns() -> usize {
    6
}

fn default_max_turns() -> usize {
    8
}

fn default_retrieval_top_k() -> usize {
    8
}

fn default_retrieval_vector_weight() -> f64 {
    0.7
}

fn default_retrieval_fts_weight() -> f64 {
    0.3
}

fn default_max_attempts() -> u32 {
    3
}

fn default_backoff_base_ms() -> u64 {
    250
}

fn default_backoff_max_ms() -> u64 {
    2_000
}

fn is_ratio(value: f64) -> bool {
    value.is_finite() && (0.0..=1.0).contains(&value)
}

#[cfg(test)]
mod tests {
    use crate::{CatalogProvider, ModelDescriptor};

    use super::*;

    #[test]
    fn default_config_has_openai_and_anthropic_registry_entries() {
        let config = ProviderConfigs::default();
        let openai = config.resolve("openai").expect("openai entry should exist");
        assert_eq!(openai.provider_type, ProviderType::Openai);
        assert_eq!(openai.api_key_env.as_deref(), Some("OPENAI_API_KEY"));

        let anthropic = config
            .resolve("anthropic")
            .expect("anthropic entry should exist");
        assert_eq!(anthropic.provider_type, ProviderType::Anthropic);
        assert_eq!(anthropic.api_key_env.as_deref(), Some("ANTHROPIC_API_KEY"));
    }

    #[test]
    fn custom_provider_resolves_from_registry() {
        let mut config = ProviderConfigs::default();
        config.registry.insert(
            "my-proxy".to_owned(),
            ProviderRegistryEntry {
                provider_type: ProviderType::Openai,
                base_url: Some("https://my-proxy.corp.internal/v1".to_owned()),
                api_key: None,
                api_key_env: Some("CORP_OPENAI_KEY".to_owned()),
                extra_headers: None,
                catalog_provider: None,
                reasoning: None,
                max_input_tokens: None,
                max_output_tokens: None,
                max_context_tokens: None,
            },
        );
        let entry = config
            .resolve("my-proxy")
            .expect("custom entry should resolve");
        assert_eq!(entry.provider_type, ProviderType::Openai);
        assert_eq!(
            entry.base_url.as_deref(),
            Some("https://my-proxy.corp.internal/v1")
        );
        assert_eq!(entry.effective_catalog_provider(), "openai");
    }

    #[test]
    fn unknown_provider_rejected() {
        let config = ProviderConfigs::default();
        let result = config.resolve("nonexistent");
        assert!(matches!(
            result,
            Err(ConfigError::UnsupportedProvider { provider }) if provider == "nonexistent"
        ));
    }

    #[test]
    fn effective_catalog_provider_defaults_by_type() {
        let openai_entry = ProviderRegistryEntry {
            provider_type: ProviderType::Openai,
            base_url: None,
            api_key: None,
            api_key_env: None,
            extra_headers: None,
            catalog_provider: None,
            reasoning: None,
            max_input_tokens: None,
            max_output_tokens: None,
            max_context_tokens: None,
        };
        assert_eq!(openai_entry.effective_catalog_provider(), "openai");

        let anthropic_entry = ProviderRegistryEntry {
            provider_type: ProviderType::Anthropic,
            catalog_provider: None,
            ..openai_entry.clone()
        };
        assert_eq!(anthropic_entry.effective_catalog_provider(), "anthropic");

        let gemini_entry = ProviderRegistryEntry {
            provider_type: ProviderType::Gemini,
            catalog_provider: None,
            ..openai_entry.clone()
        };
        assert_eq!(gemini_entry.effective_catalog_provider(), "google");

        let responses_entry = ProviderRegistryEntry {
            provider_type: ProviderType::OpenaiResponses,
            catalog_provider: None,
            ..openai_entry.clone()
        };
        assert_eq!(responses_entry.effective_catalog_provider(), "openai");

        // Explicit override takes precedence.
        let custom_entry = ProviderRegistryEntry {
            provider_type: ProviderType::Openai,
            catalog_provider: Some("custom-ns".to_owned()),
            ..openai_entry
        };
        assert_eq!(custom_entry.effective_catalog_provider(), "custom-ns");
    }

    #[test]
    fn model_validated_against_catalog_provider() {
        let catalog = test_catalog_with("openai", "gpt-4o-mini");
        let mut config = AgentConfig::default();
        config.selection.provider = ProviderId::from("openai");
        config.selection.model = ModelId::from("gpt-4o-mini");

        config
            .validate_model_in_catalog(&catalog)
            .expect("known model should pass catalog validation");

        config.selection.model = ModelId::from("nonexistent-model");
        let result = config.validate_model_in_catalog(&catalog);
        assert!(matches!(
            result,
            Err(ConfigError::UnknownModelForCatalogProvider {
                model,
                catalog_provider,
            }) if model == "nonexistent-model" && catalog_provider == "openai"
        ));
    }

    #[test]
    fn default_agent_config_validates_successfully() {
        AgentConfig::default()
            .validate()
            .expect("default config should validate");
    }

    #[test]
    fn validation_rejects_unknown_provider() {
        let mut config = AgentConfig::default();
        config.selection.provider = ProviderId::from("nonexistent");
        let result = config.validate();
        assert!(matches!(
            result,
            Err(ConfigError::UnsupportedProvider { provider }) if provider == "nonexistent"
        ));
    }

    #[test]
    fn provider_type_serde_round_trips() {
        let types = [
            (ProviderType::Openai, "\"openai\""),
            (ProviderType::Anthropic, "\"anthropic\""),
            (ProviderType::Gemini, "\"gemini\""),
            (ProviderType::OpenaiResponses, "\"openai_responses\""),
        ];
        for (variant, expected_json) in types {
            let serialized = serde_json::to_string(&variant).expect("should serialize");
            assert_eq!(serialized, expected_json);
            let deserialized: ProviderType =
                serde_json::from_str(&serialized).expect("should deserialize");
            assert_eq!(deserialized, variant);
        }
    }

    #[test]
    fn registry_entry_serialization_round_trip() {
        let entry = ProviderRegistryEntry {
            provider_type: ProviderType::Openai,
            base_url: Some("https://custom.example.com".to_owned()),
            api_key: None,
            api_key_env: Some("MY_KEY".to_owned()),
            extra_headers: Some(BTreeMap::from([(
                "X-Custom".to_owned(),
                "value".to_owned(),
            )])),
            catalog_provider: Some("openai".to_owned()),
            reasoning: None,
            max_input_tokens: None,
            max_output_tokens: None,
            max_context_tokens: None,
        };
        let json = serde_json::to_string(&entry).expect("should serialize");
        let deserialized: ProviderRegistryEntry =
            serde_json::from_str(&json).expect("should deserialize");
        assert_eq!(entry, deserialized);
    }

    fn test_catalog_with(provider: &str, model: &str) -> ModelCatalog {
        let mut models = BTreeMap::new();
        models.insert(
            model.to_owned(),
            ModelDescriptor {
                id: model.to_owned(),
                name: model.to_owned(),
                family: None,
                attachment: false,
                reasoning: false,
                tool_call: false,
                interleaved: None,
                structured_output: false,
                temperature: false,
                knowledge: None,
                release_date: None,
                last_updated: None,
                modalities: Default::default(),
                open_weights: false,
                cost: Default::default(),
                limit: Default::default(),
            },
        );
        let mut providers = BTreeMap::new();
        providers.insert(
            provider.to_owned(),
            CatalogProvider {
                id: provider.to_owned(),
                name: provider.to_owned(),
                env: vec![],
                api: None,
                doc: None,
                models,
            },
        );
        ModelCatalog::new(providers)
    }
}
