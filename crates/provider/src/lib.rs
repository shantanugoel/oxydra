use std::env;

mod anthropic;
mod openai;
mod retry;

pub use anthropic::{AnthropicConfig, AnthropicProvider};
pub use openai::{OpenAIConfig, OpenAIProvider};
pub use retry::{ReliableProvider, RetryPolicy};

#[cfg(test)]
use anthropic::{
    AnthropicMessagesRequest, AnthropicMessagesResponse, AnthropicResponseContentBlock,
    AnthropicUsage, normalize_anthropic_response,
};
#[cfg(test)]
use openai::{
    OpenAIChatCompletionRequest, OpenAIChatCompletionResponse, OpenAIChatMessageResponse,
    OpenAIChoice, OpenAIResponseFunction, OpenAIResponseToolCall, OpenAIUsage, SseDataParser,
    ToolCallAccumulator, normalize_openai_response, normalize_openai_stream_chunk,
    parse_openai_stream_payload,
};
#[cfg(test)]
use types::{
    Context, FunctionDecl, JsonSchema, Message, MessageRole, ModelCatalog, ProviderError,
    ProviderId, Response, ToolCall,
};

pub(crate) const OPENAI_PROVIDER_ID: &str = "openai";
pub(crate) const OPENAI_DEFAULT_BASE_URL: &str = "https://api.openai.com";
pub(crate) const OPENAI_CHAT_COMPLETIONS_PATH: &str = "/v1/chat/completions";
pub(crate) const ANTHROPIC_PROVIDER_ID: &str = "anthropic";
pub(crate) const ANTHROPIC_DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
pub(crate) const ANTHROPIC_MESSAGES_PATH: &str = "/v1/messages";
pub(crate) const ANTHROPIC_VERSION: &str = "2023-06-01";
pub(crate) const DEFAULT_ANTHROPIC_MAX_TOKENS: u32 = 1024;
pub(crate) const DEFAULT_STREAM_BUFFER_SIZE: usize = 64;

fn resolve_api_key(explicit_api_key: Option<String>) -> Option<String> {
    resolve_api_key_from_sources(
        explicit_api_key,
        env::var("OPENAI_API_KEY").ok(),
        env::var("API_KEY").ok(),
    )
}

fn resolve_anthropic_api_key(explicit_api_key: Option<String>) -> Option<String> {
    resolve_api_key_from_sources(
        explicit_api_key,
        env::var("ANTHROPIC_API_KEY").ok(),
        env::var("API_KEY").ok(),
    )
}

fn resolve_api_key_from_sources(
    explicit_api_key: Option<String>,
    provider_specific_env_key: Option<String>,
    fallback_env_key: Option<String>,
) -> Option<String> {
    explicit_api_key
        .and_then(non_empty)
        .or_else(|| provider_specific_env_key.and_then(non_empty))
        .or_else(|| fallback_env_key.and_then(non_empty))
}

fn non_empty(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

fn normalize_base_url(base_url: &str) -> String {
    normalize_base_url_or_default(base_url, OPENAI_DEFAULT_BASE_URL)
}

fn normalize_base_url_or_default(base_url: &str, default_base_url: &str) -> String {
    let trimmed = base_url.trim();
    if trimmed.is_empty() {
        default_base_url.to_owned()
    } else {
        trimmed.trim_end_matches('/').to_owned()
    }
}

fn extract_http_error_message(body: &str) -> String {
    if let Ok(parsed) = serde_json::from_str::<openai::OpenAIErrorEnvelope>(body)
        && let Some(message) = non_empty(parsed.error.message)
    {
        return truncate_message(&message);
    }
    if let Ok(parsed) = serde_json::from_str::<anthropic::AnthropicErrorEnvelope>(body) {
        if let Some(error) = parsed.error
            && let Some(message) = error.message.and_then(non_empty)
        {
            return truncate_message(&message);
        }
        if let Some(message) = parsed.message.and_then(non_empty) {
            return truncate_message(&message);
        }
    }
    let trimmed = body.trim();
    if trimmed.is_empty() {
        "empty error response from provider".to_owned()
    } else {
        truncate_message(trimmed)
    }
}

fn truncate_message(message: &str) -> String {
    const MAX_LEN: usize = 512;
    if message.chars().count() <= MAX_LEN {
        return message.to_owned();
    }
    let prefix = message.chars().take(MAX_LEN).collect::<String>();
    format!("{prefix}...")
}

#[cfg(test)]
mod tests;
