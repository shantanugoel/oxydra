use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

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
    DEFAULT_STREAM_BUFFER_SIZE, OPENAI_DEFAULT_BASE_URL, OPENAI_RESPONSES_PATH,
    extract_http_error_message, normalize_base_url_or_default, openai::SseEventParser,
};

#[derive(Debug)]
pub struct ResponsesProvider {
    client: Client,
    provider_id: ProviderId,
    catalog_provider_id: ProviderId,
    model_catalog: ModelCatalog,
    base_url: String,
    api_key: String,
    extra_headers: BTreeMap<String, String>,
    previous_response_id: Arc<Mutex<Option<String>>>,
    last_input_count: Arc<Mutex<usize>>,
}

impl ResponsesProvider {
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
            previous_response_id: Arc::new(Mutex::new(None)),
            last_input_count: Arc::new(Mutex::new(0)),
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

    fn responses_url(&self) -> String {
        format!("{}{}", self.base_url, OPENAI_RESPONSES_PATH)
    }

    fn authenticated_request(&self) -> reqwest::RequestBuilder {
        let mut builder = self
            .client
            .post(self.responses_url())
            .bearer_auth(&self.api_key);
        for (key, value) in &self.extra_headers {
            builder = builder.header(key, value);
        }
        builder
    }

    fn build_request(&self, context: &Context) -> Result<ResponsesApiRequest, ProviderError> {
        let previous_response_id = self.previous_response_id.lock().unwrap().clone();
        let last_input_count = *self.last_input_count.lock().unwrap();
        ResponsesApiRequest::from_context(context, previous_response_id, last_input_count)
    }

    fn reset_chaining_state(&self) {
        *self.previous_response_id.lock().unwrap() = None;
        *self.last_input_count.lock().unwrap() = 0;
    }

    fn update_chaining_state(&self, response_id: String, message_count: usize) {
        *self.previous_response_id.lock().unwrap() = Some(response_id);
        *self.last_input_count.lock().unwrap() = message_count;
    }

    #[cfg(test)]
    pub(crate) fn set_chaining_state(&self, response_id: Option<String>, last_count: usize) {
        *self.previous_response_id.lock().unwrap() = response_id;
        *self.last_input_count.lock().unwrap() = last_count;
    }

    #[cfg(test)]
    pub(crate) fn current_previous_response_id(&self) -> Option<String> {
        self.previous_response_id.lock().unwrap().clone()
    }
}

#[async_trait]
impl Provider for ResponsesProvider {
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

        let mut request = self.build_request(context)?;
        let mut attempted_with_chaining = request.previous_response_id.is_some();

        loop {
            tracing::debug!(
                provider = %self.provider_id,
                model = %context.model,
                previous_response_id = ?request.previous_response_id,
                "sending OpenAI Responses request"
            );

            let http_response = self
                .authenticated_request()
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

                if attempted_with_chaining && (400..500).contains(&status) {
                    self.reset_chaining_state();
                    request = ResponsesApiRequest::from_context(context, None, 0)?;
                    attempted_with_chaining = false;
                    continue;
                }

                return Err(ProviderError::HttpStatus {
                    provider: self.provider_id.clone(),
                    status,
                    message: extract_http_error_message(&body),
                });
            }

            let response: ResponsesApiResponse =
                http_response
                    .json()
                    .await
                    .map_err(|error| ProviderError::ResponseParse {
                        provider: self.provider_id.clone(),
                        message: error.to_string(),
                    })?;

