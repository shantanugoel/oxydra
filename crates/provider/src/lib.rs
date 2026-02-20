use std::{collections::HashMap, env, sync::Arc, time::Duration};

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
const ANTHROPIC_PROVIDER_ID: &str = "anthropic";
const ANTHROPIC_DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_MESSAGES_PATH: &str = "/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_ANTHROPIC_MAX_TOKENS: u32 = 1024;
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

#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub backoff_base: Duration,
    pub backoff_max: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            backoff_base: Duration::from_millis(250),
            backoff_max: Duration::from_secs(2),
        }
    }
}

impl RetryPolicy {
    fn normalized(self) -> Self {
        let max_attempts = self.max_attempts.max(1);
        let backoff_base = if self.backoff_base.is_zero() {
            Duration::from_millis(1)
        } else {
            self.backoff_base
        };
        let backoff_max = if self.backoff_max < backoff_base {
            backoff_base
        } else {
            self.backoff_max
        };
        Self {
            max_attempts,
            backoff_base,
            backoff_max,
        }
    }

    fn backoff_delay_for_attempt(&self, attempt: u32) -> Duration {
        let shift = attempt.saturating_sub(1).min(31);
        let factor = 1_u128 << shift;
        let base = self.backoff_base.as_millis();
        let max = self.backoff_max.as_millis();
        let delay_ms = (base.saturating_mul(factor)).min(max);
        let delay_ms_u64 = u64::try_from(delay_ms).unwrap_or(u64::MAX);
        Duration::from_millis(delay_ms_u64)
    }
}

pub struct ReliableProvider {
    inner: Arc<dyn Provider>,
    retry_policy: RetryPolicy,
}

impl ReliableProvider {
    pub fn with_defaults(inner: Box<dyn Provider>) -> Self {
        Self::new(inner, RetryPolicy::default())
    }

    pub fn new(inner: Box<dyn Provider>, retry_policy: RetryPolicy) -> Self {
        Self::from_arc(Arc::from(inner), retry_policy)
    }

    pub fn from_arc(inner: Arc<dyn Provider>, retry_policy: RetryPolicy) -> Self {
        Self {
            inner,
            retry_policy: retry_policy.normalized(),
        }
    }

    pub fn retry_policy(&self) -> RetryPolicy {
        self.retry_policy
    }

