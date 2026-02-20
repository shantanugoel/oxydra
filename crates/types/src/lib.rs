mod config;
mod error;
mod model;
mod provider;
mod tool;
mod tracing;

pub use config::{
    ANTHROPIC_DEFAULT_BASE_URL, ANTHROPIC_PROVIDER_ID, AgentConfig, AnthropicProviderConfig,
    ConfigError, OPENAI_DEFAULT_BASE_URL, OPENAI_PROVIDER_ID, OpenAIProviderConfig,
    ProviderConfigs, ProviderSelection, ReliabilityConfig, RuntimeConfig,
    SUPPORTED_CONFIG_MAJOR_VERSION, validate_config_version,
};
pub use error::{ProviderError, RuntimeError, ToolError};
pub use model::{
    Context, Message, MessageRole, ModelCatalog, ModelDescriptor, ModelId, ProviderCaps,
    ProviderId, Response, StreamItem, ToolCall, ToolCallDelta, UsageUpdate,
};
pub use provider::{Provider, ProviderStream};
pub use tool::{FunctionDecl, JsonSchema, JsonSchemaType, SafetyTier, Tool};
pub use tracing::init_tracing;
