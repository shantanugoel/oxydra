use thiserror::Error;

use crate::{ModelId, ProviderId};

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("unknown model id: {model}")]
    UnknownModel { model: ModelId },
    #[error("provider request failed for {provider}: {message}")]
    RequestFailed { provider: ProviderId, message: String },
    #[error("provider serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
}

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("invalid arguments for tool {tool}: {message}")]
    InvalidArguments { tool: String, message: String },
    #[error("tool execution failed for {tool}: {message}")]
    ExecutionFailed { tool: String, message: String },
    #[error("tool serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
}

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error(transparent)]
    Tool(#[from] ToolError),
    #[error("turn cancelled")]
    Cancelled,
    #[error("turn budget exceeded")]
    BudgetExceeded,
}
