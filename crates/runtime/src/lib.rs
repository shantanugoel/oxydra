use std::{collections::BTreeMap, sync::Arc, time::Duration};

use tokio_util::sync::CancellationToken;
use tools::ToolRegistry;
use types::{
    Context, Memory, MemoryError, MemoryRecallRequest, MemoryStoreRequest, Message, MessageRole,
    Provider, ProviderError, ProviderId, Response, RuntimeError, SafetyTier, StreamItem, ToolCall,
    ToolCallDelta, ToolError, UsageUpdate,
};

const DEFAULT_STREAM_BUFFER_SIZE: usize = 64;
const INVALID_TOOL_ARGS_RAW_KEY: &str = "__oxydra_invalid_tool_args_raw";
const INVALID_TOOL_ARGS_ERROR_KEY: &str = "__oxydra_invalid_tool_args_error";

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
    memory: Option<Arc<dyn Memory>>,
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
            memory: None,
        }
    }

    pub fn with_memory(mut self, memory: Arc<dyn Memory>) -> Self {
        self.memory = Some(memory);
        self
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
        self.run_session_internal(None, context, cancellation).await
    }

    pub async fn run_session_for_session(
        &self,
        session_id: &str,
        context: &mut Context,
        cancellation: &CancellationToken,
    ) -> Result<Response, RuntimeError> {
        self.run_session_internal(Some(session_id), context, cancellation)
            .await
    }

    pub async fn restore_session(
        &self,
        session_id: &str,
        context: &mut Context,
        limit: Option<u64>,
    ) -> Result<(), RuntimeError> {
        let Some(memory) = &self.memory else {
            return Ok(());
        };

        let restored = match memory
            .recall(MemoryRecallRequest {
                session_id: session_id.to_owned(),
                limit,
            })
            .await
        {
            Ok(records) => records,
            Err(MemoryError::NotFound { .. }) => return Ok(()),
            Err(error) => return Err(RuntimeError::from(error)),
        };

        let restored_messages = restored
            .into_iter()
            .map(|record| serde_json::from_value::<Message>(record.payload))
            .collect::<Result<Vec<_>, _>>()
            .map_err(MemoryError::from)
            .map_err(RuntimeError::from)?;
        context.messages = restored_messages;
        Ok(())
    }

    async fn run_session_internal(
        &self,
        session_id: Option<&str>,
        context: &mut Context,
        cancellation: &CancellationToken,
    ) -> Result<Response, RuntimeError> {
        if context.tools.is_empty() {
            context.tools = self.tool_registry.schemas();
        }
        let mut next_memory_sequence = self.next_memory_sequence(session_id).await?;
        self.persist_context_tail_if_needed(session_id, &mut next_memory_sequence, context)
            .await?;
        self.validate_guard_preconditions()?;
        let mut turn = 0usize;
        let mut accumulated_cost = 0.0;

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
                usage,
            } = provider_response;
            let mut assistant_message = message;
            let tool_calls = if assistant_message.tool_calls.is_empty() {
                response_tool_calls
            } else {
                assistant_message.tool_calls.clone()
            };
            assistant_message.tool_calls = tool_calls.clone();
            self.enforce_cost_budget(usage.as_ref(), &mut accumulated_cost)?;
            context.messages.push(assistant_message.clone());
            self.persist_message_if_needed(
                session_id,
                &mut next_memory_sequence,
                &assistant_message,
            )
            .await?;

            if tool_calls.is_empty() {
                let state = TurnState::Yielding;
                tracing::debug!(turn, ?state, "session yielded without tool calls");
                return Ok(Response {
                    message: assistant_message,
                    tool_calls,
                    finish_reason,
                    usage,
                });
            }

            let state = TurnState::ToolExecution;
            tracing::debug!(
                turn,
                ?state,
                tool_calls = tool_calls.len(),
                "executing tool calls"
            );

            let mut current_batch = Vec::new();
            for tool_call in &tool_calls {
                let tier = self
                    .tool_registry
                    .get(&tool_call.name)
                    .map(|t| t.safety_tier());

                if tier == Some(SafetyTier::ReadOnly) {
                    current_batch.push(tool_call);
                } else {
                    if !current_batch.is_empty() {
                        let futures = current_batch
                            .drain(..)
                            .map(|tc| self.execute_tool_and_format(tc, cancellation));
                        let batch_results = futures::future::join_all(futures).await;
                        for result in batch_results {
                            let message = result?;
                            context.messages.push(message.clone());
                            self.persist_message_if_needed(
                                session_id,
                                &mut next_memory_sequence,
                                &message,
                            )
                            .await?;
                        }
                    }

                    if cancellation.is_cancelled() {
                        let state = TurnState::Cancelled;
                        tracing::debug!(
                            turn,
                            ?state,
                            "run_session cancelled during tool execution"
                        );
                        return Err(RuntimeError::Cancelled);
                    }

                    let result = self
                        .execute_tool_and_format(tool_call, cancellation)
                        .await?;
                    context.messages.push(result.clone());
                    self.persist_message_if_needed(session_id, &mut next_memory_sequence, &result)
                        .await?;
                }
            }

            if !current_batch.is_empty() {
                let futures = current_batch
                    .drain(..)
                    .map(|tc| self.execute_tool_and_format(tc, cancellation));
                let batch_results = futures::future::join_all(futures).await;
                for result in batch_results {
                    let message = result?;
                    context.messages.push(message.clone());
                    self.persist_message_if_needed(session_id, &mut next_memory_sequence, &message)
                        .await?;
                }
            }
        }
    }

    async fn next_memory_sequence(&self, session_id: Option<&str>) -> Result<u64, RuntimeError> {
        let (Some(memory), Some(session_id)) = (&self.memory, session_id) else {
            return Ok(1);
        };

        match memory
            .recall(MemoryRecallRequest {
                session_id: session_id.to_owned(),
                limit: Some(1),
            })
            .await
        {
            Ok(records) => Ok(records
                .last()
                .map(|record| record.sequence.saturating_add(1))
                .unwrap_or(1)),
            Err(MemoryError::NotFound { .. }) => Ok(1),
            Err(error) => Err(RuntimeError::from(error)),
        }
    }

    async fn persist_context_tail_if_needed(
        &self,
        session_id: Option<&str>,
        next_sequence: &mut u64,
        context: &Context,
    ) -> Result<(), RuntimeError> {
        if session_id.is_none() || self.memory.is_none() {
            return Ok(());
        }

        let already_persisted = next_sequence.saturating_sub(1);
        let start_index = match usize::try_from(already_persisted) {
            Ok(index) => index,
            Err(_) => context.messages.len(),
        };
        for message in context.messages.iter().skip(start_index) {
            self.persist_message_if_needed(session_id, next_sequence, message)
                .await?;
        }
        Ok(())
    }

    async fn persist_message_if_needed(
        &self,
        session_id: Option<&str>,
        next_sequence: &mut u64,
        message: &Message,
    ) -> Result<(), RuntimeError> {
        let (Some(memory), Some(session_id)) = (&self.memory, session_id) else {
            return Ok(());
        };

        let payload = serde_json::to_value(message)
            .map_err(MemoryError::from)
            .map_err(RuntimeError::from)?;
        memory
            .store(MemoryStoreRequest {
                session_id: session_id.to_owned(),
                sequence: *next_sequence,
                payload,
            })
            .await
            .map_err(RuntimeError::from)?;
        *next_sequence = next_sequence.saturating_add(1);
        Ok(())
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

    fn enforce_cost_budget(
        &self,
        usage: Option<&UsageUpdate>,
        accumulated_cost: &mut f64,
    ) -> Result<(), RuntimeError> {
        let Some(max_cost) = self.limits.max_cost else {
            return Ok(());
        };
        // Phase 5 interim accounting: use provider-reported token usage as cost units.
        let turn_cost = usage
            .and_then(Self::usage_to_cost)
            .ok_or(RuntimeError::BudgetExceeded)?;
        *accumulated_cost += turn_cost;
        if *accumulated_cost > max_cost {
            return Err(RuntimeError::BudgetExceeded);
        }
        Ok(())
    }

    fn usage_to_cost(usage: &UsageUpdate) -> Option<f64> {
        let total_tokens = usage.total_tokens.or_else(|| {
            let prompt = usage.prompt_tokens.unwrap_or(0);
            let completion = usage.completion_tokens.unwrap_or(0);
            let aggregated = prompt.saturating_add(completion);
            (aggregated > 0).then_some(aggregated)
        })?;
        Some(total_tokens as f64)
    }

    fn merge_usage(existing: Option<UsageUpdate>, update: UsageUpdate) -> UsageUpdate {
        let mut merged = existing.unwrap_or_default();
        if update.prompt_tokens.is_some() {
            merged.prompt_tokens = update.prompt_tokens;
        }
        if update.completion_tokens.is_some() {
            merged.completion_tokens = update.completion_tokens;
        }
        if update.total_tokens.is_some() {
            merged.total_tokens = update.total_tokens;
        }
        merged
    }

    fn invalid_streamed_arguments(arguments: &serde_json::Value) -> Option<String> {
        let object = arguments.as_object()?;
        let raw_payload = object.get(INVALID_TOOL_ARGS_RAW_KEY)?.as_str()?;
        let parse_error = object.get(INVALID_TOOL_ARGS_ERROR_KEY)?.as_str()?;
        Some(format!(
            "invalid JSON arguments payload: {parse_error}; raw payload: {raw_payload}"
        ))
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
                Err(StreamCollectError::TurnTimedOut) => return Err(RuntimeError::BudgetExceeded),
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

        let mut stream = tokio::select! {
            _ = cancellation.cancelled() => return Err(StreamCollectError::Cancelled),
            timed = tokio::time::timeout(
                self.limits.turn_timeout,
                self.provider.stream(context, self.stream_buffer_size),
            ) => match timed {
                Ok(stream) => stream.map_err(StreamCollectError::Provider)?,
                Err(_) => return Err(StreamCollectError::TurnTimedOut),
            },
        };
        let mut text_buffer = String::new();
        let mut tool_calls = ToolCallAccumulator::default();
        let mut finish_reason = None;
        let mut usage = None;

        loop {
            let item = tokio::select! {
                _ = cancellation.cancelled() => return Err(StreamCollectError::Cancelled),
                timed = tokio::time::timeout(self.limits.turn_timeout, stream.recv()) => match timed {
                    Ok(item) => item,
                    Err(_) => return Err(StreamCollectError::TurnTimedOut),
                }
            };
            let Some(item) = item else {
                break;
            };

            match item {
                Ok(StreamItem::Text(text)) => text_buffer.push_str(&text),
                Ok(StreamItem::ToolCallDelta(delta)) => tool_calls.merge(delta),
                Ok(StreamItem::FinishReason(reason)) => finish_reason = Some(reason),
                Ok(StreamItem::UsageUpdate(update)) => {
                    usage = Some(Self::merge_usage(usage.take(), update));
                }
                Ok(StreamItem::ConnectionLost(message)) => {
                    return Err(StreamCollectError::ConnectionLost(message));
                }
                Ok(StreamItem::ReasoningDelta(_)) => {}
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
            usage,
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
        tokio::select! {
            _ = cancellation.cancelled() => Err(RuntimeError::Cancelled),
            timed = tokio::time::timeout(self.limits.turn_timeout, self.provider.complete(context)) => match timed {
                Ok(response) => response.map_err(RuntimeError::from),
                Err(_) => Err(RuntimeError::BudgetExceeded),
            }
        }
    }

    async fn execute_tool_call(
        &self,
        name: &str,
        arguments: &serde_json::Value,
        cancellation: &CancellationToken,
    ) -> Result<String, RuntimeError> {
        let tool = self.tool_registry.get(name).ok_or_else(|| {
            RuntimeError::Tool(ToolError::ExecutionFailed {
                tool: name.to_string(),
                message: format!("unknown tool `{name}`"),
            })
        })?;

        if let Some(message) = Self::invalid_streamed_arguments(arguments) {
            return Err(RuntimeError::Tool(ToolError::InvalidArguments {
                tool: name.to_string(),
                message,
            }));
        }

        let schema_decl = tool.schema();
        let schema_json =
            serde_json::to_value(&schema_decl.parameters).map_err(ToolError::Serialization)?;
        let validator = jsonschema::options().build(&schema_json).map_err(|error| {
            RuntimeError::Tool(ToolError::ExecutionFailed {
                tool: name.to_string(),
                message: format!("failed to compile validation schema: {error}"),
            })
        })?;

        if !validator.is_valid(arguments) {
            let error_msgs: Vec<String> = validator
                .iter_errors(arguments)
                .map(|e| format!("- {e}"))
                .collect();
            return Err(RuntimeError::Tool(ToolError::InvalidArguments {
                tool: name.to_string(),
                message: format!("schema validation failed:\n{}", error_msgs.join("\n")),
            }));
        }

        let arg_str = serde_json::to_string(arguments).map_err(ToolError::Serialization)?;

        tokio::select! {
            _ = cancellation.cancelled() => Err(RuntimeError::Cancelled),
            timed = tokio::time::timeout(self.limits.turn_timeout, self.tool_registry.execute(name, &arg_str)) => match timed {
                Ok(output) => output.map_err(RuntimeError::from),
                Err(_) => Err(RuntimeError::BudgetExceeded),
            }
        }
    }

    async fn execute_tool_and_format(
        &self,
        tool_call: &ToolCall,
        cancellation: &CancellationToken,
    ) -> Result<Message, RuntimeError> {
        let output = match self
            .execute_tool_call(&tool_call.name, &tool_call.arguments, cancellation)
            .await
        {
            Ok(out) => out,
            Err(RuntimeError::Tool(err)) => {
                tracing::warn!(
                    ?err,
                    tool = tool_call.name,
                    "tool execution failed, injecting error for self-correction"
                );
                format!("Tool execution failed: {}", err)
            }
            Err(e) => return Err(e),
        };

        Ok(Message {
            role: MessageRole::Tool,
            content: Some(output),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call.id.clone()),
        })
    }
}

#[derive(Debug)]
enum StreamCollectError {
    Provider(ProviderError),
    ConnectionLost(String),
    Cancelled,
    TurnTimedOut,
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
            let parsed_arguments = match serde_json::from_str(arguments) {
                Ok(parsed_arguments) => parsed_arguments,
                Err(error) => {
                    let mut payload = serde_json::Map::new();
                    payload.insert(
                        INVALID_TOOL_ARGS_RAW_KEY.to_owned(),
                        serde_json::Value::String(arguments.to_owned()),
                    );
                    payload.insert(
                        INVALID_TOOL_ARGS_ERROR_KEY.to_owned(),
                        serde_json::Value::String(error.to_string()),
                    );
                    serde_json::Value::Object(payload)
                }
            };
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
            Self::TurnTimedOut => f.write_str("turn timed out"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use async_trait::async_trait;
    use mockall::mock;
    use serde_json::{Value, json};
    use tokio::sync::mpsc;
    use tokio::time::sleep;
    use tokio_util::sync::CancellationToken;
    use types::{
        Context, FunctionDecl, JsonSchema, JsonSchemaType, Memory, MemoryError,
        MemoryForgetRequest, MemoryRecallRequest, MemoryRecord, MemoryStoreRequest, Message,
        MessageRole, ModelCatalog, ModelDescriptor, ModelId, Provider, ProviderCaps, ProviderError,
        ProviderId, Response, SafetyTier, StreamItem, Tool, ToolCall, ToolCallDelta, ToolError,
        UsageUpdate,
    };

    use super::{AgentRuntime, RuntimeLimits};
    use tools::ToolRegistry;

    mock! {
        ProviderContract {}
        #[async_trait]
        impl Provider for ProviderContract {
            fn provider_id(&self) -> &ProviderId;
            fn model_catalog(&self) -> &ModelCatalog;
            async fn complete(&self, context: &Context) -> Result<Response, ProviderError>;
            async fn stream(
                &self,
                context: &Context,
                buffer_size: usize,
            ) -> Result<types::ProviderStream, ProviderError>;
        }
    }

    mock! {
        ToolContract {}
        #[async_trait]
        impl Tool for ToolContract {
            fn schema(&self) -> FunctionDecl;
            async fn execute(&self, args: &str) -> Result<String, ToolError>;
            fn timeout(&self) -> Duration;
            fn safety_tier(&self) -> SafetyTier;
        }
    }

    #[derive(Debug)]
    enum ProviderStep {
        Stream(Vec<Result<StreamItem, ProviderError>>),
        StreamFailure(ProviderError),
        Complete(Response),
        CompleteDelayed { response: Response, delay: Duration },
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
                ProviderStep::CompleteDelayed { response, delay } => {
                    sleep(delay).await;
                    Ok(response)
                }
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

    #[derive(Debug, Clone, Copy)]
    struct SlowTool;

    #[async_trait]
    impl Tool for SlowTool {
        fn schema(&self) -> FunctionDecl {
            FunctionDecl::new(
                "slow_tool",
                None,
                JsonSchema::object(std::collections::BTreeMap::new(), vec![]),
            )
        }

        async fn execute(&self, _args: &str) -> Result<String, ToolError> {
            sleep(Duration::from_millis(50)).await;
            Ok("done".to_owned())
        }

        fn timeout(&self) -> Duration {
            Duration::from_secs(1)
        }

        fn safety_tier(&self) -> SafetyTier {
            SafetyTier::ReadOnly
        }
    }

    #[derive(Default)]
    struct RecordingMemory {
        records: Mutex<Vec<MemoryRecord>>,
    }

    impl RecordingMemory {
        fn with_records(records: Vec<MemoryRecord>) -> Self {
            Self {
                records: Mutex::new(records),
            }
        }
    }

    #[async_trait]
    impl Memory for RecordingMemory {
        async fn store(&self, request: MemoryStoreRequest) -> Result<MemoryRecord, MemoryError> {
            let record = MemoryRecord {
                session_id: request.session_id,
                sequence: request.sequence,
                payload: request.payload,
            };
            self.records
                .lock()
                .expect("memory test mutex should not be poisoned")
                .push(record.clone());
            Ok(record)
        }

        async fn recall(
            &self,
            request: MemoryRecallRequest,
        ) -> Result<Vec<MemoryRecord>, MemoryError> {
            let mut records: Vec<MemoryRecord> = self
                .records
                .lock()
                .expect("memory test mutex should not be poisoned")
                .iter()
                .filter(|record| record.session_id == request.session_id)
                .cloned()
                .collect();
            records.sort_by_key(|record| record.sequence);
            if let Some(limit) = request.limit {
                let keep = usize::try_from(limit).unwrap_or(usize::MAX);
                if records.len() > keep {
                    records = records[records.len().saturating_sub(keep)..].to_vec();
                }
            }
            if records.is_empty() {
                return Err(MemoryError::NotFound {
                    session_id: request.session_id,
                });
            }
            Ok(records)
        }

        async fn forget(&self, request: MemoryForgetRequest) -> Result<(), MemoryError> {
            let mut records = self
                .records
                .lock()
                .expect("memory test mutex should not be poisoned");
            let before = records.len();
            records.retain(|record| record.session_id != request.session_id);
            if records.len() == before {
                return Err(MemoryError::NotFound {
                    session_id: request.session_id,
                });
            }
            Ok(())
        }
    }

    fn mock_provider(
        provider_id: ProviderId,
        model_id: ModelId,
        supports_streaming: bool,
    ) -> MockProviderContract {
        let mut provider = MockProviderContract::new();
        provider
            .expect_provider_id()
            .return_const(provider_id.clone());
        provider.expect_model_catalog().return_const(test_catalog(
            provider_id,
            model_id,
            supports_streaming,
        ));
        provider
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

    #[tokio::test]
    async fn run_session_supports_mockall_provider_single_turn_without_tools() {
        let provider_id = ProviderId::from("openai");
        let model_id = ModelId::from("gpt-4o-mini");
        let mut provider = mock_provider(provider_id.clone(), model_id.clone(), false);
        provider.expect_stream().never();
        provider
            .expect_complete()
            .times(1)
            .returning(|_| Ok(assistant_response("mockall response", vec![])));

        let runtime = AgentRuntime::new(
            Box::new(provider),
            ToolRegistry::default(),
            RuntimeLimits::default(),
        );
        let mut context = test_context(provider_id, model_id);

        let response = runtime
            .run_session(&mut context, &CancellationToken::new())
            .await
            .expect("mockall provider should produce one final turn");

        assert_eq!(
            response.message.content.as_deref(),
            Some("mockall response")
        );
        assert!(response.tool_calls.is_empty());
    }

    #[tokio::test]
    async fn run_session_exposes_registered_tools_to_provider_context() {
        let provider_id = ProviderId::from("openai");
        let model_id = ModelId::from("gpt-4o-mini");
        let mut provider = mock_provider(provider_id.clone(), model_id.clone(), false);
        provider.expect_stream().never();
        provider
            .expect_complete()
            .times(1)
            .withf(|context| context.tools.iter().any(|tool| tool.name == "read_file"))
            .returning(|_| Ok(assistant_response("tool schema seen", vec![])));

        let mut tools = ToolRegistry::default();
        tools.register("read_file", MockReadTool);
        let runtime = AgentRuntime::new(Box::new(provider), tools, RuntimeLimits::default());
        let mut context = test_context(provider_id, model_id);

        let response = runtime
            .run_session(&mut context, &CancellationToken::new())
            .await
            .expect("runtime should expose tools before provider call");

        assert_eq!(
            response.message.content.as_deref(),
            Some("tool schema seen")
        );
        assert!(context.tools.iter().any(|tool| tool.name == "read_file"));
    }

    #[tokio::test]
    async fn run_session_for_session_persists_initial_context_and_new_turns() {
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
        let memory = Arc::new(RecordingMemory::default());
        let runtime = AgentRuntime::new(
            Box::new(provider),
            ToolRegistry::default(),
            RuntimeLimits::default(),
        )
        .with_memory(memory.clone());
        let mut context = test_context(provider_id, model_id);

        let response = runtime
            .run_session_for_session("session-persist", &mut context, &CancellationToken::new())
            .await
            .expect("runtime turn should complete");
        assert_eq!(response.message.content.as_deref(), Some("final answer"));

        let stored = memory
            .recall(MemoryRecallRequest {
                session_id: "session-persist".to_owned(),
                limit: None,
            })
            .await
            .expect("persisted records should be recallable");
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[0].sequence, 1);
        assert_eq!(stored[1].sequence, 2);
        let restored_first: Message =
            serde_json::from_value(stored[0].payload.clone()).expect("payload should deserialize");
        let restored_second: Message =
            serde_json::from_value(stored[1].payload.clone()).expect("payload should deserialize");
        assert_eq!(restored_first.role, MessageRole::User);
        assert_eq!(restored_second.role, MessageRole::Assistant);
    }

    #[tokio::test]
    async fn restore_session_hydrates_context_when_memory_is_configured() {
        let provider_id = ProviderId::from("openai");
        let model_id = ModelId::from("gpt-4o-mini");
        let memory = Arc::new(RecordingMemory::with_records(vec![
            MemoryRecord {
                session_id: "session-restore".to_owned(),
                sequence: 1,
                payload: serde_json::to_value(Message {
                    role: MessageRole::User,
                    content: Some("hello".to_owned()),
                    tool_calls: vec![],
                    tool_call_id: None,
                })
                .expect("message should serialize"),
            },
            MemoryRecord {
                session_id: "session-restore".to_owned(),
                sequence: 2,
                payload: serde_json::to_value(Message {
                    role: MessageRole::Assistant,
                    content: Some("world".to_owned()),
                    tool_calls: vec![],
                    tool_call_id: None,
                })
                .expect("message should serialize"),
            },
        ]));
        let runtime = AgentRuntime::new(
            Box::new(FakeProvider::new(
                provider_id.clone(),
                test_catalog(provider_id.clone(), model_id.clone(), false),
                vec![ProviderStep::Complete(assistant_response("unused", vec![]))],
            )),
            ToolRegistry::default(),
            RuntimeLimits::default(),
        )
        .with_memory(memory);
        let mut context = Context {
            provider: provider_id,
            model: model_id,
            tools: vec![],
            messages: vec![],
        };

        runtime
            .restore_session("session-restore", &mut context, None)
            .await
            .expect("restore should succeed");

        assert_eq!(context.messages.len(), 2);
        assert_eq!(context.messages[0].content.as_deref(), Some("hello"));
        assert_eq!(context.messages[1].content.as_deref(), Some("world"));
    }

    #[tokio::test]
    async fn run_session_for_session_keeps_existing_behavior_without_memory_backend() {
        let provider_id = ProviderId::from("openai");
        let model_id = ModelId::from("gpt-4o-mini");
        let provider = FakeProvider::new(
            provider_id.clone(),
            test_catalog(provider_id.clone(), model_id.clone(), false),
            vec![ProviderStep::Complete(assistant_response(
                "no memory configured",
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
            .run_session_for_session("session-disabled", &mut context, &CancellationToken::new())
            .await
            .expect("session run should still succeed without configured memory");
        assert_eq!(
            response.message.content.as_deref(),
            Some("no memory configured")
        );
        assert_eq!(context.messages.len(), 2);
    }

    #[tokio::test]
    async fn run_session_recovers_from_validation_error() {
        let provider_id = ProviderId::from("openai");
        let model_id = ModelId::from("gpt-4o-mini");
        let provider = FakeProvider::new(
            provider_id.clone(),
            test_catalog(provider_id.clone(), model_id.clone(), true),
            vec![
                ProviderStep::Stream(vec![
                    Ok(StreamItem::ToolCallDelta(ToolCallDelta {
                        index: 0,
                        id: Some("call_1".to_owned()),
                        name: Some("read_file".to_owned()),
                        // Missing "path" property, should trigger validation error
                        arguments: Some("{}".to_owned()),
                    })),
                    Ok(StreamItem::FinishReason("tool_calls".to_owned())),
                ]),
                ProviderStep::Stream(vec![
                    Ok(StreamItem::Text(
                        "Oh I missed the path argument!".to_owned(),
                    )),
                    Ok(StreamItem::FinishReason("stop".to_owned())),
                ]),
            ],
        );

        let mut tool = MockToolContract::new();
        let mut properties = std::collections::BTreeMap::new();
        properties.insert("path".to_owned(), JsonSchema::new(JsonSchemaType::String));
        tool.expect_schema().return_const(FunctionDecl::new(
            "read_file",
            None,
            JsonSchema::object(properties, vec!["path".to_owned()]),
        ));
        tool.expect_safety_tier().return_const(SafetyTier::ReadOnly);
        // Execute should not be called because validation intercepts it!
        tool.expect_execute().times(0);

        let mut registry = ToolRegistry::new(1024);
        registry.register("read_file", tool);

        let runtime = AgentRuntime::new(Box::new(provider), registry, RuntimeLimits::default());
        let mut context = Context {
            messages: vec![Message {
                role: MessageRole::User,
                content: Some("read".to_owned()),
                tool_calls: Vec::new(),
                tool_call_id: None,
            }],
            tools: Vec::new(),
            model: model_id,
            provider: provider_id,
        };

        let result = runtime
            .run_session(&mut context, &CancellationToken::new())
            .await
            .expect("should complete despite validation error");

        assert_eq!(
            result.message.content.as_deref(),
            Some("Oh I missed the path argument!")
        );
        // Context should contain the injected tool error
        let tool_result = context
            .messages
            .iter()
            .find(|m| m.role == MessageRole::Tool)
            .unwrap();
        let err_msg = tool_result.content.as_ref().unwrap();
        assert!(err_msg.contains("schema validation failed"));
        assert!(err_msg.contains("\"path\" is a required property"));
    }

    #[tokio::test]
    async fn run_session_recovers_from_malformed_streamed_json_arguments() {
        let provider_id = ProviderId::from("openai");
        let model_id = ModelId::from("gpt-4o-mini");
        let provider = FakeProvider::new(
            provider_id.clone(),
            test_catalog(provider_id.clone(), model_id.clone(), true),
            vec![
                ProviderStep::Stream(vec![
                    Ok(StreamItem::ToolCallDelta(ToolCallDelta {
                        index: 0,
                        id: Some("call_1".to_owned()),
                        name: Some("read_file".to_owned()),
                        arguments: Some("{\"path\":\"Cargo.toml\"".to_owned()),
                    })),
                    Ok(StreamItem::FinishReason("tool_calls".to_owned())),
                ]),
                ProviderStep::Stream(vec![
                    Ok(StreamItem::Text("retrying with corrected args".to_owned())),
                    Ok(StreamItem::FinishReason("stop".to_owned())),
                ]),
            ],
        );

        let mut tool = MockToolContract::new();
        let mut properties = std::collections::BTreeMap::new();
        properties.insert("path".to_owned(), JsonSchema::new(JsonSchemaType::String));
        tool.expect_schema().return_const(FunctionDecl::new(
            "read_file",
            None,
            JsonSchema::object(properties, vec!["path".to_owned()]),
        ));
        tool.expect_safety_tier().return_const(SafetyTier::ReadOnly);
        tool.expect_execute().times(0);

        let mut registry = ToolRegistry::new(1024);
        registry.register("read_file", tool);

        let runtime = AgentRuntime::new(Box::new(provider), registry, RuntimeLimits::default());
        let mut context = Context {
            messages: vec![Message {
                role: MessageRole::User,
                content: Some("read".to_owned()),
                tool_calls: Vec::new(),
                tool_call_id: None,
            }],
            tools: Vec::new(),
            model: model_id,
            provider: provider_id,
        };

        let result = runtime
            .run_session(&mut context, &CancellationToken::new())
            .await
            .expect("should complete despite malformed JSON arguments");

        assert_eq!(
            result.message.content.as_deref(),
            Some("retrying with corrected args")
        );
        let tool_result = context
            .messages
            .iter()
            .find(|m| m.role == MessageRole::Tool)
            .expect("tool result should be injected into context");
        let err_msg = tool_result
            .content
            .as_ref()
            .expect("tool result should contain error text");
        assert!(err_msg.contains("invalid JSON arguments payload"));
        assert!(err_msg.contains("EOF while parsing"));
    }

    #[tokio::test]
    async fn run_session_executes_readonly_tools_in_parallel() {
        let provider_id = ProviderId::from("openai");
        let model_id = ModelId::from("gpt-4o-mini");
        let provider = FakeProvider::new(
            provider_id.clone(),
            test_catalog(provider_id.clone(), model_id.clone(), true),
            vec![
                ProviderStep::Stream(vec![
                    Ok(StreamItem::ToolCallDelta(ToolCallDelta {
                        index: 0,
                        id: Some("call_1".to_owned()),
                        name: Some("slow_tool".to_owned()),
                        arguments: Some("{}".to_owned()),
                    })),
                    Ok(StreamItem::ToolCallDelta(ToolCallDelta {
                        index: 1,
                        id: Some("call_2".to_owned()),
                        name: Some("slow_tool".to_owned()),
                        arguments: Some("{}".to_owned()),
                    })),
                    Ok(StreamItem::FinishReason("tool_calls".to_owned())),
                ]),
                ProviderStep::Stream(vec![
                    Ok(StreamItem::Text("all done".to_owned())),
                    Ok(StreamItem::FinishReason("stop".to_owned())),
                ]),
            ],
        );

        let mut registry = ToolRegistry::new(1024);
        registry.register("slow_tool", SlowTool);

        let runtime = AgentRuntime::new(Box::new(provider), registry, RuntimeLimits::default());
        let mut context = Context {
            messages: vec![Message {
                role: MessageRole::User,
                content: Some("do slow things".to_owned()),
                tool_calls: Vec::new(),
                tool_call_id: None,
            }],
            tools: Vec::new(),
            model: model_id,
            provider: provider_id,
        };

        let start = std::time::Instant::now();
        runtime
            .run_session(&mut context, &CancellationToken::new())
            .await
            .unwrap();
        let elapsed = start.elapsed();

        // SlowTool sleeps for 50ms. Two sequential would take > 100ms.
        // Parallel should take ~50ms. We give it some buffer for setup/teardown.
        assert!(
            elapsed < Duration::from_millis(90),
            "Took {:?}, not parallel!",
            elapsed
        );

        let tool_results: Vec<_> = context
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::Tool)
            .collect();
        assert_eq!(tool_results.len(), 2);
    }

    #[tokio::test]
    async fn run_session_supports_mockall_tool_execution() {
        let provider_id = ProviderId::from("openai");
        let model_id = ModelId::from("gpt-4o-mini");
        let provider = FakeProvider::new(
            provider_id.clone(),
            test_catalog(provider_id.clone(), model_id.clone(), true),
            vec![
                ProviderStep::Stream(vec![
                    Ok(StreamItem::ToolCallDelta(ToolCallDelta {
                        index: 0,
                        id: Some("call_1".to_owned()),
                        name: Some("read_file".to_owned()),
                        arguments: Some("{\"path\":\"Cargo.toml\"}".to_owned()),
                    })),
                    Ok(StreamItem::FinishReason("tool_calls".to_owned())),
                ]),
                ProviderStep::Stream(vec![
                    Ok(StreamItem::Text("tool complete".to_owned())),
                    Ok(StreamItem::FinishReason("stop".to_owned())),
                ]),
            ],
        );

        let mut tool = MockToolContract::new();
        let mut properties = std::collections::BTreeMap::new();
        properties.insert("path".to_owned(), JsonSchema::new(JsonSchemaType::String));
        tool.expect_schema().return_const(FunctionDecl::new(
            "read_file",
            None,
            JsonSchema::object(properties, vec!["path".to_owned()]),
        ));
        tool.expect_safety_tier().return_const(SafetyTier::ReadOnly);
        tool.expect_timeout().return_const(Duration::from_secs(1));
        tool.expect_execute()
            .times(1)
            .withf(|args| args.contains("\"path\":\"Cargo.toml\""))
            .returning(|_| Ok("mockall read: Cargo.toml".to_owned()));

        let mut tools = ToolRegistry::default();
        tools.register("read_file", tool);
        let runtime = AgentRuntime::new(Box::new(provider), tools, RuntimeLimits::default());
        let mut context = test_context(provider_id, model_id);

        let response = runtime
            .run_session(&mut context, &CancellationToken::new())
            .await
            .expect("mockall tool should execute and loop to completion");

        assert_eq!(response.message.content.as_deref(), Some("tool complete"));
        assert!(matches!(context.messages[2].role, MessageRole::Tool));
        assert_eq!(
            context.messages[2].content.as_deref(),
            Some("mockall read: Cargo.toml")
        );
    }

    #[tokio::test]
    async fn run_session_cancels_before_provider_call() {
        let provider_id = ProviderId::from("openai");
        let model_id = ModelId::from("gpt-4o-mini");
        let mut provider = mock_provider(provider_id.clone(), model_id.clone(), false);
        provider.expect_stream().never();
        provider.expect_complete().never();
        let runtime = AgentRuntime::new(
            Box::new(provider),
            ToolRegistry::default(),
            RuntimeLimits::default(),
        );
        let mut context = test_context(provider_id, model_id);
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        let error = runtime
            .run_session(&mut context, &cancellation)
            .await
            .expect_err("cancelled token should short-circuit the turn");

        assert!(matches!(error, types::RuntimeError::Cancelled));
    }

    #[tokio::test]
    async fn run_session_cancels_during_provider_call() {
        let provider_id = ProviderId::from("openai");
        let model_id = ModelId::from("gpt-4o-mini");
        let provider = FakeProvider::new(
            provider_id.clone(),
            test_catalog(provider_id.clone(), model_id.clone(), false),
            vec![ProviderStep::CompleteDelayed {
                response: assistant_response("late response", vec![]),
                delay: Duration::from_millis(250),
            }],
        );
        let runtime = AgentRuntime::new(
            Box::new(provider),
            ToolRegistry::default(),
            RuntimeLimits {
                turn_timeout: Duration::from_secs(2),
                max_turns: 3,
                max_cost: None,
            },
        );
        let mut context = test_context(provider_id, model_id);
        let cancellation = CancellationToken::new();
        let cancellation_clone = cancellation.clone();
        tokio::spawn(async move {
            sleep(Duration::from_millis(30)).await;
            cancellation_clone.cancel();
        });

        let error = runtime
            .run_session(&mut context, &cancellation)
            .await
            .expect_err("provider await should observe cancellation");
        assert!(matches!(error, types::RuntimeError::Cancelled));
    }

    #[tokio::test]
    async fn run_session_errors_when_provider_stage_times_out() {
        let provider_id = ProviderId::from("openai");
        let model_id = ModelId::from("gpt-4o-mini");
        let provider = FakeProvider::new(
            provider_id.clone(),
            test_catalog(provider_id.clone(), model_id.clone(), false),
            vec![ProviderStep::CompleteDelayed {
                response: assistant_response("late response", vec![]),
                delay: Duration::from_millis(100),
            }],
        );
        let runtime = AgentRuntime::new(
            Box::new(provider),
            ToolRegistry::default(),
            RuntimeLimits {
                turn_timeout: Duration::from_millis(10),
                max_turns: 2,
                max_cost: None,
            },
        );
        let mut context = test_context(provider_id, model_id);

        let error = runtime
            .run_session(&mut context, &CancellationToken::new())
            .await
            .expect_err("provider call should be bounded by turn timeout");
        assert!(matches!(error, types::RuntimeError::BudgetExceeded));
    }

    #[tokio::test]
    async fn run_session_errors_when_tool_stage_times_out() {
        let provider_id = ProviderId::from("openai");
        let model_id = ModelId::from("gpt-4o-mini");
        let provider = FakeProvider::new(
            provider_id.clone(),
            test_catalog(provider_id.clone(), model_id.clone(), true),
            vec![ProviderStep::Stream(vec![
                Ok(StreamItem::ToolCallDelta(ToolCallDelta {
                    index: 0,
                    id: Some("call_1".to_owned()),
                    name: Some("slow_tool".to_owned()),
                    arguments: Some("{}".to_owned()),
                })),
                Ok(StreamItem::FinishReason("tool_calls".to_owned())),
            ])],
        );

        let mut tools = ToolRegistry::default();
        tools.register("slow_tool", SlowTool);
        let runtime = AgentRuntime::new(
            Box::new(provider),
            tools,
            RuntimeLimits {
                turn_timeout: Duration::from_millis(5),
                max_turns: 2,
                max_cost: None,
            },
        );
        let mut context = test_context(provider_id, model_id);

        let error = runtime
            .run_session(&mut context, &CancellationToken::new())
            .await
            .expect_err("tool stage should respect runtime timeout");
        assert!(matches!(error, types::RuntimeError::BudgetExceeded));
    }

    #[tokio::test]
    async fn run_session_errors_when_max_turn_budget_is_exceeded() {
        let provider_id = ProviderId::from("openai");
        let model_id = ModelId::from("gpt-4o-mini");
        let provider = FakeProvider::new(
            provider_id.clone(),
            test_catalog(provider_id.clone(), model_id.clone(), true),
            vec![ProviderStep::Stream(vec![
                Ok(StreamItem::ToolCallDelta(ToolCallDelta {
                    index: 0,
                    id: Some("call_1".to_owned()),
                    name: Some("read_file".to_owned()),
                    arguments: Some("{\"path\":\"Cargo.toml\"}".to_owned()),
                })),
                Ok(StreamItem::FinishReason("tool_calls".to_owned())),
            ])],
        );
        let mut tools = ToolRegistry::default();
        tools.register("read_file", MockReadTool);
        let runtime = AgentRuntime::new(
            Box::new(provider),
            tools,
            RuntimeLimits {
                turn_timeout: Duration::from_secs(1),
                max_turns: 1,
                max_cost: None,
            },
        );
        let mut context = test_context(provider_id, model_id);

        let error = runtime
            .run_session(&mut context, &CancellationToken::new())
            .await
            .expect_err("runtime should stop after max turn budget");
        assert!(matches!(error, types::RuntimeError::BudgetExceeded));
    }

    #[tokio::test]
    async fn run_session_errors_when_max_cost_budget_is_exceeded() {
        let provider_id = ProviderId::from("openai");
        let model_id = ModelId::from("gpt-4o-mini");
        let provider = FakeProvider::new(
            provider_id.clone(),
            test_catalog(provider_id.clone(), model_id.clone(), true),
            vec![ProviderStep::Stream(vec![
                Ok(StreamItem::UsageUpdate(UsageUpdate {
                    prompt_tokens: Some(4),
                    completion_tokens: Some(2),
                    total_tokens: Some(6),
                })),
                Ok(StreamItem::Text("expensive turn".to_owned())),
                Ok(StreamItem::FinishReason("stop".to_owned())),
            ])],
        );
        let runtime = AgentRuntime::new(
            Box::new(provider),
            ToolRegistry::default(),
            RuntimeLimits {
                turn_timeout: Duration::from_secs(1),
                max_turns: 2,
                max_cost: Some(5.0),
            },
        );
        let mut context = test_context(provider_id, model_id);

        let error = runtime
            .run_session(&mut context, &CancellationToken::new())
            .await
            .expect_err("max cost guard should fail fast once budget is exceeded");
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
            tools: vec![],
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
            usage: None,
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

    #[test]
    fn streamed_tool_calls_require_id_field() {
        let provider = ProviderId::from("openai");
        let mut accumulator = super::ToolCallAccumulator::default();
        accumulator.merge(ToolCallDelta {
            index: 0,
            id: None,
            name: Some("noop".to_owned()),
            arguments: Some("{}".to_owned()),
        });

        let error = accumulator
            .build(&provider)
            .expect_err("missing id should fail reconstruction");
        assert!(
            matches!(error, ProviderError::ResponseParse { message, .. } if message.contains("missing id"))
        );
    }

    #[test]
    fn streamed_tool_calls_require_function_name() {
        let provider = ProviderId::from("openai");
        let mut accumulator = super::ToolCallAccumulator::default();
        accumulator.merge(ToolCallDelta {
            index: 0,
            id: Some("call_1".to_owned()),
            name: None,
            arguments: Some("{}".to_owned()),
        });

        let error = accumulator
            .build(&provider)
            .expect_err("missing function name should fail reconstruction");
        assert!(
            matches!(error, ProviderError::ResponseParse { message, .. } if message.contains("missing function name"))
        );
    }
}
