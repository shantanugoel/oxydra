use std::collections::BTreeMap;

use async_trait::async_trait;
use base64::Engine;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;
use types::{
    Context, FunctionDecl, InlineMedia, Message, MessageRole, ModelCatalog, Provider,
    ProviderError, ProviderId, ProviderStream, Response, StreamItem, ToolCall, ToolCallDelta,
    UsageUpdate,
};

use crate::{
    DEFAULT_STREAM_BUFFER_SIZE, GEMINI_DEFAULT_BASE_URL, extract_http_error_message,
    normalize_base_url_or_default, openai::SseDataParser, validate_context_attachments,
};

#[derive(Debug, Clone)]
pub struct GeminiProvider {
    client: Client,
    provider_id: ProviderId,
    catalog_provider_id: ProviderId,
    model_catalog: ModelCatalog,
    base_url: String,
    api_key: String,
    extra_headers: BTreeMap<String, String>,
}

impl GeminiProvider {
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
            base_url: normalize_base_url_or_default(&base_url, GEMINI_DEFAULT_BASE_URL),
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
        validate_context_attachments(
            context,
            &self.provider_id,
            &self.catalog_provider_id,
            &self.model_catalog,
            &[
                types::InputModality::Image,
                types::InputModality::Audio,
                types::InputModality::Video,
                types::InputModality::Pdf,
                types::InputModality::Document,
            ],
        )?;
        Ok(())
    }

    fn generate_content_url(&self, model: &str) -> String {
        format!("{}/v1beta/models/{}:generateContent", self.base_url, model)
    }

    fn stream_generate_content_url(&self, model: &str) -> String {
        format!(
            "{}/v1beta/models/{}:streamGenerateContent?alt=sse",
            self.base_url, model
        )
    }

    fn authenticated_request(&self, url: &str) -> reqwest::RequestBuilder {
        let mut builder = self
            .client
            .post(url)
            .header("x-goog-api-key", &self.api_key);
        for (key, value) in &self.extra_headers {
            builder = builder.header(key, value);
        }
        builder
    }
}

