use super::*;
use runtime::ScheduledTurnRunner;

#[async_trait]
pub trait GatewayTurnRunner: Send + Sync {
    async fn run_turn(
        &self,
        user_id: &str,
        runtime_session_id: &str,
        prompt: String,
        cancellation: CancellationToken,
        delta_sender: mpsc::UnboundedSender<StreamItem>,
    ) -> Result<Response, RuntimeError>;
}

pub struct RuntimeGatewayTurnRunner {
    runtime: Arc<AgentRuntime>,
    provider: ProviderId,
    model: ModelId,
    contexts: Mutex<HashMap<String, Context>>,
}

impl RuntimeGatewayTurnRunner {
    pub fn new(runtime: Arc<AgentRuntime>, provider: ProviderId, model: ModelId) -> Self {
        Self {
            runtime,
            provider,
            model,
            contexts: Mutex::new(HashMap::new()),
        }
    }

    fn base_context(&self) -> Context {
        Context {
            provider: self.provider.clone(),
            model: self.model.clone(),
            tools: Vec::new(),
            messages: Vec::new(),
        }
    }
}

#[async_trait]
impl GatewayTurnRunner for RuntimeGatewayTurnRunner {
    async fn run_turn(
        &self,
        user_id: &str,
        runtime_session_id: &str,
        prompt: String,
        cancellation: CancellationToken,
        delta_sender: mpsc::UnboundedSender<StreamItem>,
    ) -> Result<Response, RuntimeError> {
        // Set the per-turn tool execution context so memory tools resolve
        // to the correct user namespace. This is safe for concurrent turns
        // because the context is cloned per tool invocation in the runtime.
        self.runtime
            .set_tool_execution_context(types::ToolExecutionContext {
                user_id: Some(user_id.to_owned()),
                session_id: Some(runtime_session_id.to_owned()),
            })
            .await;

        let mut context = {
            let mut contexts = self.contexts.lock().await;
            contexts
                .entry(runtime_session_id.to_owned())
                .or_insert_with(|| self.base_context())
                .clone()
        };

        // Save the message count before this turn so we can roll back on failure.
        // This prevents a failed turn from leaving dangling tool-call state in the
        // history that would confuse the LLM on the next user message.
        let pre_turn_message_count = context.messages.len();

        context.messages.push(Message {
            role: MessageRole::User,
            content: Some(prompt),
            tool_calls: Vec::new(),
            tool_call_id: None,
        });

        let (stream_events_tx, mut stream_events_rx): (RuntimeStreamEventSender, _) =
            mpsc::unbounded_channel();
        let runtime = Arc::clone(&self.runtime);
        let runtime_session_id_owned = runtime_session_id.to_owned();
        let runtime_cancellation = cancellation.clone();
        let runtime_future = async move {
            let mut run_context = context;
            let result = runtime
                .run_session_for_session_with_stream_events(
                    &runtime_session_id_owned,
                    &mut run_context,
                    &runtime_cancellation,
                    stream_events_tx,
                )
                .await;
            (result, run_context)
        };
        tokio::pin!(runtime_future);

        let (result, mut context) = loop {
            tokio::select! {
                maybe_event = stream_events_rx.recv() => {
                    match maybe_event {
                        // Forward text deltas and progress events to the gateway.
                        // Other stream item types (tool call assembly, reasoning
                        // traces, usage updates) are not surfaced to channels.
                        Some(item @ StreamItem::Text(_)) | Some(item @ StreamItem::Progress(_)) => {
                            let _ = delta_sender.send(item);
                        }
                        _ => {}
                    }
                }
                result = &mut runtime_future => {
                    break result;
                }
            }
        };

        if result.is_err() {
            // Roll back to the pre-turn state so the next user turn starts from a
            // clean history without any partially-executed tool calls or provider
            // errors left over from the failed turn.
            context.messages.truncate(pre_turn_message_count);
        }

        let mut contexts = self.contexts.lock().await;
        contexts.insert(runtime_session_id.to_owned(), context);
        result
    }
}

#[async_trait]
impl ScheduledTurnRunner for RuntimeGatewayTurnRunner {
    async fn run_scheduled_turn(
        &self,
        user_id: &str,
        runtime_session_id: &str,
        prompt: String,
        cancellation: CancellationToken,
    ) -> Result<String, RuntimeError> {
        let (delta_tx, _delta_rx) = mpsc::unbounded_channel();
        let response = self
            .run_turn(user_id, runtime_session_id, prompt, cancellation, delta_tx)
            .await?;
        Ok(response.message.content.unwrap_or_default())
    }
}
