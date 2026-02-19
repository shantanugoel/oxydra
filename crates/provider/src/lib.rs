use std::env;

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use types::{
    Context, Message, MessageRole, ModelCatalog, Provider, ProviderError, ProviderId, Response,
    ToolCall,
};

const OPENAI_PROVIDER_ID: &str = "openai";
const OPENAI_DEFAULT_BASE_URL: &str = "https://api.openai.com";
const OPENAI_CHAT_COMPLETIONS_PATH: &str = "/v1/chat/completions";

#[derive(Debug, Clone)]
pub struct OpenAIConfig {
    pub api_key: Option<String>,
    pub base_url: String,
}

impl Default for OpenAIConfig {
    fn default() -> Self {
        Self {
            api_key: None,
            base_url: OPENAI_DEFAULT_BASE_URL.to_owned(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct OpenAIProvider {
    client: Client,
    provider_id: ProviderId,
    model_catalog: ModelCatalog,
    base_url: String,
    api_key: String,
}

impl OpenAIProvider {
    pub fn new(config: OpenAIConfig) -> Result<Self, ProviderError> {
        let model_catalog = ModelCatalog::from_pinned_snapshot()?;
        Self::with_catalog(config, model_catalog)
    }

    pub fn with_catalog(
        config: OpenAIConfig,
        model_catalog: ModelCatalog,
    ) -> Result<Self, ProviderError> {
        let provider_id = ProviderId::from(OPENAI_PROVIDER_ID);
        let api_key =
            resolve_api_key(config.api_key).ok_or_else(|| ProviderError::MissingApiKey {
                provider: provider_id.clone(),
            })?;
        Ok(Self {
            client: Client::new(),
            provider_id,
            model_catalog,
            base_url: normalize_base_url(&config.base_url),
            api_key,
        })
    }

    fn validate_context(&self, context: &Context) -> Result<(), ProviderError> {
        if context.provider != self.provider_id {
            return Err(ProviderError::RequestFailed {
                provider: self.provider_id.clone(),
                message: format!(
                    "context provider `{}` does not match provider `{}`",
                    context.provider, self.provider_id
                ),
            });
        }
        self.model_catalog
            .validate(&self.provider_id, &context.model)?;
        Ok(())
    }

    fn chat_completions_url(&self) -> String {
        format!("{}{}", self.base_url, OPENAI_CHAT_COMPLETIONS_PATH)
    }
}

#[async_trait]
impl Provider for OpenAIProvider {
    fn provider_id(&self) -> &ProviderId {
        &self.provider_id
    }

    fn model_catalog(&self) -> &ModelCatalog {
        &self.model_catalog
    }

    async fn complete(&self, context: &Context) -> Result<Response, ProviderError> {
        self.validate_context(context)?;
        tracing::debug!(
            provider = %self.provider_id,
            model = %context.model,
            "sending OpenAI chat completion request"
        );

        let request = OpenAIChatCompletionRequest::from_context(context)?;
        let http_response = self
            .client
            .post(self.chat_completions_url())
            .bearer_auth(&self.api_key)
            .json(&request)
            .send()
            .await
            .map_err(|error| ProviderError::Transport {
                provider: self.provider_id.clone(),
                message: error.to_string(),
            })?;

        if !http_response.status().is_success() {
            let status = http_response.status().as_u16();
            let body = match http_response.text().await {
                Ok(text) => text,
                Err(error) => format!("unable to read error body: {error}"),
            };
            return Err(ProviderError::HttpStatus {
                provider: self.provider_id.clone(),
                status,
                message: extract_http_error_message(&body),
            });
        }

        let response: OpenAIChatCompletionResponse =
            http_response
                .json()
                .await
                .map_err(|error| ProviderError::ResponseParse {
                    provider: self.provider_id.clone(),
                    message: error.to_string(),
                })?;
        normalize_openai_response(response, &self.provider_id)
    }
}

#[derive(Debug, Serialize)]
struct OpenAIChatCompletionRequest {
    model: String,
    messages: Vec<OpenAIChatMessageRequest>,
    stream: bool,
}

impl OpenAIChatCompletionRequest {
    fn from_context(context: &Context) -> Result<Self, ProviderError> {
        let messages = context
            .messages
            .iter()
            .map(OpenAIChatMessageRequest::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            model: context.model.0.clone(),
            messages,
            stream: false,
        })
    }
}

#[derive(Debug, Serialize)]
struct OpenAIChatMessageRequest {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<OpenAIRequestToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

impl TryFrom<&Message> for OpenAIChatMessageRequest {
    type Error = ProviderError;

    fn try_from(value: &Message) -> Result<Self, Self::Error> {
        let tool_calls = value
            .tool_calls
            .iter()
            .map(OpenAIRequestToolCall::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            role: message_role_to_openai_role(&value.role).to_owned(),
            content: value.content.clone(),
            tool_calls,
            tool_call_id: value.tool_call_id.clone(),
        })
    }
}

#[derive(Debug, Serialize)]
struct OpenAIRequestToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    function: OpenAIRequestFunction,
}

impl TryFrom<&ToolCall> for OpenAIRequestToolCall {
    type Error = ProviderError;

    fn try_from(value: &ToolCall) -> Result<Self, Self::Error> {
        Ok(Self {
            id: value.id.clone(),
            kind: "function".to_owned(),
            function: OpenAIRequestFunction {
                name: value.name.clone(),
                arguments: serde_json::to_string(&value.arguments)?,
            },
        })
    }
}

#[derive(Debug, Serialize)]
struct OpenAIRequestFunction {
    name: String,
    arguments: String,
}

#[derive(Debug, Deserialize)]
struct OpenAIChatCompletionResponse {
    choices: Vec<OpenAIChoice>,
}

#[derive(Debug, Deserialize)]
struct OpenAIChoice {
    message: OpenAIChatMessageResponse,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAIChatMessageResponse {
    role: String,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<OpenAIResponseToolCall>,
}

#[derive(Debug, Deserialize)]
struct OpenAIResponseToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    function: OpenAIResponseFunction,
}

#[derive(Debug, Deserialize)]
struct OpenAIResponseFunction {
    name: String,
    arguments: String,
}

#[derive(Debug, Deserialize)]
struct OpenAIErrorEnvelope {
    error: OpenAIErrorBody,
}

#[derive(Debug, Deserialize)]
struct OpenAIErrorBody {
    message: String,
}

fn resolve_api_key(explicit_api_key: Option<String>) -> Option<String> {
    resolve_api_key_from_sources(
        explicit_api_key,
        env::var("OPENAI_API_KEY").ok(),
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
    let trimmed = base_url.trim();
    if trimmed.is_empty() {
        OPENAI_DEFAULT_BASE_URL.to_owned()
    } else {
        trimmed.trim_end_matches('/').to_owned()
    }
}

fn message_role_to_openai_role(role: &MessageRole) -> &'static str {
    match role {
        MessageRole::System => "system",
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::Tool => "tool",
    }
}

fn openai_role_to_message_role(role: &str) -> Option<MessageRole> {
    match role {
        "system" => Some(MessageRole::System),
        "user" => Some(MessageRole::User),
        "assistant" => Some(MessageRole::Assistant),
        "tool" => Some(MessageRole::Tool),
        _ => None,
    }
}

fn normalize_openai_response(
    response: OpenAIChatCompletionResponse,
    provider: &ProviderId,
) -> Result<Response, ProviderError> {
    let choice =
        response
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| ProviderError::ResponseParse {
                provider: provider.clone(),
                message: "OpenAI response did not contain any choices".to_owned(),
            })?;
    let message = normalize_openai_message(choice.message, provider)?;
    Ok(Response {
        tool_calls: message.tool_calls.clone(),
        message,
        finish_reason: choice.finish_reason,
    })
}

fn normalize_openai_message(
    message: OpenAIChatMessageResponse,
    provider: &ProviderId,
) -> Result<Message, ProviderError> {
    let role =
        openai_role_to_message_role(&message.role).ok_or_else(|| ProviderError::ResponseParse {
            provider: provider.clone(),
            message: format!("unsupported OpenAI role `{}`", message.role),
        })?;
    let tool_calls =
        message
            .tool_calls
            .into_iter()
            .map(|tool_call| {
                if tool_call.kind != "function" {
                    return Err(ProviderError::ResponseParse {
                        provider: provider.clone(),
                        message: format!("unsupported OpenAI tool call type `{}`", tool_call.kind),
                    });
                }
                let arguments = serde_json::from_str::<Value>(&tool_call.function.arguments)
                    .map_err(|error| ProviderError::ResponseParse {
                        provider: provider.clone(),
                        message: format!(
                            "invalid OpenAI tool call arguments for `{}`: {error}",
                            tool_call.id
                        ),
                    })?;
                Ok(ToolCall {
                    id: tool_call.id,
                    name: tool_call.function.name,
                    arguments,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
    Ok(Message {
        role,
        content: message.content,
        tool_calls,
        tool_call_id: None,
    })
}

fn extract_http_error_message(body: &str) -> String {
    if let Ok(parsed) = serde_json::from_str::<OpenAIErrorEnvelope>(body)
        && let Some(message) = non_empty(parsed.error.message)
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
mod tests {
    use serde_json::json;
    use types::{ModelDescriptor, ModelId, ProviderCaps};

    use super::*;

    #[test]
    fn api_key_resolution_uses_expected_precedence() {
        let resolved = resolve_api_key_from_sources(
            Some("explicit".to_owned()),
            Some("provider".to_owned()),
            Some("fallback".to_owned()),
        );
        assert_eq!(resolved.as_deref(), Some("explicit"));

        let resolved = resolve_api_key_from_sources(
            None,
            Some("provider".to_owned()),
            Some("fallback".to_owned()),
        );
        assert_eq!(resolved.as_deref(), Some("provider"));

        let resolved = resolve_api_key_from_sources(None, None, Some("fallback".to_owned()));
        assert_eq!(resolved.as_deref(), Some("fallback"));

        let resolved = resolve_api_key_from_sources(
            Some("   ".to_owned()),
            Some("provider".to_owned()),
            Some("fallback".to_owned()),
        );
        assert_eq!(resolved.as_deref(), Some("provider"));
    }

    #[test]
    fn request_normalization_maps_messages_and_tools() {
        let context = Context {
            provider: ProviderId::from("openai"),
            model: ModelId::from("gpt-4o-mini"),
            messages: vec![
                Message {
                    role: MessageRole::User,
                    content: Some("List project files".to_owned()),
                    tool_calls: vec![],
                    tool_call_id: None,
                },
                Message {
                    role: MessageRole::Assistant,
                    content: None,
                    tool_calls: vec![ToolCall {
                        id: "call_1".to_owned(),
                        name: "read_file".to_owned(),
                        arguments: json!({"path": "Cargo.toml"}),
                    }],
                    tool_call_id: None,
                },
            ],
        };

        let request =
            OpenAIChatCompletionRequest::from_context(&context).expect("request should normalize");
        let request_json = serde_json::to_value(request).expect("request should serialize");
        assert_eq!(request_json["model"], "gpt-4o-mini");
        assert_eq!(request_json["messages"][0]["role"], "user");
        assert_eq!(
            request_json["messages"][1]["tool_calls"][0]["function"]["name"],
            "read_file"
        );
        assert_eq!(
            request_json["messages"][1]["tool_calls"][0]["function"]["arguments"],
            "{\"path\":\"Cargo.toml\"}"
        );
    }

    #[test]
    fn response_normalization_maps_message_and_finish_reason() {
        let response = OpenAIChatCompletionResponse {
            choices: vec![OpenAIChoice {
                message: OpenAIChatMessageResponse {
                    role: "assistant".to_owned(),
                    content: Some("Done".to_owned()),
                    tool_calls: vec![OpenAIResponseToolCall {
                        id: "call_1".to_owned(),
                        kind: "function".to_owned(),
                        function: OpenAIResponseFunction {
                            name: "read_file".to_owned(),
                            arguments: "{\"path\":\"Cargo.toml\"}".to_owned(),
                        },
                    }],
                },
                finish_reason: Some("tool_calls".to_owned()),
            }],
        };

        let normalized =
            normalize_openai_response(response, &ProviderId::from("openai")).expect("should parse");
        assert_eq!(normalized.message.role, MessageRole::Assistant);
        assert_eq!(normalized.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(normalized.tool_calls.len(), 1);
        assert_eq!(normalized.tool_calls[0].name, "read_file");
        assert_eq!(
            normalized.tool_calls[0].arguments,
            json!({"path": "Cargo.toml"})
        );
    }

    #[test]
    fn unknown_models_are_rejected_before_network_request() {
        let provider = OpenAIProvider::with_catalog(
            OpenAIConfig {
                api_key: Some("test-key".to_owned()),
                base_url: OPENAI_DEFAULT_BASE_URL.to_owned(),
            },
            test_model_catalog(),
        )
        .expect("provider should initialize");

        let context = Context {
            provider: ProviderId::from("openai"),
            model: ModelId::from("unknown-model"),
            messages: vec![],
        };

        let validation = provider.validate_context(&context);
        assert!(matches!(
            validation,
            Err(ProviderError::UnknownModel { .. })
        ));
    }

    fn test_model_catalog() -> ModelCatalog {
        ModelCatalog::new(vec![ModelDescriptor {
            provider: ProviderId::from("openai"),
            model: ModelId::from("gpt-4o-mini"),
            display_name: Some("GPT-4o mini".to_owned()),
            caps: ProviderCaps::default(),
            deprecated: false,
        }])
    }
}
