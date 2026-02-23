use std::collections::BTreeMap;

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;
use types::{
    Context, FunctionDecl, Message, MessageRole, ModelCatalog, Provider, ProviderError,
    ProviderId, ProviderStream, Response, StreamItem, ToolCall, UsageUpdate,
};

use crate::{
    ANTHROPIC_DEFAULT_BASE_URL, ANTHROPIC_MESSAGES_PATH, ANTHROPIC_VERSION,
    DEFAULT_ANTHROPIC_MAX_TOKENS, DEFAULT_STREAM_BUFFER_SIZE, extract_http_error_message,
    non_empty, normalize_base_url_or_default, openai::SseDataParser,
};

#[derive(Debug, Clone)]
pub struct AnthropicProvider {
    client: Client,
    provider_id: ProviderId,
    catalog_provider_id: ProviderId,
    model_catalog: ModelCatalog,
    base_url: String,
    api_key: String,
    max_tokens: u32,
    extra_headers: BTreeMap<String, String>,
}

impl AnthropicProvider {
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
            base_url: normalize_base_url_or_default(&base_url, ANTHROPIC_DEFAULT_BASE_URL),
            api_key,
            max_tokens: DEFAULT_ANTHROPIC_MAX_TOKENS,
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

    fn messages_url(&self) -> String {
        format!("{}{}", self.base_url, ANTHROPIC_MESSAGES_PATH)
    }

    fn authenticated_request(&self, url: &str) -> reqwest::RequestBuilder {
        let mut builder = self
            .client
            .post(url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION);
        for (key, value) in &self.extra_headers {
            builder = builder.header(key, value);
        }
        builder
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
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
            "sending Anthropic messages request"
        );

        let request = AnthropicMessagesRequest::from_context(context, self.max_tokens)?;
        let http_response = self
            .authenticated_request(&self.messages_url())
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
        tracing::debug!(
            provider = %self.provider_id,
            model = %context.model,
            "sending Anthropic messages streaming request"
        );

        let request =
            AnthropicMessagesRequest::from_context_with_stream(context, self.max_tokens, true)?;
        let mut http_response = self
            .authenticated_request(&self.messages_url())
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
            let mut accumulator = AnthropicToolCallAccumulator::default();
            let mut input_tokens: Option<u64> = None;
            let mut received_error = false;
            let mut stream_done = false;

            loop {
                let chunk = match http_response.chunk().await {
                    Ok(Some(chunk)) => chunk,
                    Ok(None) => break, // EOF
                    Err(error) => {
                        // Transport error while reading chunks.
                        let _ = sender
                            .send(Ok(StreamItem::ConnectionLost(format!(
                                "Anthropic stream transport dropped: {error}"
                            ))))
                            .await;
                        return;
                    }
                };

                let payloads = match parser.push_chunk(&chunk) {
                    Ok(payloads) => payloads,
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

                if process_anthropic_payloads(
                    &payloads,
                    &sender,
                    &provider,
                    &mut accumulator,
                    &mut input_tokens,
                    &mut received_error,
                    &mut stream_done,
                )
                .await
                {
                    return;
                }
            }

            // Flush any remaining buffered data from the parser.
            let payloads = match parser.finish() {
                Ok(payloads) => payloads,
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

            if process_anthropic_payloads(
                &payloads,
                &sender,
                &provider,
                &mut accumulator,
                &mut input_tokens,
                &mut received_error,
                &mut stream_done,
            )
            .await
            {
                return;
            }

            // EOF reached without `message_stop` or `error` — connection lost.
            if !stream_done && !received_error {
                let _ = sender
                    .send(Ok(StreamItem::ConnectionLost(
                        "Anthropic stream ended before message_stop".to_owned(),
                    )))
                    .await;
            }
        });

        Ok(receiver)
    }
}

