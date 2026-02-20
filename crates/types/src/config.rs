use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{ModelId, ProviderId};

pub const SUPPORTED_CONFIG_MAJOR_VERSION: u64 = 1;
pub const OPENAI_PROVIDER_ID: &str = "openai";
pub const ANTHROPIC_PROVIDER_ID: &str = "anthropic";
pub const OPENAI_DEFAULT_BASE_URL: &str = "https://api.openai.com";
pub const ANTHROPIC_DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

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
        }
    }
}

impl AgentConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        validate_config_version(&self.config_version)?;

        let provider = self.selection.provider.0.as_str();
        match provider {
            OPENAI_PROVIDER_ID | ANTHROPIC_PROVIDER_ID => {}
            _ => {
                return Err(ConfigError::UnsupportedProvider {
                    provider: provider.to_owned(),
                });
            }
        }

        if self.selection.model.0.trim().is_empty() {
            return Err(ConfigError::EmptyModelForProvider {
                provider: provider.to_owned(),
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
}

impl Default for ContextBudgetConfig {
    fn default() -> Self {
        Self {
            trigger_ratio: default_context_budget_trigger_ratio(),
            safety_buffer_tokens: default_context_safety_buffer_tokens(),
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_memory_db_path")]
    pub db_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<String>,
    #[serde(default)]
    pub retrieval: RetrievalConfig,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            db_path: default_memory_db_path(),
            remote_url: None,
            auth_token: None,
            retrieval: RetrievalConfig::default(),
        }
    }
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

        if self.db_path.trim().is_empty() {
            return Err(ConfigError::InvalidMemoryDatabasePath);
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProviderConfigs {
    #[serde(default)]
    pub openai: OpenAIProviderConfig,
    #[serde(default)]
    pub anthropic: AnthropicProviderConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenAIProviderConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default = "default_openai_base_url")]
    pub base_url: String,
}

impl Default for OpenAIProviderConfig {
    fn default() -> Self {
        Self {
            api_key: None,
            base_url: default_openai_base_url(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnthropicProviderConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default = "default_anthropic_base_url")]
    pub base_url: String,
}

impl Default for AnthropicProviderConfig {
    fn default() -> Self {
        Self {
            api_key: None,
            base_url: default_anthropic_base_url(),
        }
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
    #[error("runtime limit `{field}` must be greater than zero; got {value}")]
    InvalidRuntimeLimit { field: &'static str, value: u64 },
    #[error("runtime context budget trigger_ratio must be within [0.0, 1.0]; got {value}")]
    InvalidContextBudgetRatio { value: f64 },
    #[error("runtime context budget safety_buffer_tokens must be greater than zero; got {value}")]
    InvalidContextSafetyBufferTokens { value: u64 },
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
    #[error("memory database path must not be empty when memory is enabled in local mode")]
    InvalidMemoryDatabasePath,
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
    ProviderId::from(OPENAI_PROVIDER_ID)
}

fn default_model_id() -> ModelId {
    ModelId::from("gpt-4o-mini")
}

fn default_openai_base_url() -> String {
    OPENAI_DEFAULT_BASE_URL.to_owned()
}

fn default_anthropic_base_url() -> String {
    ANTHROPIC_DEFAULT_BASE_URL.to_owned()
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

fn default_summarization_target_ratio() -> f64 {
    0.5
}

fn default_summarization_min_turns() -> usize {
    6
}

fn default_memory_db_path() -> String {
    ".oxydra/memory.db".to_owned()
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
