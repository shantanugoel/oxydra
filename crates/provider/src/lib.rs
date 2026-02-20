use std::{collections::HashMap, env};

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;
use types::{
    Context, FunctionDecl, JsonSchema, Message, MessageRole, ModelCatalog, Provider, ProviderError,
    ProviderId, ProviderStream, Response, StreamItem, ToolCall, ToolCallDelta, UsageUpdate,
};

const OPENAI_PROVIDER_ID: &str = "openai";
const OPENAI_DEFAULT_BASE_URL: &str = "https://api.openai.com";
const OPENAI_CHAT_COMPLETIONS_PATH: &str = "/v1/chat/completions";
const DEFAULT_STREAM_BUFFER_SIZE: usize = 64;

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

    async fn stream(
        &self,
        context: &Context,
        buffer_size: usize,
    ) -> Result<ProviderStream, ProviderError> {
        self.validate_context(context)?;
        tracing::debug!(
            provider = %self.provider_id,
            model = %context.model,
            "sending OpenAI chat completion streaming request"
        );

        let request = OpenAIChatCompletionRequest::from_stream_context(context)?;
        let mut http_response = self
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

        let channel_size = if buffer_size == 0 {
            DEFAULT_STREAM_BUFFER_SIZE
        } else {
            buffer_size
        };
        let (sender, receiver) = mpsc::channel(channel_size);
        let provider = self.provider_id.clone();
        tokio::spawn(async move {
            let mut parser = SseDataParser::default();
            let mut tool_call_accumulator = ToolCallAccumulator::default();
            loop {
                let chunk = match http_response.chunk().await {
                    Ok(chunk) => chunk,
                    Err(error) => {
                        send_connection_lost(
                            &sender,
                            format!("OpenAI stream transport dropped: {error}"),
                        )
                        .await;
                        return;
                    }
                };
                let Some(chunk) = chunk else {
                    break;
                };

                let payloads = match parser.push_chunk(&chunk) {
                    Ok(payloads) => payloads,
                    Err(message) => {
                        send_stream_error(
                            &sender,
                            ProviderError::ResponseParse {
                                provider: provider.clone(),
                                message,
                            },
                        )
                        .await;
                        return;
                    }
                };

                match emit_stream_payloads(payloads, &sender, &provider, &mut tool_call_accumulator)
                    .await
                {
                    Ok(true) => return,
                    Ok(false) => {}
                    Err(()) => return,
                }
            }

            let payloads = match parser.finish() {
                Ok(payloads) => payloads,
                Err(message) => {
                    send_stream_error(
                        &sender,
                        ProviderError::ResponseParse {
                            provider: provider.clone(),
                            message,
                        },
                    )
                    .await;
                    return;
                }
            };

            match emit_stream_payloads(payloads, &sender, &provider, &mut tool_call_accumulator)
                .await
            {
                Ok(true) => {}
                Ok(false) => {
                    send_connection_lost(
                        &sender,
                        "OpenAI stream ended before [DONE] sentinel".to_owned(),
                    )
                    .await;
                }
                Err(()) => {}
            }
        });

        Ok(receiver)
    }
}

#[derive(Debug, Serialize)]
struct OpenAIChatCompletionRequest {
    model: String,
    messages: Vec<OpenAIChatMessageRequest>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tools: Vec<OpenAIRequestToolDefinition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tool_choice: Option<String>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<OpenAIStreamOptions>,
}

impl OpenAIChatCompletionRequest {
    fn from_context(context: &Context) -> Result<Self, ProviderError> {
        Self::from_context_with_stream(context, false)
    }

    fn from_stream_context(context: &Context) -> Result<Self, ProviderError> {
        Self::from_context_with_stream(context, true)
    }

