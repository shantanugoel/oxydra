use thiserror::Error;

use crate::{ModelId, ProviderId};

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("unknown model id `{model}` for provider `{provider}`")]
    UnknownModel {
        provider: ProviderId,
        model: ModelId,
    },
    #[error("missing API key for provider `{provider}`")]
    MissingApiKey { provider: ProviderId },
    #[error("provider transport failed for `{provider}`: {message}")]
    Transport {
        provider: ProviderId,
        message: String,
    },
    #[error("provider `{provider}` returned HTTP {status}: {message}")]
    HttpStatus {
        provider: ProviderId,
        status: u16,
        message: String,
    },
    #[error("provider response parsing failed for `{provider}`: {message}")]
    ResponseParse {
        provider: ProviderId,
        message: String,
    },
    #[error("provider request failed for {provider}: {message}")]
    RequestFailed {
        provider: ProviderId,
        message: String,
    },
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
pub enum MemoryError {
    #[error("memory connection failed: {message}")]
    Connection { message: String },
    #[error("memory initialization failed: {message}")]
    Initialization { message: String },
    #[error("memory migration failed: {message}")]
    Migration { message: String },
    #[error("memory query failed: {message}")]
    Query { message: String },
    #[error("memory serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("memory item not found for session `{session_id}`")]
    NotFound { session_id: String },
}

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error(transparent)]
    Tool(#[from] ToolError),
    #[error(transparent)]
    Memory(#[from] MemoryError),
    #[error("turn cancelled")]
    Cancelled,
    #[error("turn budget exceeded")]
    BudgetExceeded,
}