/// Process a batch of SSE payload strings through
/// [`parse_anthropic_stream_payload`] and send resulting items through the
/// channel.
///
/// Returns `true` when the caller should exit the streaming loop (stream done,
/// terminal error, or receiver dropped).
async fn process_anthropic_payloads(
    payloads: &[String],
    sender: &mpsc::Sender<Result<StreamItem, ProviderError>>,
    provider: &ProviderId,
    accumulator: &mut AnthropicToolCallAccumulator,
    input_tokens: &mut Option<u64>,
    received_error: &mut bool,
    stream_done: &mut bool,
) -> bool {
    for payload in payloads {
        match parse_anthropic_stream_payload(payload, provider, accumulator, input_tokens) {
            AnthropicEventAction::Items(items) => {
                for item in items {
                    if sender.send(Ok(item)).await.is_err() {
                        // Receiver dropped.
                        return true;
                    }
                }
            }
            AnthropicEventAction::Done => {
                *stream_done = true;
                return true;
            }
            AnthropicEventAction::Error(error) => {
                *received_error = true;
                let _ = sender.send(Err(error)).await;
                return true;
            }
        }
    }
    false
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
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    stream: bool,
}

impl AnthropicMessagesRequest {
    pub(crate) fn from_context(context: &Context, max_tokens: u32) -> Result<Self, ProviderError> {
        Self::from_context_with_stream(context, max_tokens, false)
    }