            let normalized = normalize_responses_response(response, &self.provider_id)?;
            if let Some(response_id) = normalized.response_id.clone() {
                self.update_chaining_state(response_id, context.messages.len());
            }
            return Ok(normalized.response);
        }
    }

    async fn stream(
        &self,
        context: &Context,
        buffer_size: usize,
    ) -> Result<ProviderStream, ProviderError> {
        self.validate_context(context)?;

        let mut request = self.build_request(context)?;
        let mut attempted_with_chaining = request.previous_response_id.is_some();

        let http_response = loop {
            tracing::debug!(
                provider = %self.provider_id,
                model = %context.model,
                previous_response_id = ?request.previous_response_id,
                "sending OpenAI Responses streaming request"
            );

            let response = self
                .authenticated_request()
                .json(&request)
                .send()
                .await
                .map_err(|error| ProviderError::Transport {
                    provider: self.provider_id.clone(),
                    message: error.to_string(),
                })?;

            if response.status().is_success() {
                break response;
            }

            let status = response.status().as_u16();
            let body = match response.text().await {
                Ok(text) => text,
                Err(error) => format!("unable to read error body: {error}"),
            };

            if attempted_with_chaining && (400..500).contains(&status) {
                self.reset_chaining_state();
                request = ResponsesApiRequest::from_context(context, None, 0)?;
                attempted_with_chaining = false;
                continue;
            }

            return Err(ProviderError::HttpStatus {
                provider: self.provider_id.clone(),
                status,
                message: extract_http_error_message(&body),
            });
        };

        let channel_size = if buffer_size == 0 {
            DEFAULT_STREAM_BUFFER_SIZE
        } else {
            buffer_size
        };
        let (sender, receiver) = mpsc::channel(channel_size);
        let provider = self.provider_id.clone();
        let previous_response_id = Arc::clone(&self.previous_response_id);
        let last_input_count = Arc::clone(&self.last_input_count);
        let message_count = context.messages.len();

        tokio::spawn(async move {
            let mut http_response = http_response;
            let mut parser = SseEventParser::default();
            let mut accumulator = ResponsesToolCallAccumulator::default();

            loop {
                let chunk = match http_response.chunk().await {
                    Ok(Some(chunk)) => chunk,
                    Ok(None) => break,
                    Err(error) => {
                        let _ = sender
                            .send(Ok(StreamItem::ConnectionLost(format!(
                                "Responses stream transport dropped: {error}"
                            ))))
                            .await;
                        return;
                    }
                };

                let events = match parser.push_chunk(&chunk) {
                    Ok(events) => events,
                    Err(message) => {
                        let _ = sender
                            .send(Err(ProviderError::ResponseParse {
                                provider: provider.clone(),
                                message,
                            }))
                            .await;
                        return;
                    }
                };

                if process_responses_events(
                    &events,
                    &sender,
                    &provider,
                    &mut accumulator,
                    &previous_response_id,
                    &last_input_count,
                    message_count,
                )
                .await
                {
                    return;
                }
            }

            let events = match parser.finish() {
                Ok(events) => events,
                Err(message) => {
                    let _ = sender
                        .send(Err(ProviderError::ResponseParse {
                            provider: provider.clone(),
                            message,
                        }))
                        .await;
                    return;
                }
            };

            let _ = process_responses_events(
                &events,
                &sender,
                &provider,
                &mut accumulator,
                &previous_response_id,
                &last_input_count,
                message_count,
            )
            .await;
        });

        Ok(receiver)
    }
}

