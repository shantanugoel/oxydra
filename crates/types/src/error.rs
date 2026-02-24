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

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ChannelError {
    #[error("channel `{channel}` is unavailable")]
    Unavailable { channel: String },
    #[error("channel `{channel}` transport failed: {message}")]
    Transport { channel: String, message: String },
    #[error("channel `{channel}` protocol error: {message}")]
    Protocol { channel: String, message: String },
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
pub enum SchedulerError {
    #[error("invalid cron expression: {expression}: {reason}")]
    InvalidCronExpression { expression: String, reason: String },
    #[error("invalid schedule cadence: {reason}")]
    InvalidCadence { reason: String },
    #[error("schedule not found: {schedule_id}")]
    NotFound { schedule_id: String },
    #[error("schedule store error: {message}")]
    Store { message: String },
    #[error("schedule execution failed: {message}")]
    Execution { message: String },
    #[error("user {user_id} has reached the maximum number of schedules ({max})")]
    LimitExceeded { user_id: String, max: usize },
    #[error("unauthorized: schedule {schedule_id} does not belong to user {user_id}")]
    Unauthorized {
        schedule_id: String,
        user_id: String,
    },
}

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error(transparent)]
    Tool(#[from] ToolError),
    #[error(transparent)]
    Memory(#[from] MemoryError),
    #[error(transparent)]
    Scheduler(#[from] SchedulerError),
    #[error("turn cancelled")]
    Cancelled,
    #[error("turn budget exceeded")]
    BudgetExceeded,
}
