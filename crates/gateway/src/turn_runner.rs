use super::*;

#[async_trait]
pub trait GatewayTurnRunner: Send + Sync {
    async fn run_turn(
        &self,
        runtime_session_id: &str,
        prompt: String,
        cancellation: CancellationToken,
        delta_sender: mpsc::UnboundedSender<String>,
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
        runtime_session_id: &str,
        prompt: String,
        cancellation: CancellationToken,
        delta_sender: mpsc::UnboundedSender<String>,
    ) -> Result<Response, RuntimeError> {
        let mut context = {
            let mut contexts = self.contexts.lock().await;
            contexts
                .entry(runtime_session_id.to_owned())
                .or_insert_with(|| self.base_context())
                .clone()
        };

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

        let (result, context) = loop {
            tokio::select! {
                maybe_event = stream_events_rx.recv() => {
                    if let Some(StreamItem::Text(delta)) = maybe_event {
                        let _ = delta_sender.send(delta);
                    }
                }
                result = &mut runtime_future => {
                    break result;
                }
            }
        };

        let mut contexts = self.contexts.lock().await;
        contexts.insert(runtime_session_id.to_owned(), context);
        result
    }
}
