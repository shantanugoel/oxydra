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

        if self.runtime.turn_timeout_secs == 0 {
            return Err(ConfigError::InvalidRuntimeLimit {
                field: "turn_timeout_secs",
                value: 0,
            });
        }

        if self.runtime.max_turns == 0 {
            return Err(ConfigError::InvalidRuntimeLimit {
                field: "max_turns",
                value: 0,
            });
        }

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
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            turn_timeout_secs: default_turn_timeout_secs(),
            max_turns: default_max_turns(),
            max_cost: None,
        }
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

#[derive(Debug, Error, Clone, PartialEq, Eq)]
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
    #[error("reliability max_attempts must be greater than zero; got {attempts}")]
    InvalidReliabilityAttempts { attempts: u32 },
    #[error(
        "invalid reliability backoff bounds base={base_ms}ms max={max_ms}ms (both must be >0 and base<=max)"
    )]
    InvalidReliabilityBackoff { base_ms: u64, max_ms: u64 },
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

fn default_max_turns() -> usize {
    8
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