#[async_trait]
impl Provider for GeminiProvider {
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
            "sending Gemini generateContent request"
        );

        let request = GeminiGenerateContentRequest::from_context(context)?;
        let http_response = self
            .authenticated_request(&self.generate_content_url(&context.model.0))
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

        let response: GeminiGenerateContentResponse =
            http_response
                .json()
                .await
                .map_err(|error| ProviderError::ResponseParse {
                    provider: self.provider_id.clone(),
                    message: error.to_string(),
                })?;
        normalize_gemini_response(response, &self.provider_id)
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
            "sending Gemini streamGenerateContent request"
        );

        let request = GeminiGenerateContentRequest::from_context(context)?;
        let mut http_response = self
            .authenticated_request(&self.stream_generate_content_url(&context.model.0))
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
            let mut received_finish = false;

            loop {
                let chunk = match http_response.chunk().await {
                    Ok(Some(chunk)) => chunk,
                    Ok(None) => break,
                    Err(error) => {
                        let _ = sender
                            .send(Ok(StreamItem::ConnectionLost(format!(
                                "Gemini stream transport dropped: {error}"
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

                if process_gemini_payloads(&payloads, &sender, &provider, &mut received_finish)
                    .await
                {
                    return;
                }
            }

            // Flush remaining buffered data from the parser.
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

            if process_gemini_payloads(&payloads, &sender, &provider, &mut received_finish).await {
                return;
            }

            // EOF without a finish reason — connection lost.
            if !received_finish {
                let _ = sender
                    .send(Ok(StreamItem::ConnectionLost(
                        "Gemini stream ended before finish_reason".to_owned(),
                    )))
                    .await;
            }
        });

        Ok(receiver)
    }
}

/// Process a batch of SSE payload strings through Gemini chunk parsing and
/// send resulting items through the channel.
///
/// Returns `true` when the caller should exit the streaming loop.
async fn process_gemini_payloads(
    payloads: &[String],
    sender: &mpsc::Sender<Result<StreamItem, ProviderError>>,
    provider: &ProviderId,
    received_finish: &mut bool,
) -> bool {
    for payload in payloads {
        match parse_gemini_stream_payload(payload, provider) {
            Ok(items) => {
                for item in items {
                    if matches!(&item, StreamItem::FinishReason(_)) {
                        *received_finish = true;
                    }
                    if sender.send(Ok(item)).await.is_err() {
                        return true;
                    }
                }
            }
            Err(error) => {
                let _ = sender.send(Err(error)).await;
                return true;
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Request types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub(crate) struct GeminiGenerateContentRequest {
    contents: Vec<GeminiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system_instruction: Option<GeminiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<GeminiToolDeclaration>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    generation_config: Option<GeminiGenerationConfig>,
}

impl GeminiGenerateContentRequest {
    pub(crate) fn from_context(context: &Context) -> Result<Self, ProviderError> {
        let mut tool_call_names = BTreeMap::new();
        for message in &context.messages {
            if message.role == MessageRole::Assistant {
                for tool_call in &message.tool_calls {
                    tool_call_names.insert(tool_call.id.clone(), tool_call.name.clone());
                }
            }
        }
        let mut system_parts = Vec::new();
        let mut contents = Vec::new();

        for message in &context.messages {
            match message.role {
                MessageRole::System => {
                    if let Some(text) = message.content.as_deref().filter(|t| !t.trim().is_empty())
                    {
                        system_parts.push(GeminiPart::text(text));
                    }
                }
                MessageRole::User => {
                    let mut parts = Vec::new();
                    if let Some(text) = message.content.as_deref().filter(|t| !t.trim().is_empty())
                    {
                        parts.push(GeminiPart::text(text));
                    }
                    for attachment in &message.attachments {
                        parts.push(GeminiPart::inline_data(
                            &attachment.mime_type,
                            &attachment.data,
                        ));
                    }
                    if !parts.is_empty() {
                        contents.push(GeminiContent {
                            role: "user".to_owned(),
                            parts,
                        });
                    }
                }
                MessageRole::Assistant => {
                    let mut parts = Vec::new();
                    if let Some(text) = message.content.as_deref().filter(|t| !t.trim().is_empty())
                    {
                        parts.push(GeminiPart::text(text));
                    }
                    for tool_call in &message.tool_calls {
                        let thought_signature = tool_call
                            .metadata
                            .as_ref()
                            .and_then(|m| m.get("thought_signature"))
                            .and_then(|v| v.as_str());
                        parts.push(GeminiPart::function_call(
                            &tool_call.name,
                            &tool_call.arguments,
                            thought_signature,
                        ));
                    }
                    if !parts.is_empty() {
                        contents.push(GeminiContent {
                            role: "model".to_owned(),
                            parts,
                        });
                    }
                }
                MessageRole::Tool => {
                    let name = message
                        .tool_call_id
                        .as_deref()
                        .and_then(|id| tool_call_names.get(id))
                        .cloned()
                        .or_else(|| message.tool_call_id.clone())
                        .unwrap_or_else(|| "unknown_function".to_owned());
                    let response_value = message
                        .content
                        .as_deref()
                        .and_then(|c| serde_json::from_str::<Value>(c).ok())
                        .and_then(|v| v.is_object().then_some(v))
                        .unwrap_or_else(|| {
                            // Gemini requires functionResponse.response to be a JSON object
                            // (google.protobuf.Struct). Wrap any non-object value — including
                            // plain-text error messages — so the API never sees a bare string.
                            serde_json::json!({
                                "result": message.content.clone().unwrap_or_default()
                            })
                        });
                    contents.push(GeminiContent {
                        role: "user".to_owned(),
                        parts: vec![GeminiPart::function_response(&name, response_value)],
                    });
                }
            }
        }

        let system_instruction = if system_parts.is_empty() {
            None
        } else {
            Some(GeminiContent {
                role: "user".to_owned(),
                parts: system_parts,
            })
        };

        let tools = if context.tools.is_empty() {
            None
        } else {
            let function_declarations: Vec<GeminiFunctionDeclaration> = context
                .tools
                .iter()
                .map(GeminiFunctionDeclaration::from)
                .collect();
            Some(vec![GeminiToolDeclaration {
                function_declarations,
            }])
        };

        if contents.is_empty() {
            return Err(ProviderError::RequestFailed {
                provider: context.provider.clone(),
                message: "Gemini request requires at least one non-system message".to_owned(),
            });
        }

        Ok(Self {
            contents,
            system_instruction,
            tools,
            generation_config: None,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct GeminiContent {
    pub(crate) role: String,
    pub(crate) parts: Vec<GeminiPart>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct GeminiPart {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) text: Option<String>,
    #[serde(rename = "functionCall", skip_serializing_if = "Option::is_none")]
    pub(crate) function_call: Option<GeminiFunctionCall>,
    #[serde(rename = "functionResponse", skip_serializing_if = "Option::is_none")]
    pub(crate) function_response: Option<GeminiFunctionResponse>,
    /// Opaque token included by Gemini 2.5 thinking models at the Part level.
    /// Must be echoed back verbatim when replaying the conversation history or
    /// Gemini returns HTTP 400 "missing thought_signature".
    #[serde(
        rename = "thoughtSignature",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) thought_signature: Option<String>,
    #[serde(rename = "inlineData", skip_serializing_if = "Option::is_none")]
    pub(crate) inline_data: Option<GeminiInlineData>,
}

impl GeminiPart {
    fn text(text: &str) -> Self {
        Self {
            text: Some(text.to_owned()),
            function_call: None,
            function_response: None,
            thought_signature: None,
            inline_data: None,
        }
    }

    fn function_call(name: &str, args: &Value, thought_signature: Option<&str>) -> Self {
        Self {
            text: None,
            function_call: Some(GeminiFunctionCall {
                name: name.to_owned(),
                args: args.clone(),
            }),
            function_response: None,
            thought_signature: thought_signature.map(ToOwned::to_owned),
            inline_data: None,
        }
    }

    fn function_response(name: &str, response: Value) -> Self {
        Self {
            text: None,
            function_call: None,
            function_response: Some(GeminiFunctionResponse {
                name: name.to_owned(),
                response,
            }),
            thought_signature: None,
            inline_data: None,
        }
    }

    fn inline_data(mime_type: &str, data: &[u8]) -> Self {
        use base64::Engine;
        Self {
            text: None,
            function_call: None,
            function_response: None,
            thought_signature: None,
            inline_data: Some(GeminiInlineData {
                mime_type: mime_type.to_owned(),
                data: base64::engine::general_purpose::STANDARD.encode(data),
            }),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct GeminiInlineData {
    #[serde(rename = "mimeType")]
    pub(crate) mime_type: String,
    pub(crate) data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct GeminiFunctionCall {
    pub(crate) name: String,
    pub(crate) args: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct GeminiFunctionResponse {
    pub(crate) name: String,
    pub(crate) response: Value,
}

#[derive(Debug, Serialize)]
struct GeminiToolDeclaration {
    function_declarations: Vec<GeminiFunctionDeclaration>,
}

#[derive(Debug, Serialize)]
struct GeminiFunctionDeclaration {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    parameters: serde_json::Value,
}

/// Recursively sanitize a schema for Gemini: strip `additionalProperties` because
/// Gemini's function-calling API does not support that keyword.
fn sanitize_schema_for_gemini(schema: &mut serde_json::Value) {
    let Some(obj) = schema.as_object_mut() else {
        return;
    };
    obj.remove("additionalProperties");

    // Recurse into properties
    if let Some(props) = obj.get_mut("properties")
        && let Some(props_obj) = props.as_object_mut()
    {
        for v in props_obj.values_mut() {
            sanitize_schema_for_gemini(v);
        }
    }

    // Recurse into array items
    if let Some(items) = obj.get_mut("items") {
        sanitize_schema_for_gemini(items);
    }
}

impl From<&FunctionDecl> for GeminiFunctionDeclaration {
    fn from(value: &FunctionDecl) -> Self {
        let mut parameters = value.parameters.clone();
        sanitize_schema_for_gemini(&mut parameters);
        Self {
            name: value.name.clone(),
            description: value.description.clone(),
            parameters,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct GeminiGenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(rename = "maxOutputTokens", skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
}

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(crate) struct GeminiGenerateContentResponse {
    #[serde(default)]
    pub(crate) candidates: Vec<GeminiCandidate>,
    #[serde(default, rename = "usageMetadata")]
    pub(crate) usage_metadata: Option<GeminiUsageMetadata>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GeminiCandidate {
    #[serde(default)]
    pub(crate) content: Option<GeminiContent>,
    #[serde(default, rename = "finishReason")]
    pub(crate) finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GeminiUsageMetadata {
    #[serde(default, rename = "promptTokenCount")]
    pub(crate) prompt_token_count: Option<u64>,
    #[serde(default, rename = "candidatesTokenCount")]
    pub(crate) candidates_token_count: Option<u64>,
    #[serde(default, rename = "totalTokenCount")]
    pub(crate) total_token_count: Option<u64>,
}

// ---------------------------------------------------------------------------
// Error envelope
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(crate) struct GeminiErrorEnvelope {
    #[serde(default)]
    pub(crate) error: Option<GeminiErrorBody>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GeminiErrorBody {
    #[serde(default)]
    pub(crate) message: Option<String>,
}

// ---------------------------------------------------------------------------
// Response normalization
// ---------------------------------------------------------------------------

pub(crate) fn normalize_gemini_response(
    response: GeminiGenerateContentResponse,
    provider: &ProviderId,
) -> Result<Response, ProviderError> {
    let candidate =
        response
            .candidates
            .into_iter()
            .next()
            .ok_or_else(|| ProviderError::ResponseParse {
                provider: provider.clone(),
                message: "Gemini response did not contain any candidates".to_owned(),
            })?;

    let mut text_chunks = Vec::new();
    let mut tool_calls = Vec::new();
    let mut attachments = Vec::new();

    if let Some(content) = &candidate.content {
        for part in &content.parts {
            if let Some(text) = &part.text
                && !text.is_empty()
            {
                text_chunks.push(text.clone());
            }
            if let Some(fc) = &part.function_call {
                let metadata = part
                    .thought_signature
                    .as_deref()
                    .map(|sig| serde_json::json!({ "thought_signature": sig }));
                tool_calls.push(ToolCall {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: fc.name.clone(),
                    arguments: fc.args.clone(),
                    metadata,
                });
            }
            if let Some(inline_data) = &part.inline_data {
                let data = base64::engine::general_purpose::STANDARD
                    .decode(&inline_data.data)
                    .map_err(|error| ProviderError::ResponseParse {
                        provider: provider.clone(),
                        message: format!("failed to decode Gemini inlineData attachment: {error}"),
                    })?;
                attachments.push(InlineMedia {
                    mime_type: inline_data.mime_type.clone(),
                    data,
                });
            }
        }
    }

    let usage = response.usage_metadata.map(|u| UsageUpdate {
        prompt_tokens: u.prompt_token_count,
        completion_tokens: u.candidates_token_count,
        total_tokens: u.total_token_count,
    });

    let message = Message {
        role: MessageRole::Assistant,
        content: if text_chunks.is_empty() {
            None
        } else {
            Some(text_chunks.join(""))
        },
        tool_calls: tool_calls.clone(),
        tool_call_id: None,
        attachments,
    };

    Ok(Response {
        tool_calls,
        message,
        finish_reason: candidate.finish_reason,
        usage,
    })
}

// ---------------------------------------------------------------------------
// Stream chunk parsing
// ---------------------------------------------------------------------------

/// Parse a single SSE `data:` payload from a Gemini streaming response and
/// return the corresponding `StreamItem` values.
///
/// Each Gemini SSE event contains a complete `GeminiGenerateContentResponse`
/// JSON object. Unlike OpenAI, Gemini sends function calls atomically (not
/// fragmented), so each function_call part yields a single complete
/// `ToolCallDelta`.
pub(crate) fn parse_gemini_stream_payload(
    payload: &str,
    provider: &ProviderId,
) -> Result<Vec<StreamItem>, ProviderError> {
    let trimmed = payload.trim();
    if trimmed.is_empty() {
        return Ok(vec![]);
    }

    let chunk: GeminiGenerateContentResponse =
        serde_json::from_str(trimmed).map_err(|error| ProviderError::ResponseParse {
            provider: provider.clone(),
            message: format!("failed to parse Gemini streaming payload: {error}"),
        })?;

    normalize_gemini_stream_chunk(chunk)
}

/// Converts a parsed `GeminiGenerateContentResponse` chunk into `StreamItem` values.
pub(crate) fn normalize_gemini_stream_chunk(
    chunk: GeminiGenerateContentResponse,
) -> Result<Vec<StreamItem>, ProviderError> {
    let mut items = Vec::new();
    let mut tool_call_index = 0;

    for candidate in &chunk.candidates {
        if let Some(content) = &candidate.content {
            for part in &content.parts {
                if let Some(text) = &part.text
                    && !text.is_empty()
                {
                    items.push(StreamItem::Text(text.clone()));
                }
                if let Some(fc) = &part.function_call {
                    let id = uuid::Uuid::new_v4().to_string();
                    let metadata = part
                        .thought_signature
                        .as_deref()
                        .map(|sig| serde_json::json!({ "thought_signature": sig }));
                    items.push(StreamItem::ToolCallDelta(ToolCallDelta {
                        index: tool_call_index,
                        id: Some(id),
                        name: Some(fc.name.clone()),
                        arguments: Some(serde_json::to_string(&fc.args).unwrap_or_default()),
                        metadata,
                    }));
                    tool_call_index += 1;
                }
            }
        }

        if let Some(reason) = &candidate.finish_reason
            && !reason.is_empty()
        {
            items.push(StreamItem::FinishReason(reason.clone()));
        }
    }

    if let Some(usage) = &chunk.usage_metadata {
        items.push(StreamItem::UsageUpdate(UsageUpdate {
            prompt_tokens: usage.prompt_token_count,
            completion_tokens: usage.candidates_token_count,
            total_tokens: usage.total_token_count,
        }));
    }

    Ok(items)
}
