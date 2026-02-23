use std::collections::{BTreeMap, HashMap};

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;
use types::{
    Context, FunctionDecl, JsonSchema, JsonSchemaType, Message, MessageRole, ModelCatalog,
    Provider, ProviderError, ProviderId, ProviderStream, Response, StreamItem, ToolCall,
    ToolCallDelta, UsageUpdate,
};

use crate::{
    DEFAULT_STREAM_BUFFER_SIZE, OPENAI_CHAT_COMPLETIONS_PATH, OPENAI_DEFAULT_BASE_URL,
    extract_http_error_message, normalize_base_url_or_default,
};

#[derive(Debug, Clone)]
pub struct OpenAIProvider {
    client: Client,
    provider_id: ProviderId,
    catalog_provider_id: ProviderId,
    model_catalog: ModelCatalog,
    base_url: String,
    api_key: String,
    extra_headers: BTreeMap<String, String>,
}

impl OpenAIProvider {
    pub fn new(
        provider_id: ProviderId,
        catalog_provider_id: ProviderId,
        api_key: String,
        base_url: String,
        extra_headers: BTreeMap<String, String>,
        model_catalog: ModelCatalog,
    ) -> Self {
        Self {
            client: Client::new(),
            provider_id,
            catalog_provider_id,
            model_catalog,
            base_url: normalize_base_url_or_default(&base_url, OPENAI_DEFAULT_BASE_URL),
            api_key,
            extra_headers,
        }
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
            .validate(&self.catalog_provider_id, &context.model)?;
        Ok(())
    }

    fn chat_completions_url(&self) -> String {
        format!("{}{}", self.base_url, OPENAI_CHAT_COMPLETIONS_PATH)
    }

    fn authenticated_request(&self, url: &str) -> reqwest::RequestBuilder {
        let mut builder = self.client.post(url).bearer_auth(&self.api_key);
        for (key, value) in &self.extra_headers {
            builder = builder.header(key, value);
        }
        builder
    }
}

#[async_trait]
impl Provider for OpenAIProvider {
    fn provider_id(&self) -> &ProviderId {
        &self.provider_id
    }