    fn from_context_with_stream(context: &Context, stream: bool) -> Result<Self, ProviderError> {
        let messages = context
            .messages
            .iter()
            .map(OpenAIChatMessageRequest::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        let tools = context
            .tools
            .iter()
            .map(OpenAIRequestToolDefinition::from)
            .collect::<Vec<_>>();
        let tool_choice = (!tools.is_empty()).then_some("auto".to_owned());
        Ok(Self {
            model: context.model.0.clone(),
            messages,
            tools,
            tool_choice,
            stream,
            stream_options: stream.then_some(OpenAIStreamOptions {
                include_usage: true,
            }),
        })
    }
}

#[derive(Debug, Serialize)]
struct OpenAIStreamOptions {
    include_usage: bool,
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
struct OpenAIRequestToolDefinition {
    #[serde(rename = "type")]
    kind: String,
    function: OpenAIRequestFunctionDecl,
}

impl From<&FunctionDecl> for OpenAIRequestToolDefinition {
    fn from(value: &FunctionDecl) -> Self {
        Self {
            kind: "function".to_owned(),
            function: OpenAIRequestFunctionDecl {
                name: value.name.clone(),
                description: value.description.clone(),
                parameters: value.parameters.clone(),
            },
        }
    }
}

#[derive(Debug, Serialize)]
struct OpenAIRequestFunctionDecl {
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    parameters: JsonSchema,
}

#[derive(Debug, Serialize)]
struct OpenAIRequestFunction {
    name: String,
    arguments: String,
}

#[derive(Debug, Deserialize)]
struct OpenAIChatCompletionResponse {
    choices: Vec<OpenAIChoice>,
    #[serde(default)]
    usage: Option<OpenAIUsage>,
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
struct OpenAIChatCompletionStreamChunk {
    #[serde(default)]
    choices: Vec<OpenAIStreamChoice>,
    #[serde(default)]
    usage: Option<OpenAIUsage>,
}

#[derive(Debug, Deserialize)]
struct OpenAIStreamChoice {
    #[serde(default)]
    delta: OpenAIStreamDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct OpenAIStreamDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    tool_calls: Vec<OpenAIStreamToolCallDelta>,
}

#[derive(Debug, Deserialize)]
struct OpenAIStreamToolCallDelta {
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<OpenAIStreamFunctionDelta>,
}

#[derive(Debug, Deserialize, Default)]
struct OpenAIStreamFunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAIUsage {
    #[serde(default)]
    prompt_tokens: Option<u64>,
    #[serde(default)]
    completion_tokens: Option<u64>,
    #[serde(default)]
    total_tokens: Option<u64>,
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
    let OpenAIChatCompletionResponse { choices, usage } = response;
    let choice = choices
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
        usage: usage.map(|usage| UsageUpdate {
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            total_tokens: usage.total_tokens,
        }),
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

async fn emit_stream_payloads(
    payloads: Vec<String>,
    sender: &mpsc::Sender<Result<StreamItem, ProviderError>>,
    provider: &ProviderId,
    tool_call_accumulator: &mut ToolCallAccumulator,
) -> Result<bool, ()> {
    for payload in payloads {
        match parse_openai_stream_payload(&payload, provider) {
            Ok(Some(chunk)) => {
                for item in normalize_openai_stream_chunk(chunk, tool_call_accumulator) {
                    if sender.send(Ok(item)).await.is_err() {
                        return Err(());
                    }
                }
            }
            Ok(None) => return Ok(true),
            Err(error) => {
                send_stream_error(sender, error).await;
                return Err(());
            }
        }
    }
    Ok(false)
}

async fn send_stream_error(
    sender: &mpsc::Sender<Result<StreamItem, ProviderError>>,
    error: ProviderError,
) {
    let _ = sender.send(Err(error)).await;
}

async fn send_connection_lost(
    sender: &mpsc::Sender<Result<StreamItem, ProviderError>>,
    message: String,
) {
    let _ = sender.send(Ok(StreamItem::ConnectionLost(message))).await;
}

fn parse_openai_stream_payload(
    payload: &str,
    provider: &ProviderId,
) -> Result<Option<OpenAIChatCompletionStreamChunk>, ProviderError> {
    let trimmed = payload.trim();
    if trimmed.is_empty() {
        return Ok(Some(OpenAIChatCompletionStreamChunk {
            choices: Vec::new(),
            usage: None,
        }));
    }
    if trimmed == "[DONE]" {
        return Ok(None);
    }
    let chunk: OpenAIChatCompletionStreamChunk =
        serde_json::from_str(trimmed).map_err(|error| ProviderError::ResponseParse {
            provider: provider.clone(),
            message: format!("failed to parse OpenAI streaming payload: {error}"),
        })?;
    Ok(Some(chunk))
}

fn normalize_openai_stream_chunk(
    chunk: OpenAIChatCompletionStreamChunk,
    tool_call_accumulator: &mut ToolCallAccumulator,
) -> Vec<StreamItem> {
    let mut items = Vec::new();

    if let Some(usage) = chunk.usage {
        items.push(StreamItem::UsageUpdate(UsageUpdate {
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            total_tokens: usage.total_tokens,
        }));
    }

    for choice in chunk.choices {
        if let Some(content) = choice.delta.content.filter(|content| !content.is_empty()) {
            items.push(StreamItem::Text(content));
        }

        if let Some(reasoning) = choice
            .delta
            .reasoning
            .filter(|reasoning| !reasoning.is_empty())
        {
            items.push(StreamItem::ReasoningDelta(reasoning));
        }

        for tool_call in choice.delta.tool_calls {
            let function = tool_call.function.unwrap_or_default();
            if tool_call.id.is_none() && function.name.is_none() && function.arguments.is_none() {
                continue;
            }
            items.push(StreamItem::ToolCallDelta(tool_call_accumulator.merge(
                tool_call.index,
                tool_call.id,
                function,
            )));
        }

        if let Some(finish_reason) = choice
            .finish_reason
            .filter(|finish_reason| !finish_reason.is_empty())
        {
            items.push(StreamItem::FinishReason(finish_reason));
        }
    }

    items
}

#[derive(Debug, Default)]
struct ToolCallAccumulator {
    by_index: HashMap<usize, ToolCallAccumulatorEntry>,
}

#[derive(Debug, Default)]
struct ToolCallAccumulatorEntry {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

impl ToolCallAccumulator {
    fn merge(
        &mut self,
        index: usize,
        id: Option<String>,
        function: OpenAIStreamFunctionDelta,
    ) -> ToolCallDelta {
        let entry = self.by_index.entry(index).or_default();
        if let Some(id) = id {
            entry.id = Some(id);
        }
        if let Some(name) = function.name {
            entry.name = Some(name);
        }
        if let Some(arguments) = function.arguments {
            entry.arguments.push_str(&arguments);
        }

        ToolCallDelta {
            index,
            id: entry.id.clone(),
            name: entry.name.clone(),
            arguments: (!entry.arguments.is_empty()).then(|| entry.arguments.clone()),
        }
    }
}

#[derive(Debug, Default)]
struct SseDataParser {
    line_buffer: Vec<u8>,
    data_lines: Vec<String>,
}

impl SseDataParser {
    fn push_chunk(&mut self, chunk: &[u8]) -> Result<Vec<String>, String> {
        let mut payloads = Vec::new();
        for byte in chunk {
            if *byte == b'\n' {
                let line = std::mem::take(&mut self.line_buffer);
                self.process_line(&line, &mut payloads)?;
            } else {
                self.line_buffer.push(*byte);
            }
        }
        Ok(payloads)
    }

