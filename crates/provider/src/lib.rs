use std::env;

use types::{
    ModelCatalog, Provider, ProviderError, ProviderId, ProviderRegistryEntry, ProviderType,
};

mod anthropic;
mod gemini;
mod openai;
mod responses;
mod retry;

pub use anthropic::AnthropicProvider;
pub use gemini::GeminiProvider;
pub use openai::OpenAIProvider;
pub use responses::ResponsesProvider;
pub use retry::{ReliableProvider, RetryPolicy};

#[cfg(test)]
use anthropic::{
    AnthropicEventAction, AnthropicMessagesRequest, AnthropicMessagesResponse,
    AnthropicResponseContentBlock, AnthropicToolCallAccumulator, AnthropicUsage,
    normalize_anthropic_response, parse_anthropic_stream_payload,
};
#[cfg(test)]
use gemini::{
    GeminiCandidate, GeminiContent, GeminiFunctionCall, GeminiGenerateContentRequest,
    GeminiGenerateContentResponse, GeminiPart, GeminiUsageMetadata, normalize_gemini_response,
    parse_gemini_stream_payload,
};
#[cfg(test)]
use openai::{
    OpenAIChatCompletionRequest, OpenAIChatCompletionResponse, OpenAIChatMessageResponse,
    OpenAIChoice, OpenAIResponseFunction, OpenAIResponseToolCall, OpenAIUsage, SseDataParser,
    ToolCallAccumulator, normalize_openai_response, normalize_openai_stream_chunk,
    parse_openai_stream_payload,
};
#[cfg(test)]
use responses::{
    ResponsesApiRequest, ResponsesApiResponse, ResponsesCompletedData, ResponsesEventAction,
    ResponsesFunctionArgsDeltaData, ResponsesOutputBlock, ResponsesOutputContent,
    ResponsesOutputItemAddedData, ResponsesTextDeltaData, ResponsesToolCallAccumulator,
    ResponsesUsage, parse_responses_stream_event,
};
#[cfg(test)]
use types::{Context, FunctionDecl, JsonSchema, Message, MessageRole, Response, ToolCall};

pub(crate) const OPENAI_DEFAULT_BASE_URL: &str = "https://api.openai.com";
pub(crate) const OPENAI_CHAT_COMPLETIONS_PATH: &str = "/v1/chat/completions";
pub(crate) const OPENAI_RESPONSES_PATH: &str = "/v1/responses";
pub(crate) const ANTHROPIC_DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
pub(crate) const ANTHROPIC_MESSAGES_PATH: &str = "/v1/messages";
pub(crate) const ANTHROPIC_VERSION: &str = "2023-06-01";
pub(crate) const DEFAULT_ANTHROPIC_MAX_TOKENS: u32 = 1024;
pub(crate) const DEFAULT_STREAM_BUFFER_SIZE: usize = 64;
pub(crate) const GEMINI_DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com";

/// Resolve an API key for the given provider registry entry.
///
/// Resolution order:
/// 1. Explicit `api_key` from the entry
/// 2. Custom env var specified by `api_key_env`
/// 3. Provider-type-specific default env var (OPENAI_API_KEY, ANTHROPIC_API_KEY, GEMINI_API_KEY)
/// 4. Generic `API_KEY` fallback
pub fn resolve_api_key_for_entry(entry: &ProviderRegistryEntry) -> Option<String> {
    let provider_env = match entry.provider_type {
        ProviderType::Openai | ProviderType::OpenaiResponses => env::var("OPENAI_API_KEY").ok(),
        ProviderType::Anthropic => env::var("ANTHROPIC_API_KEY").ok(),
        ProviderType::Gemini => env::var("GEMINI_API_KEY").ok(),
    };
    let custom_env = entry
        .api_key_env
        .as_ref()
        .and_then(|var| env::var(var).ok());
    resolve_api_key_from_sources(
        entry.api_key.clone(),
        custom_env.or(provider_env),
        env::var("API_KEY").ok(),
    )
}

/// Build a provider from a registry entry, resolving the API key and
/// constructing the appropriate provider implementation.
pub fn build_provider(
    provider_id: ProviderId,
    entry: &ProviderRegistryEntry,
    model_catalog: ModelCatalog,
) -> Result<Box<dyn Provider>, ProviderError> {
    let api_key = resolve_api_key_for_entry(entry).ok_or_else(|| ProviderError::MissingApiKey {
        provider: provider_id.clone(),
    })?;
    let catalog_provider_id = ProviderId::from(entry.effective_catalog_provider());
    let base_url = entry.base_url.clone().unwrap_or_default();
    let extra_headers = entry.extra_headers.clone().unwrap_or_default();
    match entry.provider_type {
        ProviderType::Openai => Ok(Box::new(OpenAIProvider::new(
            provider_id,
            catalog_provider_id,
            api_key,
            base_url,
            extra_headers,
            model_catalog,
        ))),
        ProviderType::Anthropic => Ok(Box::new(AnthropicProvider::new(
            provider_id,
            catalog_provider_id,
            api_key,
            base_url,
            extra_headers,
            model_catalog,
        ))),
        ProviderType::Gemini => Ok(Box::new(GeminiProvider::new(
            provider_id,
            catalog_provider_id,
            api_key,
            base_url,
            extra_headers,
            model_catalog,
        ))),
        ProviderType::OpenaiResponses => Ok(Box::new(ResponsesProvider::new(
            provider_id,
            catalog_provider_id,
            api_key,
            base_url,
            extra_headers,
            model_catalog,
        ))),
    }
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
    if let Ok(parsed) = serde_json::from_str::<gemini::GeminiErrorEnvelope>(body)
        && let Some(error) = parsed.error
        && let Some(message) = error.message.and_then(non_empty)
    {
        return truncate_message(&message);
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