    fn catalog_provider_id(&self) -> &ProviderId {
        &self.catalog_provider_id
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
            .authenticated_request(&self.chat_completions_url())
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
            .authenticated_request(&self.chat_completions_url())
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
pub(crate) struct OpenAIChatCompletionRequest {
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
    pub(crate) fn from_context(context: &Context) -> Result<Self, ProviderError> {
        Self::from_context_with_stream(context, false)
    }

    pub(crate) fn from_stream_context(context: &Context) -> Result<Self, ProviderError> {
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

/// Recursively sanitize a schema for OpenAI strict mode:
/// - Set `additionalProperties: false` on every object schema.
/// - Add all property names to `required` so the model fills them all in.
fn sanitize_schema_strict(schema: &JsonSchema) -> JsonSchema {
    let mut s = schema.clone();
    if s.schema_type == JsonSchemaType::Object {
        s.additional_properties = Some(false);
        let all_keys: Vec<String> = s.properties.keys().cloned().collect();
        for key in all_keys {
            if !s.required.contains(&key) {
                s.required.push(key);
            }
        }
        s.properties = s
            .properties
            .iter()
            .map(|(k, v)| (k.clone(), sanitize_schema_strict(v)))
            .collect();
    }
    if let Some(items) = &s.items {
        s.items = Some(Box::new(sanitize_schema_strict(items)));
    }
    s
}

impl From<&FunctionDecl> for OpenAIRequestToolDefinition {
    fn from(value: &FunctionDecl) -> Self {
        Self {
            kind: "function".to_owned(),
            function: OpenAIRequestFunctionDecl {
                name: value.name.clone(),
                description: value.description.clone(),
                parameters: sanitize_schema_strict(&value.parameters),
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
pub(crate) struct OpenAIChatCompletionResponse {
    pub(crate) choices: Vec<OpenAIChoice>,
    #[serde(default)]
    pub(crate) usage: Option<OpenAIUsage>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenAIChoice {
    pub(crate) message: OpenAIChatMessageResponse,
    #[serde(default)]
    pub(crate) finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenAIChatMessageResponse {
    pub(crate) role: String,
    #[serde(default)]
    pub(crate) content: Option<String>,
    #[serde(default)]
    pub(crate) tool_calls: Vec<OpenAIResponseToolCall>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenAIResponseToolCall {
    pub(crate) id: String,
    #[serde(rename = "type")]
    pub(crate) kind: String,
    pub(crate) function: OpenAIResponseFunction,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenAIResponseFunction {
    pub(crate) name: String,
    pub(crate) arguments: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenAIChatCompletionStreamChunk {
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
pub(crate) struct OpenAIUsage {
    #[serde(default)]
    pub(crate) prompt_tokens: Option<u64>,
    #[serde(default)]
    pub(crate) completion_tokens: Option<u64>,
    #[serde(default)]
    pub(crate) total_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenAIErrorEnvelope {
    pub(crate) error: OpenAIErrorBody,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenAIErrorBody {
    pub(crate) message: String,
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

pub(crate) fn normalize_openai_response(
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

pub(crate) fn parse_openai_stream_payload(
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

pub(crate) fn normalize_openai_stream_chunk(
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
pub(crate) struct ToolCallAccumulator {
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
        let fragment = function.arguments;
        if let Some(ref args) = fragment {
            entry.arguments.push_str(args);
        }

        ToolCallDelta {
            index,
            id: entry.id.clone(),
            name: entry.name.clone(),
            arguments: fragment,
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct SseDataParser {
    line_buffer: Vec<u8>,
    data_lines: Vec<String>,
}

impl SseDataParser {
    pub(crate) fn push_chunk(&mut self, chunk: &[u8]) -> Result<Vec<String>, String> {
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

    pub(crate) fn finish(mut self) -> Result<Vec<String>, String> {
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

/// An SSE event with an optional event type.
///
/// The Responses API uses `event:` lines to distinguish between different
/// event types (e.g. `response.output_text.delta`, `response.completed`).
/// The standard [`SseDataParser`] discards these lines. [`SseEventParser`]
/// captures both the `event:` type and `data:` payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SseEvent {
    pub(crate) event_type: Option<String>,
    pub(crate) data: String,
}

/// SSE parser that captures both `event:` and `data:` lines.
///
/// This extends the behavior of [`SseDataParser`] to also track event types.
/// Used by the Responses API provider where event types determine how to
/// interpret the data payload.
#[derive(Debug, Default)]
pub(crate) struct SseEventParser {
    line_buffer: Vec<u8>,
    event_type: Option<String>,
    data_lines: Vec<String>,
}

impl SseEventParser {
    pub(crate) fn push_chunk(&mut self, chunk: &[u8]) -> Result<Vec<SseEvent>, String> {
        let mut events = Vec::new();
        for byte in chunk {
            if *byte == b'\n' {
                let line = std::mem::take(&mut self.line_buffer);
                self.process_line(&line, &mut events)?;
            } else {
                self.line_buffer.push(*byte);
            }
        }
        Ok(events)
    }

    pub(crate) fn finish(mut self) -> Result<Vec<SseEvent>, String> {
        let mut events = Vec::new();
        if !self.line_buffer.is_empty() {
            let line = std::mem::take(&mut self.line_buffer);
            self.process_line(&line, &mut events)?;
        }
        self.flush_event(&mut events);
        Ok(events)
    }

    fn process_line(&mut self, line: &[u8], events: &mut Vec<SseEvent>) -> Result<(), String> {
        let line = if line.ends_with(b"\r") {
            &line[..line.len() - 1]
        } else {
            line
        };

        if line.is_empty() {
            self.flush_event(events);
            return Ok(());
        }

        if line.starts_with(b":") {
            return Ok(());
        }

        if let Some(mut value) = line.strip_prefix(b"event:") {
            if value.starts_with(b" ") {
                value = &value[1..];
            }
            let value = std::str::from_utf8(value)
                .map_err(|error| format!("invalid UTF-8 in SSE event line: {error}"))?;
            self.event_type = Some(value.to_owned());
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

    fn flush_event(&mut self, events: &mut Vec<SseEvent>) {
        if !self.data_lines.is_empty() {
            events.push(SseEvent {
                event_type: self.event_type.take(),
                data: self.data_lines.join("\n"),
            });
            self.data_lines.clear();
        } else {
            // Discard event type without data.
            self.event_type = None;
        }
    }
}