    pub(crate) fn from_context_with_stream(
        context: &Context,
        max_tokens: u32,
        stream: bool,
    ) -> Result<Self, ProviderError> {
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
            stream,
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
    input_schema: serde_json::Value,
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

// ---------------------------------------------------------------------------
// Anthropic SSE event deserialization types (internal to streaming).
// These are `Deserialize`-only types constructed by serde; they will be
// consumed by `parse_anthropic_stream_payload` once event-to-StreamItem
// conversion is implemented.
// ---------------------------------------------------------------------------

// Serde-only deserialization types — serde's `Deserialize` impl constructs
// them; suppress the false-positive `dead_code` warning.
mod sse_types {
    use serde::Deserialize;

    use super::AnthropicUsage;

    /// Thin envelope — first-pass parse to extract the event type from the JSON
    /// `"type"` field. Subsequent re-parse into the appropriate typed struct is
    /// driven by the value of `kind`.
    #[derive(Debug, Deserialize)]
    pub(crate) struct AnthropicStreamEvent {
        #[serde(rename = "type")]
        pub(crate) kind: String,
    }

    #[derive(Debug, Deserialize)]
    pub(crate) struct AnthropicMessageStartEvent {
        pub(crate) message: AnthropicMessageStartPayload,
    }

    #[derive(Debug, Deserialize)]
    pub(crate) struct AnthropicMessageStartPayload {
        #[serde(default)]
        pub(crate) usage: Option<AnthropicUsage>,
    }

    #[derive(Debug, Deserialize)]
    pub(crate) struct AnthropicContentBlockStartEvent {
        pub(crate) index: usize,
        pub(crate) content_block: AnthropicContentBlockStartPayload,
    }

    #[derive(Debug, Deserialize)]
    pub(crate) struct AnthropicContentBlockStartPayload {
        #[serde(rename = "type")]
        pub(crate) kind: String,
        /// Tool-call ID (present only for `tool_use` blocks).
        #[serde(default)]
        pub(crate) id: Option<String>,
        /// Tool name (present only for `tool_use` blocks).
        #[serde(default)]
        pub(crate) name: Option<String>,
    }

    #[derive(Debug, Deserialize)]
    pub(crate) struct AnthropicContentBlockDeltaEvent {
        pub(crate) index: usize,
        pub(crate) delta: AnthropicDelta,
    }

    #[derive(Debug, Deserialize)]
    pub(crate) struct AnthropicDelta {
        #[serde(rename = "type")]
        pub(crate) kind: String,
        /// Text fragment (present for `text_delta`).
        #[serde(default)]
        pub(crate) text: Option<String>,
        /// Partial tool-call arguments JSON (present for `input_json_delta`).
        #[serde(default)]
        pub(crate) partial_json: Option<String>,
        /// Thinking/reasoning content from Claude's extended thinking feature
        /// (present for `thinking_delta`).
        #[serde(default)]
        pub(crate) thinking: Option<String>,
    }

    #[derive(Debug, Deserialize)]
    pub(crate) struct AnthropicMessageDeltaEvent {
        pub(crate) delta: AnthropicMessageDeltaPayload,
        #[serde(default)]
        pub(crate) usage: Option<AnthropicDeltaUsage>,
    }

    #[derive(Debug, Deserialize)]
    pub(crate) struct AnthropicMessageDeltaPayload {
        #[serde(default)]
        pub(crate) stop_reason: Option<String>,
    }

    /// Usage data from `message_delta` events — only carries `output_tokens`.
    /// Separate from `AnthropicUsage` which appears in `message_start` and
    /// non-streaming responses.
    #[derive(Debug, Deserialize)]
    pub(crate) struct AnthropicDeltaUsage {
        #[serde(default)]
        pub(crate) output_tokens: Option<u64>,
    }

    #[derive(Debug, Deserialize)]
    pub(crate) struct AnthropicStreamErrorEvent {
        pub(crate) error: AnthropicStreamErrorPayload,
    }

    #[derive(Debug, Deserialize)]
    pub(crate) struct AnthropicStreamErrorPayload {
        #[serde(default)]
        pub(crate) message: Option<String>,
    }
}

// Re-export for use by `parse_anthropic_stream_payload` and tests.
pub(crate) use sse_types::*;

// ---------------------------------------------------------------------------
// Anthropic tool call accumulator with content-block-index → tool-call-ordinal
// remapping.
//
// Anthropic's `index` field counts *all* content blocks (text, tool_use,
// thinking) sequentially. Consumers expect tool call indices to be 0-based
// contiguous ordinals among tool_use blocks only. This accumulator assigns
// ordinals on `start_tool` and maps block indices on `append_json`.
// ---------------------------------------------------------------------------

// Accumulator types are used by `parse_anthropic_stream_payload` and the
// streaming loop.
mod tool_call_accumulator {
    use std::collections::HashMap;
    use types::ToolCallDelta;

    /// Accumulated state for a single in-flight tool call.
    #[derive(Debug, Default)]
    pub(crate) struct AnthropicToolCallEntry {
        pub(crate) id: Option<String>,
        pub(crate) name: Option<String>,
        pub(crate) arguments: String,
    }

    /// Maintains index-remapping and argument accumulation for Anthropic tool
    /// call streaming.
    ///
    /// Anthropic streams tool call arguments as `input_json_delta` fragments
    /// identified by a content-block index that counts *all* block types. This
    /// accumulator converts those indices to tool-call ordinals (0-based,
    /// counting only `tool_use` blocks) and accumulates the partial JSON
    /// fragments so each emitted [`ToolCallDelta`] carries the full arguments
    /// received so far.
    #[derive(Debug, Default)]
    pub(crate) struct AnthropicToolCallAccumulator {
        /// Maps content block index → accumulated tool call state.
        pub(crate) by_block_index: HashMap<usize, AnthropicToolCallEntry>,
        /// Maps content block index → tool call ordinal (0-based, tool_use
        /// blocks only).
        pub(crate) block_to_ordinal: HashMap<usize, usize>,
        /// Next ordinal to assign when a new tool_use block starts.
        next_ordinal: usize,
    }

    impl AnthropicToolCallAccumulator {
        /// Called on `content_block_start` with `type: "tool_use"`.
        ///
        /// Assigns a tool-call ordinal for this content block index and returns
        /// the initial [`ToolCallDelta`] carrying the id and name (but no
        /// arguments yet).
        pub(crate) fn start_tool(
            &mut self,
            block_index: usize,
            id: Option<String>,
            name: Option<String>,
        ) -> ToolCallDelta {
            let ordinal = self.next_ordinal;
            self.next_ordinal += 1;
            self.block_to_ordinal.insert(block_index, ordinal);
            self.by_block_index.insert(
                block_index,
                AnthropicToolCallEntry {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: String::new(),
                },
            );

            ToolCallDelta {
                index: ordinal,
                id,
                name,
                arguments: None,
                metadata: None,
            }
        }

        /// Called on `content_block_delta` with `type: "input_json_delta"`.
        ///
        /// Appends `partial_json` to the accumulated arguments for the given
        /// content block and returns a [`ToolCallDelta`] with the full
        /// accumulated arguments and the remapped ordinal index.
        ///
        /// If `block_index` was never registered via [`start_tool`], the
        /// fragment is still accumulated under that index (with ordinal assigned
        /// on the fly), making the accumulator tolerant of out-of-order events.
        pub(crate) fn append_json(
            &mut self,
            block_index: usize,
            partial_json: &str,
        ) -> ToolCallDelta {
            let ordinal = *self.block_to_ordinal.entry(block_index).or_insert_with(|| {
                let o = self.next_ordinal;
                self.next_ordinal += 1;
                o
            });

            let entry = self.by_block_index.entry(block_index).or_default();
            entry.arguments.push_str(partial_json);

            ToolCallDelta {
                index: ordinal,
                id: entry.id.clone(),
                name: entry.name.clone(),
                arguments: if partial_json.is_empty() {
                    None
                } else {
                    Some(partial_json.to_owned())
                },
                metadata: None,
            }
        }
    }
}

// Re-export for use by `parse_anthropic_stream_payload` and tests.
pub(crate) use tool_call_accumulator::*;

// ---------------------------------------------------------------------------
// Event-to-StreamItem conversion
// ---------------------------------------------------------------------------

/// Action returned by [`parse_anthropic_stream_payload`] indicating how the
/// streaming loop should proceed.
#[derive(Debug)]
pub(crate) enum AnthropicEventAction {
    /// Zero or more `StreamItem` values to send through the channel.
    Items(Vec<StreamItem>),
    /// The stream completed normally (`message_stop`).
    Done,
    /// A terminal error from the provider; the stream must stop.
    Error(ProviderError),
}

/// Parse a single SSE `data:` payload from an Anthropic streaming response and
/// return the corresponding action.
///
/// The function performs a two-pass parse:
/// 1. Extract the `"type"` field via [`AnthropicStreamEvent`].
/// 2. Re-parse into the appropriate typed struct and map to [`StreamItem`]
///    values.
///
/// `input_tokens` is shared mutable state that stores prompt tokens from
/// `message_start` so they can be combined with output tokens at
/// `message_delta` to compute `total_tokens`.
pub(crate) fn parse_anthropic_stream_payload(
    payload: &str,
    provider: &ProviderId,
    accumulator: &mut AnthropicToolCallAccumulator,
    input_tokens: &mut Option<u64>,
) -> AnthropicEventAction {
    let trimmed = payload.trim();
    if trimmed.is_empty() {
        return AnthropicEventAction::Items(vec![]);
    }

    // First pass: extract event type.
    let envelope: AnthropicStreamEvent = match serde_json::from_str(trimmed) {
        Ok(ev) => ev,
        Err(err) => {
            return AnthropicEventAction::Error(ProviderError::ResponseParse {
                provider: provider.clone(),
                message: format!("failed to parse Anthropic stream event envelope: {err}"),
            });
        }
    };

    match envelope.kind.as_str() {
        "ping" | "content_block_stop" => AnthropicEventAction::Items(vec![]),

        "message_stop" => AnthropicEventAction::Done,

        "message_start" => parse_message_start(trimmed, provider, input_tokens),

        "content_block_start" => parse_content_block_start(trimmed, provider, accumulator),

        "content_block_delta" => parse_content_block_delta(trimmed, provider, accumulator),

        "message_delta" => parse_message_delta(trimmed, provider, input_tokens),

        "error" => parse_error_event(trimmed, provider),

        // Forward-compatible: ignore unknown event types.
        _ => AnthropicEventAction::Items(vec![]),
    }
}

fn parse_message_start(
    payload: &str,
    provider: &ProviderId,
    input_tokens: &mut Option<u64>,
) -> AnthropicEventAction {
    let event: AnthropicMessageStartEvent = match serde_json::from_str(payload) {
        Ok(ev) => ev,
        Err(err) => {
            return AnthropicEventAction::Error(ProviderError::ResponseParse {
                provider: provider.clone(),
                message: format!("failed to parse Anthropic message_start event: {err}"),
            });
        }
    };

    let prompt = event.message.usage.and_then(|u| u.input_tokens);
    if prompt.is_some() {
        *input_tokens = prompt;
    }

    // Emit prompt tokens immediately for mid-stream budget enforcement.
    if prompt.is_some() {
        AnthropicEventAction::Items(vec![StreamItem::UsageUpdate(UsageUpdate {
            prompt_tokens: prompt,
            completion_tokens: None,
            total_tokens: None,
        })])
    } else {
        AnthropicEventAction::Items(vec![])
    }
}

fn parse_content_block_start(
    payload: &str,
    provider: &ProviderId,
    accumulator: &mut AnthropicToolCallAccumulator,
) -> AnthropicEventAction {
    let event: AnthropicContentBlockStartEvent = match serde_json::from_str(payload) {
        Ok(ev) => ev,
        Err(err) => {
            return AnthropicEventAction::Error(ProviderError::ResponseParse {
                provider: provider.clone(),
                message: format!("failed to parse Anthropic content_block_start event: {err}"),
            });
        }
    };

    match event.content_block.kind.as_str() {
        "tool_use" => {
            let delta = accumulator.start_tool(
                event.index,
                event.content_block.id,
                event.content_block.name,
            );
            AnthropicEventAction::Items(vec![StreamItem::ToolCallDelta(delta)])
        }
        // text, thinking, and unknown types: content arrives via deltas.
        _ => AnthropicEventAction::Items(vec![]),
    }
}

fn parse_content_block_delta(
    payload: &str,
    provider: &ProviderId,
    accumulator: &mut AnthropicToolCallAccumulator,
) -> AnthropicEventAction {
    let event: AnthropicContentBlockDeltaEvent = match serde_json::from_str(payload) {
        Ok(ev) => ev,
        Err(err) => {
            return AnthropicEventAction::Error(ProviderError::ResponseParse {
                provider: provider.clone(),
                message: format!("failed to parse Anthropic content_block_delta event: {err}"),
            });
        }
    };

    match event.delta.kind.as_str() {
        "text_delta" => {
            if let Some(text) = event.delta.text.filter(|t| !t.is_empty()) {
                AnthropicEventAction::Items(vec![StreamItem::Text(text)])
            } else {
                AnthropicEventAction::Items(vec![])
            }
        }
        "input_json_delta" => {
            if let Some(partial) = event.delta.partial_json {
                let delta = accumulator.append_json(event.index, &partial);
                AnthropicEventAction::Items(vec![StreamItem::ToolCallDelta(delta)])
            } else {
                AnthropicEventAction::Items(vec![])
            }
        }
        "thinking_delta" => {
            if let Some(thinking) = event.delta.thinking.filter(|t| !t.is_empty()) {
                AnthropicEventAction::Items(vec![StreamItem::ReasoningDelta(thinking)])
            } else {
                AnthropicEventAction::Items(vec![])
            }
        }
        // Forward-compatible: ignore unknown delta types.
        _ => AnthropicEventAction::Items(vec![]),
    }
}

fn parse_message_delta(
    payload: &str,
    provider: &ProviderId,
    input_tokens: &mut Option<u64>,
) -> AnthropicEventAction {
    let event: AnthropicMessageDeltaEvent = match serde_json::from_str(payload) {
        Ok(ev) => ev,
        Err(err) => {
            return AnthropicEventAction::Error(ProviderError::ResponseParse {
                provider: provider.clone(),
                message: format!("failed to parse Anthropic message_delta event: {err}"),
            });
        }
    };

    let mut items = Vec::new();

    // Emit FinishReason if present and non-empty.
    if let Some(reason) = event.delta.stop_reason.filter(|r| !r.is_empty()) {
        items.push(StreamItem::FinishReason(reason));
    }

    // Emit UsageUpdate combining stored input_tokens with output_tokens.
    if let Some(delta_usage) = &event.usage {
        let output = delta_usage.output_tokens;
        let stored_input = *input_tokens;
        let total = match (stored_input, output) {
            (Some(i), Some(o)) => Some(i.saturating_add(o)),
            _ => None,
        };
        items.push(StreamItem::UsageUpdate(UsageUpdate {
            prompt_tokens: stored_input,
            completion_tokens: output,
            total_tokens: total,
        }));
    }

    AnthropicEventAction::Items(items)
}

fn parse_error_event(payload: &str, provider: &ProviderId) -> AnthropicEventAction {
    let event: AnthropicStreamErrorEvent = match serde_json::from_str(payload) {
        Ok(ev) => ev,
        Err(err) => {
            return AnthropicEventAction::Error(ProviderError::ResponseParse {
                provider: provider.clone(),
                message: format!("failed to parse Anthropic error event: {err}"),
            });
        }
    };

    let message = event
        .error
        .message
        .unwrap_or_else(|| "unknown streaming error from Anthropic".to_owned());

    AnthropicEventAction::Error(ProviderError::ResponseParse {
        provider: provider.clone(),
        message,
    })
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
                    metadata: None,
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

#[cfg(test)]
mod sse_event_deser_tests {
    use super::*;

    #[test]
    fn stream_event_envelope_extracts_type() {
        let json = r#"{"type":"message_start","message":{"usage":{}}}"#;
        let ev: AnthropicStreamEvent = serde_json::from_str(json).unwrap();
        assert_eq!(ev.kind, "message_start");
    }

    #[test]
    fn message_start_with_usage() {
        let json = r#"{"type":"message_start","message":{"usage":{"input_tokens":42}}}"#;
        let ev: AnthropicMessageStartEvent = serde_json::from_str(json).unwrap();
        let usage = ev.message.usage.unwrap();
        assert_eq!(usage.input_tokens, Some(42));
        assert_eq!(usage.output_tokens, None);
    }

    #[test]
    fn message_start_without_usage() {
        let json = r#"{"type":"message_start","message":{}}"#;
        let ev: AnthropicMessageStartEvent = serde_json::from_str(json).unwrap();
        assert!(ev.message.usage.is_none());
    }

    #[test]
    fn content_block_start_text() {
        let json =
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#;
        let ev: AnthropicContentBlockStartEvent = serde_json::from_str(json).unwrap();
        assert_eq!(ev.index, 0);
        assert_eq!(ev.content_block.kind, "text");
        assert!(ev.content_block.id.is_none());
        assert!(ev.content_block.name.is_none());
    }

    #[test]
    fn content_block_start_tool_use() {
        let json = r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_123","name":"get_weather"}}"#;
        let ev: AnthropicContentBlockStartEvent = serde_json::from_str(json).unwrap();
        assert_eq!(ev.index, 1);
        assert_eq!(ev.content_block.kind, "tool_use");
        assert_eq!(ev.content_block.id.as_deref(), Some("toolu_123"));
        assert_eq!(ev.content_block.name.as_deref(), Some("get_weather"));
    }

    #[test]
    fn content_block_start_thinking() {
        let json =
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking"}}"#;
        let ev: AnthropicContentBlockStartEvent = serde_json::from_str(json).unwrap();
        assert_eq!(ev.content_block.kind, "thinking");
    }

    #[test]
    fn content_block_delta_text() {
        let json = r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#;
        let ev: AnthropicContentBlockDeltaEvent = serde_json::from_str(json).unwrap();
        assert_eq!(ev.index, 0);
        assert_eq!(ev.delta.kind, "text_delta");
        assert_eq!(ev.delta.text.as_deref(), Some("Hello"));
        assert!(ev.delta.partial_json.is_none());
        assert!(ev.delta.thinking.is_none());
    }

    #[test]
    fn content_block_delta_input_json() {
        let json = r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"loc"}}"#;
        let ev: AnthropicContentBlockDeltaEvent = serde_json::from_str(json).unwrap();
        assert_eq!(ev.delta.kind, "input_json_delta");
        assert_eq!(ev.delta.partial_json.as_deref(), Some("{\"loc"));
    }

    #[test]
    fn content_block_delta_thinking() {
        let json = r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Let me think..."}}"#;
        let ev: AnthropicContentBlockDeltaEvent = serde_json::from_str(json).unwrap();
        assert_eq!(ev.delta.kind, "thinking_delta");
        assert_eq!(ev.delta.thinking.as_deref(), Some("Let me think..."));
    }

    #[test]
    fn message_delta_with_stop_reason_and_usage() {
        let json = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":15}}"#;
        let ev: AnthropicMessageDeltaEvent = serde_json::from_str(json).unwrap();
        assert_eq!(ev.delta.stop_reason.as_deref(), Some("end_turn"));
        let usage = ev.usage.unwrap();
        assert_eq!(usage.output_tokens, Some(15));
    }

    #[test]
    fn message_delta_null_stop_reason() {
        let json =
            r#"{"type":"message_delta","delta":{"stop_reason":null},"usage":{"output_tokens":5}}"#;
        let ev: AnthropicMessageDeltaEvent = serde_json::from_str(json).unwrap();
        assert!(ev.delta.stop_reason.is_none());
    }

    #[test]
    fn message_delta_without_usage() {
        let json = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#;
        let ev: AnthropicMessageDeltaEvent = serde_json::from_str(json).unwrap();
        assert!(ev.usage.is_none());
    }

    #[test]
    fn stream_error_event() {
        let json = r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;
        let ev: AnthropicStreamErrorEvent = serde_json::from_str(json).unwrap();
        assert_eq!(ev.error.message.as_deref(), Some("Overloaded"));
    }

    #[test]
    fn stream_error_without_message() {
        let json = r#"{"type":"error","error":{"type":"server_error"}}"#;
        let ev: AnthropicStreamErrorEvent = serde_json::from_str(json).unwrap();
        assert!(ev.error.message.is_none());
    }

    #[test]
    fn unknown_fields_are_tolerated() {
        // serde ignores unknown fields by default (no #[serde(deny_unknown_fields)])
        let json = r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi","future_field":true},"extra":42}"#;
        let ev: AnthropicContentBlockDeltaEvent = serde_json::from_str(json).unwrap();
        assert_eq!(ev.delta.text.as_deref(), Some("hi"));
    }

    #[test]
    fn ping_event_parses() {
        let json = r#"{"type":"ping"}"#;
        let ev: AnthropicStreamEvent = serde_json::from_str(json).unwrap();
        assert_eq!(ev.kind, "ping");
    }

    #[test]
    fn message_stop_parses() {
        let json = r#"{"type":"message_stop"}"#;
        let ev: AnthropicStreamEvent = serde_json::from_str(json).unwrap();
        assert_eq!(ev.kind, "message_stop");
    }

    #[test]
    fn content_block_stop_parses() {
        let json = r#"{"type":"content_block_stop","index":0}"#;
        let ev: AnthropicStreamEvent = serde_json::from_str(json).unwrap();
        assert_eq!(ev.kind, "content_block_stop");
    }
}

#[cfg(test)]
mod tool_call_accumulator_tests {
    use super::*;