    fn finish(mut self) -> Result<Vec<String>, String> {
        let mut payloads = Vec::new();
        if !self.line_buffer.is_empty() {
            let line = std::mem::take(&mut self.line_buffer);
            self.process_line(&line, &mut payloads)?;
        }
        self.flush_event(&mut payloads);
        Ok(payloads)
    }

    fn process_line(&mut self, line: &[u8], payloads: &mut Vec<String>) -> Result<(), String> {
        let line = if line.ends_with(b"\r") {
            &line[..line.len() - 1]
        } else {
            line
        };

        if line.is_empty() {
            self.flush_event(payloads);
            return Ok(());
        }

        if line.starts_with(b":") {
            return Ok(());
        }

        if let Some(mut data) = line.strip_prefix(b"data:") {
            if data.starts_with(b" ") {
                data = &data[1..];
            }
            let data = std::str::from_utf8(data)
                .map_err(|error| format!("invalid UTF-8 in SSE data line: {error}"))?;
            self.data_lines.push(data.to_owned());
        }

        Ok(())
    }

    fn flush_event(&mut self, payloads: &mut Vec<String>) {
        if !self.data_lines.is_empty() {
            payloads.push(self.data_lines.join("\n"));
            self.data_lines.clear();
        }
    }
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
    use std::{
        io::{Read, Write},
        net::TcpListener,
    };