    async fn complete_with_retry(&self, context: &Context) -> Result<Response, ProviderError> {
        let mut attempt = 1_u32;
        loop {
            match self.inner.complete(context).await {
                Ok(response) => return Ok(response),
                Err(error) => {
                    if !is_retriable_provider_error(&error)
                        || attempt >= self.retry_policy.max_attempts
                    {
                        return Err(error);
                    }

                    let delay = self.retry_policy.backoff_delay_for_attempt(attempt);
                    tracing::warn!(
                        provider = %self.inner.provider_id(),
                        attempt,
                        max_attempts = self.retry_policy.max_attempts,
                        delay_ms = delay.as_millis() as u64,
                        error = %error,
                        "retrying provider completion after transient failure"
                    );
                    attempt += 1;
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }
}

#[async_trait]
impl Provider for ReliableProvider {
    fn provider_id(&self) -> &ProviderId {
        self.inner.provider_id()
    }

    fn model_catalog(&self) -> &ModelCatalog {
        self.inner.model_catalog()
    }

    async fn complete(&self, context: &Context) -> Result<Response, ProviderError> {
        self.complete_with_retry(context).await
    }

    async fn stream(
        &self,
        context: &Context,
        buffer_size: usize,
    ) -> Result<ProviderStream, ProviderError> {
        let channel_size = if buffer_size == 0 {
            DEFAULT_STREAM_BUFFER_SIZE
        } else {
            buffer_size
        };
        let (sender, receiver) = mpsc::channel(channel_size);
        let context = context.clone();
        let inner = Arc::clone(&self.inner);
        let retry_policy = self.retry_policy;

        tokio::spawn(async move {
            let mut attempt = 1_u32;
            'retry_stream: loop {
                let mut stream = match inner.stream(&context, buffer_size).await {
                    Ok(stream) => stream,
                    Err(error) => {
                        if !is_retriable_provider_error(&error)
                            || attempt >= retry_policy.max_attempts
                        {
                            let _ = sender.send(Err(error)).await;
                            return;
                        }

                        let delay = retry_policy.backoff_delay_for_attempt(attempt);
                        tracing::warn!(
                            provider = %inner.provider_id(),
                            attempt,
                            max_attempts = retry_policy.max_attempts,
                            delay_ms = delay.as_millis() as u64,
                            error = %error,
                            "retrying provider stream setup after transient failure"
                        );
                        attempt += 1;
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                };

                while let Some(item) = stream.recv().await {
                    match item {
                        Ok(StreamItem::ConnectionLost(message)) => {
                            if attempt >= retry_policy.max_attempts {
                                let _ = sender.send(Ok(StreamItem::ConnectionLost(message))).await;
                                return;
                            }

                            let delay = retry_policy.backoff_delay_for_attempt(attempt);
                            tracing::warn!(
                                provider = %inner.provider_id(),
                                attempt,
                                max_attempts = retry_policy.max_attempts,
                                delay_ms = delay.as_millis() as u64,
                                message = %message,
                                "retrying provider stream after connection loss"
                            );
                            attempt += 1;
                            tokio::time::sleep(delay).await;
                            continue 'retry_stream;
                        }
                        other => {
                            if sender.send(other).await.is_err() {
                                return;
                            }
                        }
                    }
                }

                return;
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

#[derive(Debug, Serialize)]
struct AnthropicMessagesRequest {
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
    fn from_context(context: &Context, max_tokens: u32) -> Result<Self, ProviderError> {
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
struct AnthropicMessagesResponse {
    role: String,
    content: Vec<AnthropicResponseContentBlock>,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    usage: Option<AnthropicUsage>,
}

#[derive(Debug, Deserialize)]
struct AnthropicResponseContentBlock {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    input: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct AnthropicUsage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct AnthropicErrorEnvelope {
    #[serde(default)]
    error: Option<AnthropicErrorBody>,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AnthropicErrorBody {
    #[serde(default)]
    message: Option<String>,
}

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

fn message_role_to_openai_role(role: &MessageRole) -> &'static str {
    match role {
        MessageRole::System => "system",
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::Tool => "tool",
    }
}

fn message_role_to_anthropic_role(role: &MessageRole) -> &'static str {
    match role {
        MessageRole::User | MessageRole::Tool => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::System => "user",
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

fn anthropic_role_to_message_role(role: &str) -> Option<MessageRole> {
    match role {
        "user" => Some(MessageRole::User),
        "assistant" => Some(MessageRole::Assistant),
        _ => None,
    }
}

fn is_retriable_provider_error(error: &ProviderError) -> bool {
    match error {
        ProviderError::Transport { .. } => true,
        ProviderError::HttpStatus { status, .. } => *status == 429 || *status >= 500,
        _ => false,
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

fn normalize_anthropic_response(
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
    if let Ok(parsed) = serde_json::from_str::<AnthropicErrorEnvelope>(body) {
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
mod tests {
    use std::{
        collections::VecDeque,
        io::{Read, Write},
        net::TcpListener,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
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
    fn default_anthropic_config_uses_anthropic_base_url() {
        let config = AnthropicConfig::default();
        assert_eq!(config.base_url, ANTHROPIC_DEFAULT_BASE_URL);
        assert_eq!(config.max_tokens, DEFAULT_ANTHROPIC_MAX_TOKENS);
    }

    #[test]
    fn anthropic_api_key_resolution_uses_expected_precedence() {
        let resolved = resolve_api_key_from_sources(
            Some("explicit".to_owned()),
            Some("anthropic-env".to_owned()),
            Some("fallback".to_owned()),
        );
        assert_eq!(resolved.as_deref(), Some("explicit"));

        let resolved = resolve_api_key_from_sources(
            None,
            Some("anthropic-env".to_owned()),
            Some("fallback".to_owned()),
        );
        assert_eq!(resolved.as_deref(), Some("anthropic-env"));

        let resolved = resolve_api_key_from_sources(None, None, Some("fallback".to_owned()));
        assert_eq!(resolved.as_deref(), Some("fallback"));
    }

    #[test]
    fn anthropic_request_normalization_snapshot_is_stable() {
        let context = Context {
            provider: ProviderId::from("anthropic"),
            model: ModelId::from("claude-3-5-sonnet-latest"),
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
                    role: MessageRole::System,
                    content: Some("You are concise".to_owned()),
                    tool_calls: vec![],
                    tool_call_id: None,
                },
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
                Message {
                    role: MessageRole::Tool,
                    content: Some("{\"ok\":true}".to_owned()),
                    tool_calls: vec![],
                    tool_call_id: Some("call_1".to_owned()),
                },
            ],
        };
        let request =
            AnthropicMessagesRequest::from_context(&context, DEFAULT_ANTHROPIC_MAX_TOKENS)
                .expect("request should normalize");
        let request_json = serde_json::to_value(request).expect("request should serialize");
        assert_json_snapshot!(
            request_json,
            @r###"
        {
          "max_tokens": 1024,
          "messages": [
            {
              "content": [
                {
                  "text": "List project files",
                  "type": "text"
                }
              ],
              "role": "user"
            },
            {
              "content": [
                {
                  "id": "call_1",
                  "input": {
                    "path": "Cargo.toml"
                  },
                  "name": "read_file",
                  "type": "tool_use"
                }
              ],
              "role": "assistant"
            },
            {
              "content": [
                {
                  "content": "{\"ok\":true}",
                  "tool_use_id": "call_1",
                  "type": "tool_result"
                }
              ],
              "role": "user"
            }
          ],
          "model": "claude-3-5-sonnet-latest",
          "system": "You are concise",
          "tool_choice": {
            "type": "auto"
          },
          "tools": [
            {
              "description": "Read UTF-8 text from a file",
              "input_schema": {
                "additionalProperties": false,
                "properties": {
                  "path": {
                    "type": "string"
                  }
                },
                "required": [
                  "path"
                ],
                "type": "object"
              },
              "name": "read_file"
            }
          ]
        }
        "###
        );
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
    fn anthropic_response_normalization_maps_text_and_tool_use() {
        let response = AnthropicMessagesResponse {
            role: "assistant".to_owned(),
            content: vec![
                AnthropicResponseContentBlock {
                    kind: "text".to_owned(),
                    text: Some("Done".to_owned()),
                    id: None,
                    name: None,
                    input: None,
                },
                AnthropicResponseContentBlock {
                    kind: "tool_use".to_owned(),
                    text: None,
                    id: Some("call_1".to_owned()),
                    name: Some("read_file".to_owned()),
                    input: Some(json!({"path":"Cargo.toml"})),
                },
            ],
            stop_reason: Some("tool_use".to_owned()),
            usage: Some(AnthropicUsage {
                input_tokens: Some(5),
                output_tokens: Some(2),
            }),
        };

        let normalized = normalize_anthropic_response(response, &ProviderId::from("anthropic"))
            .expect("should parse");
        assert_eq!(normalized.message.role, MessageRole::Assistant);
        assert_eq!(normalized.message.content.as_deref(), Some("Done"));
        assert_eq!(normalized.finish_reason.as_deref(), Some("tool_use"));
        assert_eq!(
            normalized.usage,
            Some(UsageUpdate {
                prompt_tokens: Some(5),
                completion_tokens: Some(2),
                total_tokens: Some(7),
            })
        );
        assert_eq!(normalized.tool_calls.len(), 1);
        assert_eq!(normalized.tool_calls[0].id, "call_1");
        assert_eq!(normalized.tool_calls[0].name, "read_file");
        assert_eq!(
            normalized.tool_calls[0].arguments,
            json!({"path": "Cargo.toml"})
        );
    }

    #[test]
    fn anthropic_unknown_models_are_rejected_before_network_request() {
        let provider = AnthropicProvider::with_catalog(
            AnthropicConfig {
                api_key: Some("test-key".to_owned()),
                base_url: "http://127.0.0.1:9".to_owned(),
                max_tokens: 64,
            },
            test_model_catalog_for("anthropic", "claude-3-5-sonnet-latest", "Claude 3.5 Sonnet"),
        )
        .expect("provider should initialize");

        let context = test_context_for("anthropic", "unknown-model", "Ping");
        let validation = run_complete_anthropic(&provider, &context);
        assert!(matches!(
            validation,
            Err(ProviderError::UnknownModel { .. })
        ));
    }

    #[test]
    fn anthropic_transport_errors_are_mapped() {
        let provider = AnthropicProvider::with_catalog(
            AnthropicConfig {
                api_key: Some("test-key".to_owned()),
                base_url: "http://127.0.0.1:9".to_owned(),
                max_tokens: 64,
            },
            test_model_catalog_for("anthropic", "claude-3-5-sonnet-latest", "Claude 3.5 Sonnet"),
        )
        .expect("provider should initialize");
        let completion = run_complete_anthropic(
            &provider,
            &test_context_for("anthropic", "claude-3-5-sonnet-latest", "Ping"),
        );
        assert!(matches!(completion, Err(ProviderError::Transport { .. })));
    }

    #[test]
    fn anthropic_http_status_errors_are_mapped() {
        let base_url = spawn_one_shot_server(
            "429 Too Many Requests",
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"rate limited"}}"#,
            "application/json",
        );
        let provider = AnthropicProvider::with_catalog(
            AnthropicConfig {
                api_key: Some("test-key".to_owned()),
                base_url,
                max_tokens: 64,
            },
            test_model_catalog_for("anthropic", "claude-3-5-sonnet-latest", "Claude 3.5 Sonnet"),
        )
        .expect("provider should initialize");
        let completion = run_complete_anthropic(
            &provider,
            &test_context_for("anthropic", "claude-3-5-sonnet-latest", "Ping"),
        );
        assert!(matches!(
            completion,
            Err(ProviderError::HttpStatus {
                status: 429,
                message,
                ..
            }) if message == "rate limited"
        ));
    }

    #[test]
    fn anthropic_response_parse_errors_are_mapped() {
        let base_url = spawn_one_shot_server("200 OK", "not-json", "text/plain");
        let provider = AnthropicProvider::with_catalog(
            AnthropicConfig {
                api_key: Some("test-key".to_owned()),
                base_url,
                max_tokens: 64,
            },
            test_model_catalog_for("anthropic", "claude-3-5-sonnet-latest", "Claude 3.5 Sonnet"),
        )
        .expect("provider should initialize");
        let completion = run_complete_anthropic(
            &provider,
            &test_context_for("anthropic", "claude-3-5-sonnet-latest", "Ping"),
        );
        assert!(matches!(
            completion,
            Err(ProviderError::ResponseParse { .. })
        ));
    }

    #[test]
    fn reliable_provider_retries_transport_failures_then_succeeds() {
        let provider = Arc::new(SequencedProvider::new(
            "openai",
            "gpt-4o-mini",
            vec![
                Err(ProviderError::Transport {
                    provider: ProviderId::from("openai"),
                    message: "timeout".to_owned(),
                }),
                Ok(sample_response("Recovered")),
            ],
        ));
        let reliable = ReliableProvider::from_arc(
            provider.clone(),
            RetryPolicy {
                max_attempts: 3,
                backoff_base: Duration::from_millis(1),
                backoff_max: Duration::from_millis(1),
            },
        );

        let response = run_complete_with(&reliable, &test_context("gpt-4o-mini"))
            .expect("retry should recover");
        assert_eq!(response.message.content.as_deref(), Some("Recovered"));
        assert_eq!(provider.complete_call_count(), 2);
    }

    #[test]
    fn reliable_provider_does_not_retry_non_retriable_errors() {
        let provider = Arc::new(SequencedProvider::new(
            "openai",
            "gpt-4o-mini",
            vec![
                Err(ProviderError::ResponseParse {
                    provider: ProviderId::from("openai"),
                    message: "schema mismatch".to_owned(),
                }),
                Ok(sample_response("ignored")),
            ],
        ));
        let reliable = ReliableProvider::from_arc(
            provider.clone(),
            RetryPolicy {
                max_attempts: 3,
                backoff_base: Duration::from_millis(1),
                backoff_max: Duration::from_millis(1),
            },
        );

        let error = run_complete_with(&reliable, &test_context("gpt-4o-mini"))
            .expect_err("non-retriable error should be surfaced");
        assert!(matches!(error, ProviderError::ResponseParse { .. }));
        assert_eq!(provider.complete_call_count(), 1);
    }

    #[test]
    fn reliable_provider_enforces_max_attempts() {
        let provider = Arc::new(SequencedProvider::new(
            "openai",
            "gpt-4o-mini",
            vec![
                Err(ProviderError::Transport {
                    provider: ProviderId::from("openai"),
                    message: "timeout-1".to_owned(),
                }),
                Err(ProviderError::Transport {
                    provider: ProviderId::from("openai"),
                    message: "timeout-2".to_owned(),
                }),
                Ok(sample_response("should not be reached")),
            ],
        ));
        let reliable = ReliableProvider::from_arc(
            provider.clone(),
            RetryPolicy {
                max_attempts: 2,
                backoff_base: Duration::from_millis(1),
                backoff_max: Duration::from_millis(1),
            },
        );

        let error = run_complete_with(&reliable, &test_context("gpt-4o-mini"))
            .expect_err("max attempts should stop retries");
        assert!(matches!(error, ProviderError::Transport { .. }));
        assert_eq!(provider.complete_call_count(), 2);
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

    struct SequencedProvider {
        provider_id: ProviderId,
        model_catalog: ModelCatalog,
        complete_steps: Mutex<VecDeque<Result<Response, ProviderError>>>,
        complete_calls: AtomicUsize,
    }

    impl SequencedProvider {
        fn new(
            provider_id: &str,
            model_id: &str,
            complete_steps: Vec<Result<Response, ProviderError>>,
        ) -> Self {
            Self {
                provider_id: ProviderId::from(provider_id),
                model_catalog: test_model_catalog_for(provider_id, model_id, model_id),
                complete_steps: Mutex::new(VecDeque::from(complete_steps)),
                complete_calls: AtomicUsize::new(0),
            }
        }

        fn complete_call_count(&self) -> usize {
            self.complete_calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl Provider for SequencedProvider {
        fn provider_id(&self) -> &ProviderId {
            &self.provider_id
        }

        fn model_catalog(&self) -> &ModelCatalog {
            &self.model_catalog
        }

        async fn complete(&self, _context: &Context) -> Result<Response, ProviderError> {
            self.complete_calls.fetch_add(1, Ordering::SeqCst);
            let mut steps = self
                .complete_steps
                .lock()
                .expect("sequenced provider complete steps mutex should not be poisoned");
            steps.pop_front().unwrap_or_else(|| {
                Err(ProviderError::RequestFailed {
                    provider: self.provider_id.clone(),
                    message: "sequenced provider had no remaining completion steps".to_owned(),
                })
            })
        }

        async fn stream(
            &self,
            _context: &Context,
            _buffer_size: usize,
        ) -> Result<types::ProviderStream, ProviderError> {
            Err(ProviderError::RequestFailed {
                provider: self.provider_id.clone(),
                message: "sequenced provider stream is not configured".to_owned(),
            })
        }
    }

    fn sample_response(content: &str) -> Response {
        Response {
            message: Message {
                role: MessageRole::Assistant,
                content: Some(content.to_owned()),
                tool_calls: vec![],
                tool_call_id: None,
            },
            tool_calls: vec![],
            finish_reason: Some("stop".to_owned()),
            usage: None,
        }
    }

    fn test_model_catalog() -> ModelCatalog {
        test_model_catalog_for("openai", "gpt-4o-mini", "GPT-4o mini")
    }

    fn test_model_catalog_for(provider: &str, model: &str, display_name: &str) -> ModelCatalog {
        ModelCatalog::new(vec![ModelDescriptor {
            provider: ProviderId::from(provider),
            model: ModelId::from(model),
            display_name: Some(display_name.to_owned()),
            caps: ProviderCaps::default(),
            deprecated: false,
        }])
    }

    fn test_context(model: &str) -> Context {
        test_context_with_prompt(model, "Ping")
    }

    fn test_context_with_prompt(model: &str, prompt: &str) -> Context {
        test_context_for("openai", model, prompt)
    }

    fn test_context_for(provider: &str, model: &str, prompt: &str) -> Context {
        Context {
            provider: ProviderId::from(provider),
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
        run_complete_with(provider, context)
    }

    fn run_complete_anthropic(
        provider: &AnthropicProvider,
        context: &Context,
    ) -> Result<Response, ProviderError> {
        run_complete_with(provider, context)
    }

    fn run_complete_with<P: Provider>(
        provider: &P,
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
