use std::{collections::BTreeMap, sync::Arc, time::Duration};

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tools::ToolRegistry;
use types::{
    Context, Memory, MemoryError, MemoryHybridQueryRequest, MemoryRecallRequest, MemoryRetrieval,
    MemoryStoreRequest, MemorySummaryReadRequest, MemorySummaryState, MemorySummaryWriteRequest,
    Message, MessageRole, Provider, ProviderError, ProviderId, Response, RuntimeError,
    RuntimeProgressEvent, RuntimeProgressKind, SafetyTier, StreamItem, ToolCall, ToolCallDelta,
    ToolError, ToolExecutionContext, UsageUpdate,
};

const DEFAULT_STREAM_BUFFER_SIZE: usize = 64;
const INVALID_TOOL_ARGS_RAW_KEY: &str = "__oxydra_invalid_tool_args_raw";
const INVALID_TOOL_ARGS_ERROR_KEY: &str = "__oxydra_invalid_tool_args_error";

pub type RuntimeStreamEventSender = mpsc::UnboundedSender<StreamItem>;

mod budget;
mod memory;
mod provider_response;
mod scheduler_executor;
mod scrubbing;
mod tool_execution;

pub use scheduler_executor::{SchedulerExecutor, SchedulerNotifier};
pub use scrubbing::PathScrubMapping;

/// Trait that the gateway layer implements so the scheduler executor can
/// trigger agent turns without depending on the gateway crate directly.
#[async_trait]
pub trait ScheduledTurnRunner: Send + Sync {
    async fn run_scheduled_turn(
        &self,
        user_id: &str,
        runtime_session_id: &str,
        prompt: String,
        cancellation: CancellationToken,
    ) -> Result<String, RuntimeError>;
}

