use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;
use types::{
    Context, FunctionDecl, JsonSchema, Message, MessageRole, ModelCatalog, Provider, ProviderError,
    ProviderId, ProviderStream, Response, StreamItem, ToolCall, ToolCallDelta, UsageUpdate,
};

use crate::{
    ANTHROPIC_DEFAULT_BASE_URL, ANTHROPIC_MESSAGES_PATH, ANTHROPIC_PROVIDER_ID, ANTHROPIC_VERSION,
    DEFAULT_ANTHROPIC_MAX_TOKENS, DEFAULT_STREAM_BUFFER_SIZE, extract_http_error_message,
    non_empty, normalize_base_url_or_default, resolve_anthropic_api_key,
};

#[derive(Debug, Clone)]
pub struct AnthropicConfig {
    pub api_key: Option<String>,
    pub base_url: String,
    pub max_tokens: u32,
}

impl Default for AnthropicConfig {
    fn default() -> Self {
        Self {
            api_key: None,
            base_url: ANTHROPIC_DEFAULT_BASE_URL.to_owned(),
            max_tokens: DEFAULT_ANTHROPIC_MAX_TOKENS,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AnthropicProvider {
    client: Client,
    provider_id: ProviderId,
    model_catalog: ModelCatalog,
    base_url: String,
    api_key: String,
    max_tokens: u32,
}

impl AnthropicProvider {
    pub fn new(config: AnthropicConfig) -> Result<Self, ProviderError> {
        let model_catalog = ModelCatalog::from_pinned_snapshot()?;
        Self::with_catalog(config, model_catalog)
    }

    pub fn with_catalog(
        config: AnthropicConfig,
        model_catalog: ModelCatalog,
    ) -> Result<Self, ProviderError> {
        let provider_id = ProviderId::from(ANTHROPIC_PROVIDER_ID);
        let api_key = resolve_anthropic_api_key(config.api_key).ok_or_else(|| {
            ProviderError::MissingApiKey {
                provider: provider_id.clone(),
            }
        })?;

        Ok(Self {
            client: Client::new(),
            provider_id,
            model_catalog,
            base_url: normalize_base_url_or_default(&config.base_url, ANTHROPIC_DEFAULT_BASE_URL),
            api_key,
            max_tokens: config.max_tokens.max(1),
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

    fn messages_url(&self) -> String {
        format!("{}{}", self.base_url, ANTHROPIC_MESSAGES_PATH)
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
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
            "sending Anthropic messages request"
        );

        let request = AnthropicMessagesRequest::from_context(context, self.max_tokens)?;
        let http_response = self
            .client
            .post(self.messages_url())
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
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

        let response: AnthropicMessagesResponse =
            http_response
                .json()
                .await
                .map_err(|error| ProviderError::ResponseParse {
                    provider: self.provider_id.clone(),
                    message: error.to_string(),
                })?;
        normalize_anthropic_response(response, &self.provider_id)
    }

    async fn stream(
        &self,
        context: &Context,
        buffer_size: usize,
    ) -> Result<ProviderStream, ProviderError> {
        self.validate_context(context)?;
        let Response {
            message,
            tool_calls,
            finish_reason,
            usage,
        } = self.complete(context).await?;

        let channel_size = if buffer_size == 0 {
            DEFAULT_STREAM_BUFFER_SIZE
        } else {
            buffer_size
        };
        let (sender, receiver) = mpsc::channel(channel_size);

        if let Some(content) = message.content.filter(|content| !content.is_empty()) {
            let _ = sender.send(Ok(StreamItem::Text(content))).await;
        }

        for (index, tool_call) in tool_calls.into_iter().enumerate() {
            let arguments = serde_json::to_string(&tool_call.arguments).map_err(|error| {
                ProviderError::ResponseParse {
                    provider: self.provider_id.clone(),
                    message: format!(
                        "failed to serialize Anthropic tool call arguments for `{}`: {error}",
                        tool_call.id
                    ),
                }
            })?;
            let _ = sender
                .send(Ok(StreamItem::ToolCallDelta(ToolCallDelta {
                    index,
                    id: Some(tool_call.id),
                    name: Some(tool_call.name),
                    arguments: Some(arguments),
                })))
                .await;
        }

        if let Some(usage) = usage {
            let _ = sender.send(Ok(StreamItem::UsageUpdate(usage))).await;
        }
        if let Some(finish_reason) = finish_reason {
            let _ = sender
                .send(Ok(StreamItem::FinishReason(finish_reason)))
                .await;
        }
        drop(sender);

        Ok(receiver)
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct AnthropicMessagesRequest {
    model: String,
    max_tokens: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    messages: Vec<AnthropicMessageRequest>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tools: Vec<AnthropicRequestToolDefinition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tool_choice: Option<AnthropicToolChoice>,
}

impl AnthropicMessagesRequest {
    pub(crate) fn from_context(context: &Context, max_tokens: u32) -> Result<Self, ProviderError> {
        let mut system_chunks = Vec::new();
        let mut messages = Vec::new();

        for message in &context.messages {
            match message.role {
                MessageRole::System => {
                    if let Some(content) = message.content.clone().and_then(non_empty) {
                        system_chunks.push(content);
                    }
                }
                MessageRole::User | MessageRole::Assistant => {
                    let mut content = Vec::new();
                    if let Some(text) = message.content.clone().and_then(non_empty) {
                        content.push(AnthropicRequestContentBlock::Text { text });
                    }
                    for tool_call in &message.tool_calls {
                        content.push(AnthropicRequestContentBlock::ToolUse {
                            id: tool_call.id.clone(),
                            name: tool_call.name.clone(),
                            input: tool_call.arguments.clone(),
                        });
                    }
                    if content.is_empty() {
                        return Err(ProviderError::RequestFailed {
                            provider: context.provider.clone(),
                            message: format!(
                                "cannot convert {:?} message with empty content and no tool calls to Anthropic payload",
                                message.role
                            ),
                        });
                    }
                    messages.push(AnthropicMessageRequest {
                        role: message_role_to_anthropic_role(&message.role).to_owned(),
                        content,
                    });
                }
                MessageRole::Tool => {
                    let tool_use_id = message.tool_call_id.clone().ok_or_else(|| {
                        ProviderError::RequestFailed {
                            provider: context.provider.clone(),
                            message: "tool message is missing tool_call_id for Anthropic payload"
                                .to_owned(),
                        }
                    })?;
                    messages.push(AnthropicMessageRequest {
                        role: "user".to_owned(),
                        content: vec![AnthropicRequestContentBlock::ToolResult {
                            tool_use_id,
                            content: message.content.clone().unwrap_or_default(),
                        }],
                    });
                }
            }
        }

        let tools = context
            .tools
            .iter()
            .map(AnthropicRequestToolDefinition::from)
            .collect::<Vec<_>>();
        if messages.is_empty() {
            return Err(ProviderError::RequestFailed {
                provider: context.provider.clone(),
                message: "Anthropic request requires at least one non-system message".to_owned(),
            });
        }
        let tool_choice = (!tools.is_empty()).then_some(AnthropicToolChoice::auto());
        let system = (!system_chunks.is_empty()).then(|| system_chunks.join("\n\n"));

        Ok(Self {
            model: context.model.0.clone(),
            max_tokens: max_tokens.max(1),
            system,
            messages,
            tools,
            tool_choice,
        })
    }
}

#[derive(Debug, Serialize)]
struct AnthropicMessageRequest {
    role: String,
    content: Vec<AnthropicRequestContentBlock>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicRequestContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
    },
}

#[derive(Debug, Serialize)]
struct AnthropicRequestToolDefinition {
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    input_schema: JsonSchema,
}

impl From<&FunctionDecl> for AnthropicRequestToolDefinition {
    fn from(value: &FunctionDecl) -> Self {
        Self {
            name: value.name.clone(),
            description: value.description.clone(),
            input_schema: value.parameters.clone(),
        }
    }
}

#[derive(Debug, Serialize)]
struct AnthropicToolChoice {
    #[serde(rename = "type")]
    kind: String,
}

impl AnthropicToolChoice {
    fn auto() -> Self {
        Self {
            kind: "auto".to_owned(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct AnthropicMessagesResponse {
    pub(crate) role: String,
    pub(crate) content: Vec<AnthropicResponseContentBlock>,
    #[serde(default)]
    pub(crate) stop_reason: Option<String>,
    #[serde(default)]
    pub(crate) usage: Option<AnthropicUsage>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AnthropicResponseContentBlock {
    #[serde(rename = "type")]
    pub(crate) kind: String,
    #[serde(default)]
    pub(crate) text: Option<String>,
    #[serde(default)]
    pub(crate) id: Option<String>,
    #[serde(default)]
    pub(crate) name: Option<String>,
    #[serde(default)]
    pub(crate) input: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AnthropicUsage {
    #[serde(default)]
    pub(crate) input_tokens: Option<u64>,
    #[serde(default)]
    pub(crate) output_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AnthropicErrorEnvelope {
    #[serde(default)]
    pub(crate) error: Option<AnthropicErrorBody>,
    #[serde(default)]
    pub(crate) message: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AnthropicErrorBody {
    #[serde(default)]
    pub(crate) message: Option<String>,
}

fn message_role_to_anthropic_role(role: &MessageRole) -> &'static str {
    match role {
        MessageRole::User | MessageRole::Tool => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::System => "user",
    }
}

fn anthropic_role_to_message_role(role: &str) -> Option<MessageRole> {
    match role {
        "user" => Some(MessageRole::User),
        "assistant" => Some(MessageRole::Assistant),
        _ => None,
    }
}

pub(crate) fn normalize_anthropic_response(
    response: AnthropicMessagesResponse,
    provider: &ProviderId,
) -> Result<Response, ProviderError> {
    let role = anthropic_role_to_message_role(&response.role).ok_or_else(|| {
        ProviderError::ResponseParse {
            provider: provider.clone(),
            message: format!("unsupported Anthropic role `{}`", response.role),
        }
    })?;

    let mut text_chunks = Vec::new();
    let mut tool_calls = Vec::new();
    for block in response.content {
        match block.kind.as_str() {
            "text" => {
                if let Some(text) = block.text.and_then(non_empty) {
                    text_chunks.push(text);
                }
            }
            "tool_use" => {
                let id = block.id.ok_or_else(|| ProviderError::ResponseParse {
                    provider: provider.clone(),
                    message: "Anthropic tool_use block missing id".to_owned(),
                })?;
                let name = block.name.ok_or_else(|| ProviderError::ResponseParse {
                    provider: provider.clone(),
                    message: format!("Anthropic tool_use block `{id}` missing name"),
                })?;
                let arguments = block.input.ok_or_else(|| ProviderError::ResponseParse {
                    provider: provider.clone(),
                    message: format!("Anthropic tool_use block `{id}` missing input"),
                })?;
                tool_calls.push(ToolCall {
                    id,
                    name,
                    arguments,
                });
            }
            _ => {}
        }
    }

    let usage = response.usage.map(|usage| {
        let total_tokens = match (usage.input_tokens, usage.output_tokens) {
            (Some(input_tokens), Some(output_tokens)) => {
                Some(input_tokens.saturating_add(output_tokens))
            }
            _ => None,
        };
        UsageUpdate {
            prompt_tokens: usage.input_tokens,
            completion_tokens: usage.output_tokens,
            total_tokens,
        }
    });

    let message = Message {
        role,
        content: (!text_chunks.is_empty()).then(|| text_chunks.join("\n")),
        tool_calls: tool_calls.clone(),
        tool_call_id: None,
    };

    Ok(Response {
        tool_calls,
        message,
        finish_reason: response.stop_reason,
        usage,
    })
}