    #[test]
    fn start_tool_assigns_sequential_ordinals() {
        let mut acc = AnthropicToolCallAccumulator::default();

        let delta0 = acc.start_tool(1, Some("id_a".into()), Some("func_a".into()));
        assert_eq!(delta0.index, 0);
        assert_eq!(delta0.id.as_deref(), Some("id_a"));
        assert_eq!(delta0.name.as_deref(), Some("func_a"));
        assert!(delta0.arguments.is_none());

        let delta1 = acc.start_tool(3, Some("id_b".into()), Some("func_b".into()));
        assert_eq!(delta1.index, 1);
        assert_eq!(delta1.id.as_deref(), Some("id_b"));
        assert_eq!(delta1.name.as_deref(), Some("func_b"));
        assert!(delta1.arguments.is_none());
    }

    #[test]
    fn append_json_accumulates_arguments() {
        let mut acc = AnthropicToolCallAccumulator::default();
        acc.start_tool(1, Some("id_a".into()), Some("func_a".into()));

        let delta1 = acc.append_json(1, r#"{"lo"#);
        assert_eq!(delta1.index, 0);
        assert_eq!(delta1.arguments.as_deref(), Some(r#"{"lo"#));

        let delta2 = acc.append_json(1, r#"c":"NYC"}"#);
        assert_eq!(delta2.index, 0);
        assert_eq!(delta2.arguments.as_deref(), Some(r#"c":"NYC"}"#));
        // id and name are preserved across appends.
        assert_eq!(delta2.id.as_deref(), Some("id_a"));
        assert_eq!(delta2.name.as_deref(), Some("func_a"));
    }

    #[test]
    fn index_remapping_skips_non_tool_blocks() {
        // Simulates: text@0, tool_use@1, text@2, tool_use@3
        let mut acc = AnthropicToolCallAccumulator::default();

        let d0 = acc.start_tool(1, Some("t1".into()), Some("fn1".into()));
        assert_eq!(d0.index, 0, "first tool_use should be ordinal 0");

        let d1 = acc.start_tool(3, Some("t2".into()), Some("fn2".into()));
        assert_eq!(d1.index, 1, "second tool_use should be ordinal 1");

        // Arguments for block 1 map to ordinal 0.
        let a0 = acc.append_json(1, r#"{"a":1}"#);
        assert_eq!(a0.index, 0);
        assert_eq!(a0.arguments.as_deref(), Some(r#"{"a":1}"#));

        // Arguments for block 3 map to ordinal 1.
        let a1 = acc.append_json(3, r#"{"b":2}"#);
        assert_eq!(a1.index, 1);
        assert_eq!(a1.arguments.as_deref(), Some(r#"{"b":2}"#));
    }

    #[test]
    fn append_json_without_prior_start_tool_assigns_ordinal() {
        // Edge case: delta arrives before start (out-of-order tolerance).
        let mut acc = AnthropicToolCallAccumulator::default();
        let d = acc.append_json(5, r#"{"x":1}"#);
        assert_eq!(d.index, 0, "should get ordinal 0 as first seen tool block");
        assert!(d.id.is_none());
        assert!(d.name.is_none());
        assert_eq!(d.arguments.as_deref(), Some(r#"{"x":1}"#));
    }

    #[test]
    fn multiple_tools_accumulate_independently() {
        let mut acc = AnthropicToolCallAccumulator::default();
        acc.start_tool(0, Some("t0".into()), Some("read".into()));
        acc.start_tool(2, Some("t1".into()), Some("write".into()));

        // Interleaved appends.
        let a = acc.append_json(0, r#"{"p"#);
        assert_eq!(a.index, 0);
        assert_eq!(a.arguments.as_deref(), Some(r#"{"p"#));

        let b = acc.append_json(2, r#"{"q"#);
        assert_eq!(b.index, 1);
        assert_eq!(b.arguments.as_deref(), Some(r#"{"q"#));

        let a2 = acc.append_json(0, r#"ath":"/"}"#);
        assert_eq!(a2.index, 0);
        assert_eq!(a2.arguments.as_deref(), Some(r#"ath":"/"}"#));

        let b2 = acc.append_json(2, r#"":"v"}"#);
        assert_eq!(b2.index, 1);
        assert_eq!(b2.arguments.as_deref(), Some(r#"":"v"}"#));
    }

    #[test]
    fn default_accumulator_is_empty() {
        let acc = AnthropicToolCallAccumulator::default();
        assert!(acc.by_block_index.is_empty());
        assert!(acc.block_to_ordinal.is_empty());
    }
}