#[cfg(test)]
mod tests;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnState {
    Streaming,
    ToolExecution,
    Yielding,
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct ContextBudgetLimits {
    pub trigger_ratio: f64,
    pub safety_buffer_tokens: u64,
    pub fallback_max_context_tokens: u32,
}

#[derive(Debug, Clone)]
pub struct RetrievalLimits {
    pub top_k: usize,
    pub vector_weight: f64,
    pub fts_weight: f64,
}

#[derive(Debug, Clone)]
pub struct SummarizationLimits {
    pub target_ratio: f64,
    pub min_turns: usize,
}

#[derive(Debug, Clone)]
pub struct RuntimeLimits {
    pub turn_timeout: Duration,
    pub max_turns: usize,
    pub max_cost: Option<f64>,
    pub context_budget: ContextBudgetLimits,
    pub retrieval: RetrievalLimits,
    pub summarization: SummarizationLimits,
}

impl Default for RuntimeLimits {
    fn default() -> Self {
        Self {
            turn_timeout: Duration::from_secs(60),
            max_turns: 8,
            max_cost: None,
            context_budget: ContextBudgetLimits {
                trigger_ratio: 0.85,
                safety_buffer_tokens: 1_024,
                fallback_max_context_tokens: 128_000,
            },
            retrieval: RetrievalLimits {
                top_k: 8,
                vector_weight: 0.7,
                fts_weight: 0.3,
            },
            summarization: SummarizationLimits {
                target_ratio: 0.5,
                min_turns: 6,
            },
        }
    }
}

pub struct AgentRuntime {
    provider: Box<dyn Provider>,
    tool_registry: ToolRegistry,
    limits: RuntimeLimits,
    stream_buffer_size: usize,
    memory: Option<Arc<dyn Memory>>,
    memory_retrieval: Option<Arc<dyn MemoryRetrieval>>,
    tool_execution_context: std::sync::Arc<tokio::sync::Mutex<ToolExecutionContext>>,
    path_scrub_mappings: Vec<PathScrubMapping>,
    system_prompt: Option<String>,
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
            memory_retrieval: None,
            tool_execution_context: std::sync::Arc::new(tokio::sync::Mutex::new(
                ToolExecutionContext::default(),
            )),
            path_scrub_mappings: Vec::new(),
            system_prompt: None,
        }
    }

    /// Deprecated: prefer [`with_memory_retrieval`] which preserves the
    /// retrieval layer for hybrid search and context injection.  This method
    /// is kept for backward compatibility but clears `memory_retrieval`.
    pub fn with_memory(mut self, memory: Arc<dyn Memory>) -> Self {
        self.memory = Some(memory);
        self.memory_retrieval = None;
        self
    }

    pub fn with_memory_retrieval(mut self, memory: Arc<dyn MemoryRetrieval>) -> Self {
        self.memory = Some(memory.clone());
        self.memory_retrieval = Some(memory);
        self
    }

    pub fn with_stream_buffer_size(mut self, stream_buffer_size: usize) -> Self {
        self.stream_buffer_size = stream_buffer_size.max(1);
        self
    }

    /// Set the host-path-to-virtual-path mappings used to scrub tool output
    /// before injecting it into the LLM context.
    ///
    /// For example, mapping the host path
    /// `/Users/alice/.oxydra/workspaces/bob/shared` → `/shared` ensures the
    /// LLM never sees host-specific filesystem details in tool results or
    /// security policy error messages.
    pub fn with_path_scrub_mappings(mut self, mappings: Vec<PathScrubMapping>) -> Self {
        self.path_scrub_mappings = mappings;
        self
    }

    /// Set the system prompt injected at the start of the conversation if
    /// no `MessageRole::System` message is already present in the context.
    pub fn with_system_prompt(mut self, prompt: String) -> Self {
        self.system_prompt = Some(prompt);
        self
    }

    /// Set the per-turn tool execution context (user_id, session_id) that
    /// memory tools use to scope operations to the correct user namespace.
    pub async fn set_tool_execution_context(&self, context: ToolExecutionContext) {
        *self.tool_execution_context.lock().await = context;
    }

    pub fn limits(&self) -> &RuntimeLimits {
        &self.limits
    }

    pub async fn run_session(
        &self,
        context: &mut Context,
        cancellation: &CancellationToken,
    ) -> Result<Response, RuntimeError> {
        self.run_session_internal(None, context, cancellation, None)
            .await
    }

    pub async fn run_session_for_session(
        &self,
        session_id: &str,
        context: &mut Context,
        cancellation: &CancellationToken,
    ) -> Result<Response, RuntimeError> {
        self.run_session_internal(Some(session_id), context, cancellation, None)
            .await
    }

    pub async fn run_session_for_session_with_stream_events(
        &self,
        session_id: &str,
        context: &mut Context,
        cancellation: &CancellationToken,
        stream_events: RuntimeStreamEventSender,
    ) -> Result<Response, RuntimeError> {
        self.run_session_internal(Some(session_id), context, cancellation, Some(stream_events))
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
        stream_events: Option<RuntimeStreamEventSender>,
    ) -> Result<Response, RuntimeError> {
        if context.tools.is_empty() {
            context.tools = self.tool_registry.schemas();
        }

        // Inject the system prompt if one is configured and the context does
        // not already contain a system message (to allow callers to provide
        // their own).
        if let Some(ref prompt) = self.system_prompt {
            let has_system = context
                .messages
                .iter()
                .any(|m| m.role == MessageRole::System);
            if !has_system {
                context.messages.insert(
                    0,
                    Message {
                        role: MessageRole::System,
                        content: Some(prompt.clone()),
                        tool_calls: Vec::new(),
                        tool_call_id: None,
                    },
                );
            }
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
            tracing::info!(turn, max_turns = self.limits.max_turns, "calling provider");

            // Notify listeners that a provider call is about to happen.
            if let Some(ref sender) = stream_events {
                let _ = sender.send(StreamItem::Progress(RuntimeProgressEvent {
                    kind: RuntimeProgressKind::ProviderCall,
                    message: format!("[{turn}/{}] Calling provider", self.limits.max_turns),
                    turn,
                    max_turns: self.limits.max_turns,
                }));
            }

            self.maybe_trigger_rolling_summary(session_id, context)
                .await?;
            let provider_context = self.prepare_provider_context(session_id, context).await?;
            let provider_response = self
                .request_provider_response(&provider_context, cancellation, stream_events.clone())
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

            // Notify listeners about the upcoming tool execution batch.
            if let Some(ref sender) = stream_events {
                let tool_names: Vec<String> = tool_calls.iter().map(|tc| tc.name.clone()).collect();
                let names_display = tool_names.join(", ");
                let _ = sender.send(StreamItem::Progress(RuntimeProgressEvent {
                    kind: RuntimeProgressKind::ToolExecution {
                        tool_names: tool_names.clone(),
                    },
                    message: format!(
                        "[{turn}/{}] Executing tools: {names_display}",
                        self.limits.max_turns
                    ),
                    turn,
                    max_turns: self.limits.max_turns,
                }));
            }

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
}

#[derive(Debug)]
enum StreamCollectError {
    Provider(ProviderError),
    ConnectionLost(String),
    Cancelled,
    TurnTimedOut,
}

/// When the LLM stuffs multiple JSON objects into a single tool-call arguments
/// string (e.g. `{"query":"a"}{"query":"b"}`), `serde_json::from_str` rejects
/// the concatenation with a "trailing characters" error.
///
/// This helper extracts all valid JSON objects from the concatenation so they
/// can be fanned out as parallel tool calls. When there is only a single
/// salvageable object (i.e. the remainder is garbage, not another object) we
/// return it alone so the caller can use the existing single-call path.
///
/// Returns `None` when the input is a single valid object (no concatenation) or
/// when no objects can be extracted at all.
fn try_extract_concatenated_json_objects(raw: &str) -> Option<Vec<serde_json::Value>> {
    let trimmed = raw.trim();
    if !trimmed.starts_with('{') {
        return None;
    }
    let stream = serde_json::Deserializer::from_str(trimmed).into_iter::<serde_json::Value>();
    let objects: Vec<serde_json::Value> = stream.filter_map(|r| r.ok()).collect();
    if objects.len() <= 1 {
        // Either nothing parsed or the whole string was one object — let the
        // normal parse path handle it (including trailing-junk errors).
        return None;
    }
    Some(objects)
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
    /// First non-None metadata wins; used to carry thought_signature through
    /// the streaming pipeline for Gemini thinking models.
    metadata: Option<serde_json::Value>,
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
        if entry.metadata.is_none() {
            entry.metadata = delta.metadata;
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
            match serde_json::from_str(arguments) {
                Ok(parsed_arguments) => {
                    tool_calls.push(ToolCall {
                        id,
                        name,
                        arguments: parsed_arguments,
                        metadata: entry.metadata,
                    });
                }
                Err(error) => match try_extract_concatenated_json_objects(arguments) {
                    Some(objects) => {
                        tracing::info!(
                            tool = %name,
                            count = objects.len(),
                            "tool call arguments contain concatenated JSON objects; \
                             fanning out as parallel tool calls"
                        );
                        for (i, obj) in objects.into_iter().enumerate() {
                            tool_calls.push(ToolCall {
                                id: format!("{id}_{i}"),
                                name: name.clone(),
                                arguments: obj,
                                metadata: entry.metadata.clone(),
                            });
                        }
                    }
                    None => {
                        let mut payload = serde_json::Map::new();
                        payload.insert(
                            INVALID_TOOL_ARGS_RAW_KEY.to_owned(),
                            serde_json::Value::String(arguments.to_owned()),
                        );
                        payload.insert(
                            INVALID_TOOL_ARGS_ERROR_KEY.to_owned(),
                            serde_json::Value::String(error.to_string()),
                        );
                        tool_calls.push(ToolCall {
                            id,
                            name,
                            arguments: serde_json::Value::Object(payload),
                            metadata: entry.metadata,
                        });
                    }
                },
            };
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