    use insta::assert_json_snapshot;
    use serde_json::json;
    use types::{
        ModelDescriptor, ModelId, Provider, ProviderCaps, StreamItem, ToolCallDelta, UsageUpdate,
    };

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

        let resolved = resolve_api_key_from_sources(None, None, None);
        assert!(resolved.is_none());
    }

    #[test]
    fn default_openai_config_uses_openai_base_url() {
        assert_eq!(OpenAIConfig::default().base_url, OPENAI_DEFAULT_BASE_URL);
    }

    #[test]
    fn request_normalization_maps_messages_and_tools() {
        let context = Context {
            provider: ProviderId::from("openai"),
            model: ModelId::from("gpt-4o-mini"),
            tools: vec![FunctionDecl::new(
                "read_file",
                Some("Read UTF-8 text from a file".to_owned()),
                JsonSchema::object(
                    std::collections::BTreeMap::from([(
                        "path".to_owned(),
                        JsonSchema::new(types::JsonSchemaType::String),
                    )]),
                    vec!["path".to_owned()],
                ),
            )],
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
        assert_eq!(request_json["tools"][0]["type"], "function");
        assert_eq!(request_json["tools"][0]["function"]["name"], "read_file");
        assert_eq!(request_json["tool_choice"], "auto");
        assert_eq!(
            request_json["messages"][1]["tool_calls"][0]["function"]["arguments"],
            "{\"path\":\"Cargo.toml\"}"
        );
    }

    #[test]
    fn streaming_request_normalization_snapshot_is_stable() {
        let request =
            OpenAIChatCompletionRequest::from_stream_context(&test_context("gpt-4o-mini"))
                .expect("request should normalize");
        let request_json = serde_json::to_value(request).expect("request should serialize");

        assert_json_snapshot!(
            request_json,
            @r###"
        {
          "messages": [
            {
              "content": "Ping",
              "role": "user"
            }
          ],
          "model": "gpt-4o-mini",
          "stream": true,
          "stream_options": {
            "include_usage": true
          }
        }
        "###
        );
    }

    #[test]
    fn streaming_request_normalization_enables_stream_and_usage() {
        let request =
            OpenAIChatCompletionRequest::from_stream_context(&test_context("gpt-4o-mini"))
                .expect("request should normalize");
        let request_json = serde_json::to_value(request).expect("request should serialize");

        assert_eq!(request_json["stream"], true);
        assert_eq!(request_json["stream_options"]["include_usage"], true);
    }

    #[test]
    fn stream_payload_normalization_maps_text_tool_usage_and_finish_reason() {
        let payload = json!({
            "choices": [{
                "delta": {
                    "content": "Done",
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_1",
                        "function": {
                            "name": "read_file",
                            "arguments": "{\"path\":\"Cargo.toml\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {
                "prompt_tokens": 5,
                "completion_tokens": 2,
                "total_tokens": 7
            }
        })
        .to_string();

        let provider = ProviderId::from("openai");
        let mut accumulator = ToolCallAccumulator::default();
        let chunk = parse_openai_stream_payload(&payload, &provider)
            .expect("payload should parse")
            .expect("payload should not terminate stream");
        let items = normalize_openai_stream_chunk(chunk, &mut accumulator);

        assert_eq!(
            items,
            vec![
                StreamItem::UsageUpdate(UsageUpdate {
                    prompt_tokens: Some(5),
                    completion_tokens: Some(2),
                    total_tokens: Some(7),
                }),
                StreamItem::Text("Done".to_owned()),
                StreamItem::ToolCallDelta(ToolCallDelta {
                    index: 0,
                    id: Some("call_1".to_owned()),
                    name: Some("read_file".to_owned()),
                    arguments: Some("{\"path\":\"Cargo.toml\"}".to_owned()),
                }),
                StreamItem::FinishReason("tool_calls".to_owned()),
            ]
        );
    }

    #[test]
    fn tool_call_deltas_reassemble_arguments_across_payloads() {
        let first_payload = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_1",
                        "function": {
                            "name": "read_file",
                            "arguments": "{\"path\":\"Car"
                        }
                    }]
                }
            }]
        })
        .to_string();
        let second_payload = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "function": {
                            "arguments": "go.toml\"}"
                        }
                    }]
                }
            }]
        })
        .to_string();

        let provider = ProviderId::from("openai");
        let mut accumulator = ToolCallAccumulator::default();
        let first_chunk = parse_openai_stream_payload(&first_payload, &provider)
            .expect("first payload should parse")
            .expect("first payload should not terminate stream");
        let first_items = normalize_openai_stream_chunk(first_chunk, &mut accumulator);
        assert!(matches!(
            first_items.first(),
            Some(StreamItem::ToolCallDelta(ToolCallDelta { arguments: Some(arguments), .. }))
                if arguments == "{\"path\":\"Car"
        ));

        let second_chunk = parse_openai_stream_payload(&second_payload, &provider)
            .expect("second payload should parse")
            .expect("second payload should not terminate stream");
        let second_items = normalize_openai_stream_chunk(second_chunk, &mut accumulator);
        let arguments = match second_items.first() {
            Some(StreamItem::ToolCallDelta(delta)) => delta
                .arguments
                .as_ref()
                .expect("reassembled arguments should be present"),
            _ => panic!("expected tool-call delta item"),
        };
        let parsed: serde_json::Value =
            serde_json::from_str(arguments).expect("reassembled arguments should be valid JSON");
        assert_eq!(parsed, json!({"path": "Cargo.toml"}));
    }

    #[test]
    fn tool_call_deltas_reassemble_split_unicode_escape_sequences() {
        let first_payload = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_1",
                        "function": {
                            "name": "echo",
                            "arguments": "{\"emoji\":\"\\uD83D"
                        }
                    }]
                }
            }]
        })
        .to_string();
        let second_payload = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "function": {
                            "arguments": "\\uDE00\"}"
                        }
                    }]
                }
            }]
        })
        .to_string();

        let provider = ProviderId::from("openai");
        let mut accumulator = ToolCallAccumulator::default();
        let first_chunk = parse_openai_stream_payload(&first_payload, &provider)
            .expect("first payload should parse")
            .expect("first payload should not terminate stream");
        let _ = normalize_openai_stream_chunk(first_chunk, &mut accumulator);
        let second_chunk = parse_openai_stream_payload(&second_payload, &provider)
            .expect("second payload should parse")
            .expect("second payload should not terminate stream");
        let second_items = normalize_openai_stream_chunk(second_chunk, &mut accumulator);
        let arguments = match second_items.first() {
            Some(StreamItem::ToolCallDelta(delta)) => delta
                .arguments
                .as_ref()
                .expect("reassembled arguments should be present"),
            _ => panic!("expected tool-call delta item"),
        };
        let parsed: serde_json::Value =
            serde_json::from_str(arguments).expect("reassembled arguments should be valid JSON");
        assert_eq!(parsed, json!({"emoji": "😀"}));
    }

    #[test]
    fn sse_parser_handles_fragmented_frames() {
        let mut parser = SseDataParser::default();
        let payloads = parser
            .push_chunk(br#"data: {"choices":[{"delta":{"content":"Hel"#)
            .expect("first chunk should parse");
        assert!(payloads.is_empty());

        let payloads = parser
            .push_chunk(
                br#"lo"}}]}

data: [DONE]

"#,
            )
            .expect("second chunk should parse");
        assert_eq!(payloads.len(), 2);

        let provider = ProviderId::from("openai");
        let mut accumulator = ToolCallAccumulator::default();
        let chunk = parse_openai_stream_payload(&payloads[0], &provider)
            .expect("payload should parse")
            .expect("payload should not terminate stream");
        let items = normalize_openai_stream_chunk(chunk, &mut accumulator);
        assert_eq!(items, vec![StreamItem::Text("Hello".to_owned())]);

        let done = parse_openai_stream_payload(&payloads[1], &provider)
            .expect("done payload should parse");
        assert!(done.is_none());
    }

    #[test]
    fn stream_emits_connection_lost_when_done_sentinel_missing() {
        let base_url = spawn_one_shot_server(
            "200 OK",
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n",
            "text/event-stream",
        );
        let provider = OpenAIProvider::with_catalog(
            OpenAIConfig {
                api_key: Some("test-key".to_owned()),
                base_url,
            },
            test_model_catalog(),
        )
        .expect("provider should initialize");

        let items = run_stream_collect(&provider, &test_context("gpt-4o-mini"));
        assert!(
            items
                .iter()
                .any(|item| matches!(item, Ok(StreamItem::Text(text)) if text == "Hi"))
        );
        assert!(matches!(
            items.last(),
            Some(Ok(StreamItem::ConnectionLost(message))) if message.contains("[DONE]")
        ));
    }

    #[test]
    fn stream_finishes_without_connection_lost_when_done_is_received() {
        let base_url = spawn_one_shot_server(
            "200 OK",
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\ndata: [DONE]\n\n",
            "text/event-stream",
        );
        let provider = OpenAIProvider::with_catalog(
            OpenAIConfig {
                api_key: Some("test-key".to_owned()),
                base_url,
            },
            test_model_catalog(),
        )
        .expect("provider should initialize");

        let items = run_stream_collect(&provider, &test_context("gpt-4o-mini"));
        assert!(items.iter().all(Result::is_ok));
        assert!(
            items
                .iter()
                .any(|item| matches!(item, Ok(StreamItem::Text(text)) if text == "Hi"))
        );
        assert!(
            !items
                .iter()
                .any(|item| matches!(item, Ok(StreamItem::ConnectionLost(_))))
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
            usage: Some(OpenAIUsage {
                prompt_tokens: Some(5),
                completion_tokens: Some(2),
                total_tokens: Some(7),
            }),
        };

        let normalized =
            normalize_openai_response(response, &ProviderId::from("openai")).expect("should parse");
        assert_eq!(normalized.message.role, MessageRole::Assistant);
        assert_eq!(normalized.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(
            normalized.usage,
            Some(UsageUpdate {
                prompt_tokens: Some(5),
                completion_tokens: Some(2),
                total_tokens: Some(7),
            })
        );
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
                base_url: "http://127.0.0.1:9".to_owned(),
            },
            test_model_catalog(),
        )
        .expect("provider should initialize");

        let context = test_context("unknown-model");
        let validation = run_complete(&provider, &context);
        assert!(matches!(
            validation,
            Err(ProviderError::UnknownModel { .. })
        ));
    }

    #[test]
    fn transport_errors_are_mapped() {
        let provider = OpenAIProvider::with_catalog(
            OpenAIConfig {
                api_key: Some("test-key".to_owned()),
                base_url: "http://127.0.0.1:9".to_owned(),
            },
            test_model_catalog(),
        )
        .expect("provider should initialize");
        let completion = run_complete(&provider, &test_context("gpt-4o-mini"));
        assert!(matches!(completion, Err(ProviderError::Transport { .. })));
    }

    #[test]
    fn http_status_errors_are_mapped() {
        let base_url = spawn_one_shot_server(
            "401 Unauthorized",
            r#"{"error":{"message":"invalid API key"}}"#,
            "application/json",
        );
        let provider = OpenAIProvider::with_catalog(
            OpenAIConfig {
                api_key: Some("test-key".to_owned()),
                base_url,
            },
            test_model_catalog(),
        )
        .expect("provider should initialize");
        let completion = run_complete(&provider, &test_context("gpt-4o-mini"));
        assert!(matches!(
            completion,
            Err(ProviderError::HttpStatus {
                status: 401,
                message,
                ..
            }) if message == "invalid API key"
        ));
    }

    #[test]
    fn response_parse_errors_are_mapped() {
        let base_url = spawn_one_shot_server("200 OK", "not-json", "text/plain");
        let provider = OpenAIProvider::with_catalog(
            OpenAIConfig {
                api_key: Some("test-key".to_owned()),
                base_url,
            },
            test_model_catalog(),
        )
        .expect("provider should initialize");
        let completion = run_complete(&provider, &test_context("gpt-4o-mini"));
        assert!(matches!(
            completion,
            Err(ProviderError::ResponseParse { .. })
        ));
    }

    #[test]
    #[ignore = "requires OPENAI_API_KEY and network access"]
    fn live_openai_smoke_normalizes_response() {
        let _api_key = env::var("OPENAI_API_KEY")
            .expect("set OPENAI_API_KEY to run live_openai_smoke_normalizes_response");
        let provider = OpenAIProvider::new(OpenAIConfig::default())
            .expect("provider should initialize from OPENAI_API_KEY");
        let context = test_context_with_prompt("gpt-4o-mini", "Reply with exactly: PONG");
        let response = run_complete(&provider, &context).expect("live completion should succeed");

        assert_eq!(response.message.role, MessageRole::Assistant);
        assert!(
            response
                .message
                .content
                .as_deref()
                .is_some_and(|content| !content.trim().is_empty())
                || !response.tool_calls.is_empty(),
            "live response should include content or tool calls"
        );
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

    fn test_context(model: &str) -> Context {
        test_context_with_prompt(model, "Ping")
    }

    fn test_context_with_prompt(model: &str, prompt: &str) -> Context {
        Context {
            provider: ProviderId::from("openai"),
            model: ModelId::from(model),
            tools: vec![],
            messages: vec![Message {
                role: MessageRole::User,
                content: Some(prompt.to_owned()),
                tool_calls: vec![],
                tool_call_id: None,
            }],
        }
    }

    fn run_complete(
        provider: &OpenAIProvider,
        context: &Context,
    ) -> Result<Response, ProviderError> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime should build")
            .block_on(provider.complete(context))
    }

    fn run_stream_collect(
        provider: &OpenAIProvider,
        context: &Context,
    ) -> Vec<Result<StreamItem, ProviderError>> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime should build")
            .block_on(async {
                let mut stream = provider
                    .stream(context, DEFAULT_STREAM_BUFFER_SIZE)
                    .await
                    .expect("stream should start");
                let mut items = Vec::new();
                while let Some(item) = stream.recv().await {
                    items.push(item);
                }
                items
            })
    }

    fn spawn_one_shot_server(status_line: &str, body: &str, content_type: &str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("server should bind");
        let address = listener
            .local_addr()
            .expect("server should expose a local address");
        let status_line = status_line.to_owned();
        let body = body.to_owned();
        let content_type = content_type.to_owned();
        let _server = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut request_buffer = [0_u8; 8_192];
                let _ = stream.read(&mut request_buffer);
                let response = format!(
                    "HTTP/1.1 {status_line}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        format!("http://{address}")
    }
}