async fn process_responses_events(
    events: &[crate::openai::SseEvent],
    sender: &mpsc::Sender<Result<StreamItem, ProviderError>>,
    provider: &ProviderId,
    accumulator: &mut ResponsesToolCallAccumulator,
    previous_response_id: &Arc<Mutex<Option<String>>>,
    last_input_count: &Arc<Mutex<usize>>,
    message_count: usize,
) -> bool {
    for event in events {
        let action = match parse_responses_stream_event(event, provider, accumulator) {
            Ok(action) => action,
            Err(error) => {
                let _ = sender.send(Err(error)).await;
                return true;
            }
        };

        match action {
            ResponsesEventAction::Items(items) => {
                for item in items {
                    if sender.send(Ok(item)).await.is_err() {
                        return true;
                    }
                }
            }
            ResponsesEventAction::Completed { response_id, items } => {
                *previous_response_id.lock().unwrap() = Some(response_id);
                *last_input_count.lock().unwrap() = message_count;
                for item in items {
                    if sender.send(Ok(item)).await.is_err() {
                        return true;
                    }
                }
                return true;
            }
            ResponsesEventAction::Error(error) => {
                let _ = sender.send(Err(error)).await;
                return true;
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Request/Response Types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub(crate) struct ResponsesApiRequest {
    model: String,
    input: Vec<ResponsesInputItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_response_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ResponsesToolDeclaration>,
    store: bool,
    stream: bool,
}

impl ResponsesApiRequest {
    pub(crate) fn from_context(
        context: &Context,
        previous_response_id: Option<String>,
        last_input_count: usize,
    ) -> Result<Self, ProviderError> {
        let mut system_instructions = Vec::new();
        let mut input = Vec::new();

        let start_index = if previous_response_id.is_some() && last_input_count > 0 {
            last_input_count.min(context.messages.len())
        } else {
            0
        };

        for message in context.messages.iter().skip(start_index) {
            match message.role {
                MessageRole::System => {
                    if let Some(text) = message.content.as_deref().filter(|t| !t.trim().is_empty())
                    {
                        system_instructions.push(text.to_owned());
                    }
                }
                MessageRole::User => {
                    if let Some(text) = message.content.as_deref().filter(|t| !t.trim().is_empty())
                    {
                        input.push(ResponsesInputItem::Message {
                            role: "user".to_owned(),
                            content: text.to_owned(),
                        });
                    }
                }
                MessageRole::Assistant => {
                    if let Some(text) = message.content.as_deref().filter(|t| !t.trim().is_empty())
                    {
                        input.push(ResponsesInputItem::Message {
                            role: "assistant".to_owned(),
                            content: text.to_owned(),
                        });
                    }
                    for tool_call in &message.tool_calls {
                        input.push(ResponsesInputItem::FunctionCall {
                            call_id: tool_call.id.clone(),
                            name: tool_call.name.clone(),
                            arguments: serde_json::to_string(&tool_call.arguments)
                                .unwrap_or_default(),
                        });
                    }
                }
                MessageRole::Tool => {
                    let call_id = message.tool_call_id.clone().unwrap_or_default();
                    let output = message.content.clone().unwrap_or_default();
                    input.push(ResponsesInputItem::FunctionCallOutput { call_id, output });
                }
            }
        }

        let instructions = if start_index == 0 && !system_instructions.is_empty() {
            Some(system_instructions.join("\n"))
        } else {
            None
        };

        let tools = context
            .tools
            .iter()
            .map(ResponsesToolDeclaration::from)
            .collect::<Vec<_>>();

        Ok(Self {
            model: context.model.0.clone(),
            input,
            previous_response_id,
            instructions,
            tools,
            store: true,
            stream: false,
        })
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ResponsesInputItem {
    Message {
        role: String,
        content: String,
    },
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    FunctionCallOutput {
        call_id: String,
        output: String,
    },
}

#[derive(Debug, Serialize)]
pub(crate) struct ResponsesToolDeclaration {
    r#type: String,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    parameters: JsonSchema,
    #[serde(skip_serializing_if = "Option::is_none")]
    strict: Option<bool>,
}

/// Recursively sanitize a schema for OpenAI Responses API strict mode:
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

impl From<&FunctionDecl> for ResponsesToolDeclaration {
    fn from(value: &FunctionDecl) -> Self {
        Self {
            r#type: "function".to_owned(),
            name: value.name.clone(),
            description: value.description.clone(),
            parameters: sanitize_schema_strict(&value.parameters),
            strict: None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ResponsesApiResponse {
    pub(crate) id: Option<String>,
    #[serde(default)]
    pub(crate) output: Vec<ResponsesOutputBlock>,
    #[serde(default)]
    pub(crate) status: Option<String>,
    #[serde(default)]
    pub(crate) usage: Option<ResponsesUsage>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ResponsesOutputBlock {
    Message {
        role: String,
        #[serde(default)]
        content: Vec<ResponsesOutputContent>,
    },
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ResponsesOutputContent {
    OutputText { text: String },
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ResponsesUsage {
    #[serde(default)]
    pub(crate) input_tokens: Option<u64>,
    #[serde(default)]
    pub(crate) output_tokens: Option<u64>,
    #[serde(default)]
    pub(crate) total_tokens: Option<u64>,
}

pub(crate) struct NormalizedResponses {
    pub(crate) response: Response,
    pub(crate) response_id: Option<String>,
}

pub(crate) fn normalize_responses_response(
    response: ResponsesApiResponse,
    provider: &ProviderId,
) -> Result<NormalizedResponses, ProviderError> {
    let mut text_chunks = Vec::new();
    let mut tool_calls = Vec::new();

    for output in response.output.iter() {
        match output {
            ResponsesOutputBlock::Message { content, .. } => {
                for item in content {
                    let ResponsesOutputContent::OutputText { text } = item;
                    if !text.is_empty() {
                        text_chunks.push(text.clone());
                    }
                }
            }
            ResponsesOutputBlock::FunctionCall {
                call_id,
                name,
                arguments,
            } => {
                let args_json = serde_json::from_str::<Value>(arguments)
                    .unwrap_or_else(|_| Value::String(arguments.clone()));
                tool_calls.push(ToolCall {
                    id: call_id.clone(),
                    name: name.clone(),
                    arguments: args_json,
                });
            }
        }
    }

    let message = Message {
        role: MessageRole::Assistant,
        content: if text_chunks.is_empty() {
            None
        } else {
            Some(text_chunks.join(""))
        },
        tool_calls: tool_calls.clone(),
        tool_call_id: None,
    };

    let usage = response.usage.map(|usage| UsageUpdate {
        prompt_tokens: usage.input_tokens,
        completion_tokens: usage.output_tokens,
        total_tokens: usage.total_tokens,
    });

    let finish_reason = response.status.clone();

    if response.output.is_empty() {
        return Err(ProviderError::ResponseParse {
            provider: provider.clone(),
            message: "Responses API response contained no output items".to_owned(),
        });
    }

    Ok(NormalizedResponses {
        response: Response {
            message,
            tool_calls,
            finish_reason,
            usage,
        },
        response_id: response.id,
    })
}

// ---------------------------------------------------------------------------
// Streaming event parsing
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ResponsesTextDeltaData {
    pub(crate) delta: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ResponsesFunctionArgsDeltaData {
    pub(crate) output_index: u32,
    pub(crate) delta: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ResponsesOutputItemAddedData {
    pub(crate) output_index: u32,
    #[serde(default)]
    pub(crate) item: ResponsesOutputItemAdded,
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub(crate) struct ResponsesOutputItemAdded {
    #[serde(default)]
    pub(crate) r#type: Option<String>,
    #[serde(default)]
    pub(crate) call_id: Option<String>,
    #[serde(default)]
    pub(crate) name: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ResponsesCompletedData {
    pub(crate) response: ResponsesApiResponse,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ResponsesFailedData {
    pub(crate) error: Option<String>,
}

#[derive(Debug)]
pub(crate) enum ResponsesEventAction {
    Items(Vec<StreamItem>),
    Completed {
        response_id: String,
        items: Vec<StreamItem>,
    },
    Error(ProviderError),
}

#[derive(Debug, Default)]
pub(crate) struct ResponsesToolCallAccumulator {
    output_index_to_call: BTreeMap<u32, ToolCallState>,
    next_ordinal: u32,
}

#[derive(Debug, Default)]
struct ToolCallState {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
    ordinal: Option<u32>,
}

impl ResponsesToolCallAccumulator {
    fn register_output_item(
        &mut self,
        output_index: u32,
        call_id: Option<String>,
        name: Option<String>,
    ) {
        // Assign a monotonic ordinal for new entries before borrowing entry mutably.
        let needs_ordinal = self
            .output_index_to_call
            .get(&output_index)
            .is_none_or(|e| e.ordinal.is_none());
        if needs_ordinal {
            let next = self.next_ordinal;
            self.next_ordinal += 1;
            self.output_index_to_call
                .entry(output_index)
                .or_default()
                .ordinal = Some(next);
        }
        let entry = self.output_index_to_call.entry(output_index).or_default();
        if call_id.is_some() {
            entry.id = call_id;
        }
        if name.is_some() {
            entry.name = name;
        }
    }

    fn append_arguments(&mut self, output_index: u32, delta: &str) -> Option<StreamItem> {
        // Assign a monotonic ordinal for entries that arrive without a prior
        // `response.output_item.added` event (fallback path).
        let needs_ordinal = self
            .output_index_to_call
            .get(&output_index)
            .is_none_or(|e| e.ordinal.is_none());
        if needs_ordinal {
            let next = self.next_ordinal;
            self.next_ordinal += 1;
            self.output_index_to_call
                .entry(output_index)
                .or_default()
                .ordinal = Some(next);
        }

        let entry = self.output_index_to_call.get_mut(&output_index).unwrap();
        entry.arguments.push_str(delta);
        let ordinal = entry.ordinal.unwrap() as usize;

        Some(StreamItem::ToolCallDelta(ToolCallDelta {
            index: ordinal,
            id: entry.id.clone(),
            name: entry.name.clone(),
            arguments: if delta.is_empty() {
                None
            } else {
                Some(delta.to_owned())
            },
        }))
    }

    fn has_streamed_tool_calls(&self) -> bool {
        !self.output_index_to_call.is_empty()
    }
}

pub(crate) fn parse_responses_stream_event(
    event: &crate::openai::SseEvent,
    provider: &ProviderId,
    accumulator: &mut ResponsesToolCallAccumulator,
) -> Result<ResponsesEventAction, ProviderError> {
    let event_type = event.event_type.as_deref().unwrap_or("");
    if event_type.is_empty() {
        return Ok(ResponsesEventAction::Items(vec![]));
    }

    match event_type {
        "response.output_text.delta" => {
            let data: ResponsesTextDeltaData =
                serde_json::from_str(&event.data).map_err(|e| ProviderError::ResponseParse {
                    provider: provider.clone(),
                    message: format!("failed to parse output_text.delta: {e}"),
                })?;
            if data.delta.is_empty() {
                Ok(ResponsesEventAction::Items(vec![]))
            } else {
                Ok(ResponsesEventAction::Items(vec![StreamItem::Text(
                    data.delta,
                )]))
            }
        }
        "response.function_call_arguments.delta" => {
            let data: ResponsesFunctionArgsDeltaData =
                serde_json::from_str(&event.data).map_err(|e| ProviderError::ResponseParse {
                    provider: provider.clone(),
                    message: format!("failed to parse function_call_arguments.delta: {e}"),
                })?;
            if let Some(item) = accumulator.append_arguments(data.output_index, &data.delta) {
                Ok(ResponsesEventAction::Items(vec![item]))
            } else {
                Ok(ResponsesEventAction::Items(vec![]))
            }
        }
        "response.output_item.added" => {
            let data: ResponsesOutputItemAddedData =
                serde_json::from_str(&event.data).map_err(|e| ProviderError::ResponseParse {
                    provider: provider.clone(),
                    message: format!("failed to parse output_item.added: {e}"),
                })?;
            accumulator.register_output_item(data.output_index, data.item.call_id, data.item.name);
            Ok(ResponsesEventAction::Items(vec![]))
        }
        "response.completed" => {
            let data: ResponsesCompletedData =
                serde_json::from_str(&event.data).map_err(|e| ProviderError::ResponseParse {
                    provider: provider.clone(),
                    message: format!("failed to parse response.completed: {e}"),
                })?;
            let normalized = normalize_responses_response(data.response, provider)?;
            let mut items = Vec::new();
            if let Some(content) = normalized.response.message.content {
                items.push(StreamItem::Text(content));
            }
            // Only emit tool call deltas from the completed event when no incremental
            // streaming deltas were already sent.  If the accumulator has entries,
            // the arguments were already sent fragment-by-fragment via
            // `response.function_call_arguments.delta`; re-emitting them here would
            // cause the runtime accumulator to double-append the arguments.
            if !accumulator.has_streamed_tool_calls() {
                for (idx, tool_call) in normalized.response.tool_calls.iter().enumerate() {
                    items.push(StreamItem::ToolCallDelta(ToolCallDelta {
                        index: idx,
                        id: Some(tool_call.id.clone()),
                        name: Some(tool_call.name.clone()),
                        arguments: Some(
                            serde_json::to_string(&tool_call.arguments).unwrap_or_default(),
                        ),
                    }));
                }
            }
            if let Some(usage) = normalized.response.usage.clone() {
                items.push(StreamItem::UsageUpdate(usage));
            }
            if let Some(reason) = normalized.response.finish_reason.clone() {
                items.push(StreamItem::FinishReason(reason));
            }

            if let Some(response_id) = normalized.response_id {
                Ok(ResponsesEventAction::Completed { response_id, items })
            } else {
                Ok(ResponsesEventAction::Items(items))
            }
        }
        "response.failed" => {
            let data: ResponsesFailedData =
                serde_json::from_str(&event.data).map_err(|e| ProviderError::ResponseParse {
                    provider: provider.clone(),
                    message: format!("failed to parse response.failed: {e}"),
                })?;
            Ok(ResponsesEventAction::Error(ProviderError::RequestFailed {
                provider: provider.clone(),
                message: data
                    .error
                    .unwrap_or_else(|| "Responses API failed".to_owned()),
            }))
        }
        _ => Ok(ResponsesEventAction::Items(vec![])),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Issue 4: Ordinal assignment via monotonic counter
    // -----------------------------------------------------------------------

    #[test]
    fn single_tool_call_gets_ordinal_zero() {
        let mut acc = ResponsesToolCallAccumulator::default();
        acc.register_output_item(0, Some("call_1".to_owned()), Some("search".to_owned()));
        let item = acc.append_arguments(0, r#"{"q":"hi"}"#).unwrap();
        let StreamItem::ToolCallDelta(delta) = item else {
            panic!("expected ToolCallDelta");
        };
        assert_eq!(delta.index, 0);
    }

    #[test]
    fn two_tool_calls_get_distinct_ordinals_zero_and_one() {
        let mut acc = ResponsesToolCallAccumulator::default();
        acc.register_output_item(0, Some("call_1".to_owned()), Some("search".to_owned()));
        acc.register_output_item(1, Some("call_2".to_owned()), Some("fetch".to_owned()));

        let item0 = acc.append_arguments(0, r#"{"q":"a"}"#).unwrap();
        let item1 = acc.append_arguments(1, r#"{"url":"b"}"#).unwrap();

        let StreamItem::ToolCallDelta(delta0) = item0 else {
            panic!("expected ToolCallDelta");
        };
        let StreamItem::ToolCallDelta(delta1) = item1 else {
            panic!("expected ToolCallDelta");
        };
        assert_eq!(delta0.index, 0, "first tool call must have ordinal 0");
        assert_eq!(delta1.index, 1, "second tool call must have ordinal 1");
    }

    #[test]
    fn three_tool_calls_get_consecutive_ordinals() {
        let mut acc = ResponsesToolCallAccumulator::default();
        for i in 0u32..3 {
            acc.register_output_item(i, Some(format!("call_{i}")), Some(format!("tool_{i}")));
        }
        let ordinals: Vec<usize> = (0u32..3)
            .map(|i| {
                let item = acc.append_arguments(i, "{}").unwrap();
                let StreamItem::ToolCallDelta(delta) = item else {
                    panic!("expected ToolCallDelta");
                };
                delta.index
            })
            .collect();
        assert_eq!(ordinals, vec![0, 1, 2]);
    }

    #[test]
    fn ordinal_assigned_in_append_when_no_prior_register() {
        // Fallback: delta arrives without a preceding output_item.added event.
        let mut acc = ResponsesToolCallAccumulator::default();
        let item0 = acc.append_arguments(5, r#"{"a":1}"#).unwrap();
        let item1 = acc.append_arguments(7, r#"{"b":2}"#).unwrap();

        let StreamItem::ToolCallDelta(delta0) = item0 else {
            panic!("expected ToolCallDelta");
        };
        let StreamItem::ToolCallDelta(delta1) = item1 else {
            panic!("expected ToolCallDelta");
        };
        assert_eq!(delta0.index, 0, "first fallback delta must get ordinal 0");
        assert_eq!(delta1.index, 1, "second fallback delta must get ordinal 1");
    }

    // -----------------------------------------------------------------------
    // Issue 5: response.completed index assignment and double-emit guard
    // -----------------------------------------------------------------------

    fn make_completed_event(tool_calls: &[(&str, &str, &str)]) -> crate::openai::SseEvent {
        // Build a minimal response.completed SSE event with the given tool calls.
        // Each tuple is (call_id, name, arguments_json).
        let output: Vec<serde_json::Value> = tool_calls
            .iter()
            .map(|(id, name, args)| {
                serde_json::json!({
                    "type": "function_call",
                    "call_id": id,
                    "name": name,
                    "arguments": args,
                })
            })
            .collect();
        let data = serde_json::json!({
            "response": {
                "id": "resp_1",
                "status": "completed",
                "output": output,
            }
        });
        crate::openai::SseEvent {
            event_type: Some("response.completed".to_owned()),
            data: data.to_string(),
        }
    }

    fn collect_tool_call_deltas(items: &[StreamItem]) -> Vec<(usize, String, String)> {
        items
            .iter()
            .filter_map(|item| {
                if let StreamItem::ToolCallDelta(d) = item {
                    Some((
                        d.index,
                        d.id.clone().unwrap_or_default(),
                        d.name.clone().unwrap_or_default(),
                    ))
                } else {
                    None
                }
            })
            .collect()
    }

    #[test]
    fn completed_event_emits_sequential_indices_when_no_streaming_deltas() {
        let provider = ProviderId("test".to_owned());
        let mut acc = ResponsesToolCallAccumulator::default();
        let event = make_completed_event(&[
            ("call_a", "search", r#"{"q":"hello"}"#),
            ("call_b", "fetch", r#"{"url":"http://x.com"}"#),
            ("call_c", "compute", r#"{"expr":"1+1"}"#),
        ]);

        let action = parse_responses_stream_event(&event, &provider, &mut acc).unwrap();
        let ResponsesEventAction::Completed { items, .. } = action else {
            panic!("expected Completed action");
        };

        let deltas = collect_tool_call_deltas(&items);
        assert_eq!(deltas.len(), 3, "should emit 3 tool call deltas");
        assert_eq!(deltas[0].0, 0, "first delta index must be 0");
        assert_eq!(deltas[1].0, 1, "second delta index must be 1");
        assert_eq!(deltas[2].0, 2, "third delta index must be 2");
        assert_eq!(deltas[0].1, "call_a");
        assert_eq!(deltas[1].1, "call_b");
        assert_eq!(deltas[2].1, "call_c");
    }

    #[test]
    fn completed_event_skips_tool_calls_when_streaming_deltas_already_sent() {
        // Simulate streaming: accumulator already has entries from delta events.
        let provider = ProviderId("test".to_owned());
        let mut acc = ResponsesToolCallAccumulator::default();
        acc.register_output_item(0, Some("call_a".to_owned()), Some("search".to_owned()));
        let _ = acc.append_arguments(0, r#"{"q":"hello"}"#);

        let event = make_completed_event(&[("call_a", "search", r#"{"q":"hello"}"#)]);
        let action = parse_responses_stream_event(&event, &provider, &mut acc).unwrap();
        let ResponsesEventAction::Completed { items, .. } = action else {
            panic!("expected Completed action");
        };

        let deltas = collect_tool_call_deltas(&items);
        assert!(
            deltas.is_empty(),
            "tool call deltas must not be re-emitted when streaming deltas were already sent"
        );
    }
}
