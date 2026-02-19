use std::{collections::BTreeMap, time::Duration};

use tokio_util::sync::CancellationToken;
use tools::ToolRegistry;
use types::{
    Context, Message, MessageRole, Provider, ProviderError, ProviderId, Response, RuntimeError,
    StreamItem, ToolCall, ToolCallDelta, ToolError,
};

const DEFAULT_STREAM_BUFFER_SIZE: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnState {
    Streaming,
    ToolExecution,
    Yielding,
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct RuntimeLimits {
    pub turn_timeout: Duration,
    pub max_turns: usize,
    pub max_cost: Option<f64>,
}

impl Default for RuntimeLimits {
    fn default() -> Self {
        Self {
            turn_timeout: Duration::from_secs(60),
            max_turns: 8,
            max_cost: None,
        }
    }
}

pub struct AgentRuntime {
    provider: Box<dyn Provider>,
    tool_registry: ToolRegistry,
    limits: RuntimeLimits,
    stream_buffer_size: usize,
}

impl AgentRuntime {
    pub fn new(
        provider: Box<dyn Provider>,
        tool_registry: ToolRegistry,
        limits: RuntimeLimits,
    ) -> Self {
        Self {
            provider,
            tool_registry,
            limits,
            stream_buffer_size: DEFAULT_STREAM_BUFFER_SIZE,
        }
    }

    pub fn with_stream_buffer_size(mut self, stream_buffer_size: usize) -> Self {
        self.stream_buffer_size = stream_buffer_size.max(1);
        self
    }

    pub fn limits(&self) -> &RuntimeLimits {
        &self.limits
    }

    pub async fn run_session(
        &self,
        context: &mut Context,
        cancellation: &CancellationToken,
    ) -> Result<Response, RuntimeError> {
        self.validate_guard_preconditions()?;
        let mut turn = 0usize;

        loop {
            if cancellation.is_cancelled() {
                let state = TurnState::Cancelled;
                tracing::debug!(?state, "run_session cancelled before provider call");
                return Err(RuntimeError::Cancelled);
            }
            if turn >= self.limits.max_turns {
                return Err(RuntimeError::BudgetExceeded);
            }
            turn += 1;

            let state = TurnState::Streaming;
            tracing::debug!(turn, ?state, "running provider turn");

            let provider_response = self
                .request_provider_response(context, cancellation)
                .await?;
            let Response {
                message,
                tool_calls: response_tool_calls,
                finish_reason,
            } = provider_response;
            let mut assistant_message = message;
            let tool_calls = if assistant_message.tool_calls.is_empty() {
                response_tool_calls
            } else {
                assistant_message.tool_calls.clone()
            };
            assistant_message.tool_calls = tool_calls.clone();
            context.messages.push(assistant_message.clone());

            if tool_calls.is_empty() {
                let state = TurnState::Yielding;
                tracing::debug!(turn, ?state, "session yielded without tool calls");
                return Ok(Response {
                    message: assistant_message,
                    tool_calls,
                    finish_reason,
                });
            }

            let state = TurnState::ToolExecution;
            tracing::debug!(
                turn,
                ?state,
                tool_calls = tool_calls.len(),
                "executing tool calls"
            );
            for tool_call in tool_calls {
                if cancellation.is_cancelled() {
                    let state = TurnState::Cancelled;
                    tracing::debug!(turn, ?state, "run_session cancelled during tool execution");
                    return Err(RuntimeError::Cancelled);
                }
                let arguments =
                    serde_json::to_string(&tool_call.arguments).map_err(ToolError::from)?;
                let output = self.execute_tool_call(&tool_call.name, &arguments).await?;
                context.messages.push(Message {
                    role: MessageRole::Tool,
                    content: Some(output),
                    tool_calls: Vec::new(),
                    tool_call_id: Some(tool_call.id),
                });
            }
        }
    }

    fn validate_guard_preconditions(&self) -> Result<(), RuntimeError> {
        if self.limits.turn_timeout.is_zero() || self.limits.max_turns == 0 {
            return Err(RuntimeError::BudgetExceeded);
        }
        if self
            .limits
            .max_cost
            .is_some_and(|max_cost| !max_cost.is_finite() || max_cost <= 0.0)
        {
            return Err(RuntimeError::BudgetExceeded);
        }
        Ok(())
    }

    async fn request_provider_response(
        &self,
        context: &Context,
        cancellation: &CancellationToken,
    ) -> Result<Response, RuntimeError> {
        let caps = self.provider.capabilities(&context.model)?;
        if caps.supports_streaming {
            match self.stream_response(context, cancellation).await {
                Ok(response) => return Ok(response),
                Err(StreamCollectError::Cancelled) => return Err(RuntimeError::Cancelled),
                Err(error) => {
                    tracing::debug!(?error, "streaming path failed; falling back to complete");
                }
            }
        }
        self.complete_response(context, cancellation).await
    }

    async fn stream_response(
        &self,
        context: &Context,
        cancellation: &CancellationToken,
    ) -> Result<Response, StreamCollectError> {
        if cancellation.is_cancelled() {
            return Err(StreamCollectError::Cancelled);
        }

        let mut stream = self
            .provider
            .stream(context, self.stream_buffer_size)
            .await
            .map_err(StreamCollectError::Provider)?;
        let mut text_buffer = String::new();
        let mut tool_calls = ToolCallAccumulator::default();
        let mut finish_reason = None;

        while let Some(item) = stream.recv().await {
            if cancellation.is_cancelled() {
                return Err(StreamCollectError::Cancelled);
            }

            match item {
                Ok(StreamItem::Text(text)) => text_buffer.push_str(&text),
                Ok(StreamItem::ToolCallDelta(delta)) => tool_calls.merge(delta),
                Ok(StreamItem::FinishReason(reason)) => finish_reason = Some(reason),
                Ok(StreamItem::ConnectionLost(message)) => {
                    return Err(StreamCollectError::ConnectionLost(message));
                }
                Ok(StreamItem::ReasoningDelta(_) | StreamItem::UsageUpdate(_)) => {}
                Err(error) => return Err(StreamCollectError::Provider(error)),
            }
        }

        let tool_calls = tool_calls
            .build(&context.provider)
            .map_err(StreamCollectError::Provider)?;
        let content = (!text_buffer.is_empty()).then_some(text_buffer);
        let message = Message {
            role: MessageRole::Assistant,
            content,
            tool_calls: tool_calls.clone(),
            tool_call_id: None,
        };
        Ok(Response {
            message,
            tool_calls,
            finish_reason,
        })
    }

    async fn complete_response(
        &self,
        context: &Context,
        cancellation: &CancellationToken,
    ) -> Result<Response, RuntimeError> {
        if cancellation.is_cancelled() {
            return Err(RuntimeError::Cancelled);
        }
        self.provider
            .complete(context)
            .await
            .map_err(RuntimeError::from)
    }

    async fn execute_tool_call(&self, name: &str, arguments: &str) -> Result<String, RuntimeError> {
        self.tool_registry
            .execute(name, arguments)
            .await
            .map_err(RuntimeError::from)
    }
}

#[derive(Debug)]
enum StreamCollectError {
    Provider(ProviderError),
    ConnectionLost(String),
    Cancelled,
}

#[derive(Debug, Default)]
struct ToolCallAccumulator {
    by_index: BTreeMap<usize, ToolCallAccumulatorEntry>,
}

#[derive(Debug, Default)]
struct ToolCallAccumulatorEntry {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

impl ToolCallAccumulator {
    fn merge(&mut self, delta: ToolCallDelta) {
        let entry = self.by_index.entry(delta.index).or_default();
        if let Some(id) = delta.id {
            entry.id = Some(id);
        }
        if let Some(name) = delta.name {
            entry.name = Some(name);
        }
        if let Some(arguments) = delta.arguments {
            entry.arguments.push_str(&arguments);
        }
    }

    fn build(self, provider: &ProviderId) -> Result<Vec<ToolCall>, ProviderError> {
        let mut tool_calls = Vec::with_capacity(self.by_index.len());
        for (index, entry) in self.by_index {
            let id = entry.id.ok_or_else(|| ProviderError::ResponseParse {
                provider: provider.clone(),
                message: format!("streamed tool-call at index {index} is missing id"),
            })?;
            let name = entry.name.ok_or_else(|| ProviderError::ResponseParse {
                provider: provider.clone(),
                message: format!("streamed tool-call at index {index} is missing function name"),
            })?;
            let arguments = if entry.arguments.trim().is_empty() {
                "{}"
            } else {
                entry.arguments.as_str()
            };
            let parsed_arguments =
                serde_json::from_str(arguments).map_err(|error| ProviderError::ResponseParse {
                    provider: provider.clone(),
                    message: format!(
                        "streamed tool-call `{id}` has invalid arguments payload: {error}"
                    ),
                })?;
            tool_calls.push(ToolCall {
                id,
                name,
                arguments: parsed_arguments,
            });
        }
        Ok(tool_calls)
    }
}

impl std::fmt::Display for StreamCollectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Provider(error) => write!(f, "{error}"),
            Self::ConnectionLost(message) => write!(f, "{message}"),
            Self::Cancelled => f.write_str("cancelled"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Mutex, time::Duration};

    use async_trait::async_trait;
    use serde_json::{Value, json};
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;
    use types::{
        Context, FunctionDecl, JsonSchema, JsonSchemaType, Message, MessageRole, ModelCatalog,
        ModelDescriptor, ModelId, Provider, ProviderCaps, ProviderError, ProviderId, Response,
        SafetyTier, StreamItem, Tool, ToolCall, ToolCallDelta, ToolError,
    };

    use super::{AgentRuntime, RuntimeLimits};
    use tools::ToolRegistry;

    #[derive(Debug)]
    enum ProviderStep {
        Stream(Vec<Result<StreamItem, ProviderError>>),
        StreamFailure(ProviderError),
        Complete(Response),
    }

    struct FakeProvider {
        provider_id: ProviderId,
        model_catalog: ModelCatalog,
        steps: Mutex<VecDeque<ProviderStep>>,
    }

    impl FakeProvider {
        fn new(
            provider_id: ProviderId,
            model_catalog: ModelCatalog,
            steps: Vec<ProviderStep>,
        ) -> Self {
            Self {
                provider_id,
                model_catalog,
                steps: Mutex::new(steps.into()),
            }
        }

        fn next_step(&self) -> ProviderStep {
            self.steps
                .lock()
                .expect("test provider mutex should not be poisoned")
                .pop_front()
                .expect("test provider expected another scripted step")
        }
    }

    #[async_trait]
    impl Provider for FakeProvider {
        fn provider_id(&self) -> &ProviderId {
            &self.provider_id
        }

        fn model_catalog(&self) -> &ModelCatalog {
            &self.model_catalog
        }

        async fn complete(&self, _context: &Context) -> Result<Response, ProviderError> {
            match self.next_step() {
                ProviderStep::Complete(response) => Ok(response),
                other => Err(ProviderError::RequestFailed {
                    provider: self.provider_id.clone(),
                    message: format!("unexpected provider step for complete: {other:?}"),
                }),
            }
        }

        async fn stream(
            &self,
            _context: &Context,
            _buffer_size: usize,
        ) -> Result<types::ProviderStream, ProviderError> {
            match self.next_step() {
                ProviderStep::Stream(items) => {
                    let (sender, receiver) = mpsc::channel(items.len().max(1));
                    for item in items {
                        sender
                            .try_send(item)
                            .expect("test stream channel should accept scripted item");
                    }
                    drop(sender);
                    Ok(receiver)
                }
                ProviderStep::StreamFailure(error) => Err(error),
                other => Err(ProviderError::RequestFailed {
                    provider: self.provider_id.clone(),
                    message: format!("unexpected provider step for stream: {other:?}"),
                }),
            }
        }
    }

    #[derive(Debug, Clone, Copy)]
    struct MockReadTool;

    #[async_trait]
    impl Tool for MockReadTool {
        fn schema(&self) -> FunctionDecl {
            let mut properties = std::collections::BTreeMap::new();
            properties.insert("path".to_owned(), JsonSchema::new(JsonSchemaType::String));
            FunctionDecl::new(
                "read_file",
                None,
                JsonSchema::object(properties, vec!["path".to_owned()]),
            )
        }

        async fn execute(&self, args: &str) -> Result<String, ToolError> {
            let parsed: Value = serde_json::from_str(args)?;
            let path = parsed.get("path").and_then(Value::as_str).ok_or_else(|| {
                ToolError::InvalidArguments {
                    tool: "read_file".to_owned(),
                    message: "missing `path`".to_owned(),
                }
            })?;
            Ok(format!("mock read: {path}"))
        }

        fn timeout(&self) -> Duration {
            Duration::from_secs(1)
        }

        fn safety_tier(&self) -> SafetyTier {
            SafetyTier::ReadOnly
        }
    }

    #[tokio::test]
    async fn run_session_uses_complete_path_when_streaming_is_disabled() {
        let provider_id = ProviderId::from("openai");
        let model_id = ModelId::from("gpt-4o-mini");
        let provider = FakeProvider::new(
            provider_id.clone(),
            test_catalog(provider_id.clone(), model_id.clone(), false),
            vec![ProviderStep::Complete(assistant_response(
                "final answer",
                vec![],
            ))],
        );
        let runtime = AgentRuntime::new(
            Box::new(provider),
            ToolRegistry::default(),
            RuntimeLimits::default(),
        );
        let mut context = test_context(provider_id, model_id);

        let response = runtime
            .run_session(&mut context, &CancellationToken::new())
            .await
            .expect("runtime turn should complete");

        assert_eq!(response.message.content.as_deref(), Some("final answer"));
        assert_eq!(context.messages.len(), 2);
        assert!(matches!(context.messages[1].role, MessageRole::Assistant));
    }

    #[tokio::test]
    async fn run_session_reconstructs_streamed_tool_calls_and_loops_until_done() {
        let provider_id = ProviderId::from("openai");
        let model_id = ModelId::from("gpt-4o-mini");
        let provider = FakeProvider::new(
            provider_id.clone(),
            test_catalog(provider_id.clone(), model_id.clone(), true),
            vec![
                ProviderStep::Stream(vec![
                    Ok(StreamItem::Text("checking file".to_owned())),
                    Ok(StreamItem::ToolCallDelta(ToolCallDelta {
                        index: 0,
                        id: Some("call_1".to_owned()),
                        name: Some("read_file".to_owned()),
                        arguments: Some("{\"path\":\"Car".to_owned()),
                    })),
                    Ok(StreamItem::ToolCallDelta(ToolCallDelta {
                        index: 0,
                        id: None,
                        name: None,
                        arguments: Some("go.toml\"}".to_owned()),
                    })),
                    Ok(StreamItem::FinishReason("tool_calls".to_owned())),
                ]),
                ProviderStep::Stream(vec![
                    Ok(StreamItem::Text("done".to_owned())),
                    Ok(StreamItem::FinishReason("stop".to_owned())),
                ]),
            ],
        );
        let mut tools = ToolRegistry::default();
        tools.register("read_file", MockReadTool);
        let runtime = AgentRuntime::new(Box::new(provider), tools, RuntimeLimits::default());
        let mut context = test_context(provider_id, model_id);

        let response = runtime
            .run_session(&mut context, &CancellationToken::new())
            .await
            .expect("runtime turn should complete");

        assert_eq!(response.message.content.as_deref(), Some("done"));
        assert_eq!(context.messages.len(), 4);
        assert!(matches!(context.messages[1].role, MessageRole::Assistant));
        assert_eq!(context.messages[1].tool_calls.len(), 1);
        assert!(matches!(context.messages[2].role, MessageRole::Tool));
        assert_eq!(context.messages[2].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(
            context.messages[2].content.as_deref(),
            Some("mock read: Cargo.toml")
        );
        assert!(matches!(context.messages[3].role, MessageRole::Assistant));
    }

    #[tokio::test]
    async fn run_session_falls_back_to_complete_after_stream_failure() {
        let provider_id = ProviderId::from("openai");
        let model_id = ModelId::from("gpt-4o-mini");
        let provider = FakeProvider::new(
            provider_id.clone(),
            test_catalog(provider_id.clone(), model_id.clone(), true),
            vec![
                ProviderStep::StreamFailure(ProviderError::Transport {
                    provider: provider_id.clone(),
                    message: "stream failed".to_owned(),
                }),
                ProviderStep::Complete(assistant_response("fallback response", vec![])),
            ],
        );
        let runtime = AgentRuntime::new(
            Box::new(provider),
            ToolRegistry::default(),
            RuntimeLimits::default(),
        );
        let mut context = test_context(provider_id, model_id);

        let response = runtime
            .run_session(&mut context, &CancellationToken::new())
            .await
            .expect("runtime should fall back to complete");

        assert_eq!(
            response.message.content.as_deref(),
            Some("fallback response")
        );
        assert_eq!(context.messages.len(), 2);
    }

    #[tokio::test]
    async fn run_session_rejects_invalid_guard_preconditions() {
        let provider_id = ProviderId::from("openai");
        let model_id = ModelId::from("gpt-4o-mini");
        let provider = FakeProvider::new(
            provider_id.clone(),
            test_catalog(provider_id.clone(), model_id.clone(), false),
            vec![ProviderStep::Complete(assistant_response("unused", vec![]))],
        );
        let runtime = AgentRuntime::new(
            Box::new(provider),
            ToolRegistry::default(),
            RuntimeLimits {
                turn_timeout: Duration::from_secs(1),
                max_turns: 0,
                max_cost: None,
            },
        );
        let mut context = test_context(provider_id, model_id);

        let error = runtime
            .run_session(&mut context, &CancellationToken::new())
            .await
            .expect_err("invalid guard preconditions should fail");
        assert!(matches!(error, types::RuntimeError::BudgetExceeded));
    }

    fn test_catalog(
        provider_id: ProviderId,
        model_id: ModelId,
        supports_streaming: bool,
    ) -> ModelCatalog {
        ModelCatalog::new(vec![ModelDescriptor {
            provider: provider_id,
            model: model_id,
            display_name: None,
            caps: ProviderCaps {
                supports_streaming,
                supports_tools: true,
                supports_json_mode: false,
                supports_reasoning_traces: false,
                max_input_tokens: None,
                max_output_tokens: None,
                max_context_tokens: None,
            },
            deprecated: false,
        }])
    }

    fn test_context(provider_id: ProviderId, model_id: ModelId) -> Context {
        Context {
            provider: provider_id,
            model: model_id,
            messages: vec![Message {
                role: MessageRole::User,
                content: Some("Read Cargo.toml".to_owned()),
                tool_calls: Vec::new(),
                tool_call_id: None,
            }],
        }
    }

    fn assistant_response(content: &str, tool_calls: Vec<ToolCall>) -> Response {
        Response {
            message: Message {
                role: MessageRole::Assistant,
                content: Some(content.to_owned()),
                tool_calls: tool_calls.clone(),
                tool_call_id: None,
            },
            tool_calls,
            finish_reason: Some("stop".to_owned()),
        }
    }

    #[test]
    fn runtime_limits_default_matches_phase5_baseline() {
        let limits = RuntimeLimits::default();
        assert_eq!(limits.turn_timeout, Duration::from_secs(60));
        assert_eq!(limits.max_turns, 8);
        assert_eq!(limits.max_cost, None);
    }

    #[test]
    fn runtime_limits_can_store_optional_max_cost_guard() {
        let limits = RuntimeLimits {
            turn_timeout: Duration::from_secs(15),
            max_turns: 3,
            max_cost: Some(1.25),
        };
        assert_eq!(limits.max_cost, Some(1.25));
    }

    #[test]
    fn streamed_tool_calls_accept_empty_argument_payload_as_object() {
        let provider = ProviderId::from("openai");
        let mut accumulator = super::ToolCallAccumulator::default();
        accumulator.merge(ToolCallDelta {
            index: 0,
            id: Some("call_1".to_owned()),
            name: Some("noop".to_owned()),
            arguments: None,
        });
        let tool_calls = accumulator
            .build(&provider)
            .expect("empty streamed arguments should normalize to object");
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].arguments, json!({}));
    }
}
