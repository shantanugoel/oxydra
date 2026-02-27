use std::collections::BTreeMap;

use super::*;
use runtime::ScheduledTurnRunner;
use types::{AgentDefinition, ChannelCapabilities, InlineMedia, ProviderSelection};

/// User-submitted content for a single turn (text + optional media).
pub struct UserTurnInput {
    pub prompt: String,
    pub attachments: Vec<InlineMedia>,
}

/// Per-turn channel origin, passed alongside a turn submission.
/// Captures the ingress channel so tools (e.g. schedule_create) can record
/// where a turn originated, enabling origin-only notification routing.
#[derive(Debug, Clone, Default)]
pub struct TurnOrigin {
    pub channel_id: Option<String>,
    pub channel_context_id: Option<String>,
    /// Agent name associated with the active session.
    pub agent_name: Option<String>,
    /// Capabilities of the channel the user is connected through.
    pub channel_capabilities: Option<ChannelCapabilities>,
}

#[async_trait]
pub trait GatewayTurnRunner: Send + Sync {
    async fn run_turn(
        &self,
        user_id: &str,
        session_id: &str,
        input: UserTurnInput,
        cancellation: CancellationToken,
        delta_sender: mpsc::UnboundedSender<StreamItem>,
        origin: TurnOrigin,
    ) -> Result<Response, RuntimeError>;

    async fn drop_session_context(&self, session_id: &str);
}

pub struct RuntimeGatewayTurnRunner {
    runtime: Arc<AgentRuntime>,
    default_selection: ProviderSelection,
    agent_selections: BTreeMap<String, ProviderSelection>,
    contexts: Mutex<HashMap<String, Context>>,
}

impl RuntimeGatewayTurnRunner {
    pub fn new(
        runtime: Arc<AgentRuntime>,
        default_selection: ProviderSelection,
        agents: BTreeMap<String, AgentDefinition>,
    ) -> Self {
        let mut agent_selections = BTreeMap::new();
        for (agent_name, definition) in agents {
            if agent_name == "default" {
                continue;
            }
            if let Some(selection) = definition.selection {
                agent_selections.insert(agent_name, selection);
            }
        }
        Self {
            runtime,
            default_selection,
            agent_selections,
            contexts: Mutex::new(HashMap::new()),
        }
    }

    fn selection_for_agent(&self, agent_name: &str) -> ProviderSelection {
        if agent_name == "default" {
            return self.default_selection.clone();
        }
        self.agent_selections
            .get(agent_name)
            .cloned()
            .unwrap_or_else(|| self.default_selection.clone())
    }

    fn base_context(&self, agent_name: &str) -> Context {
        let selection = self.selection_for_agent(agent_name);
        Context {
            provider: selection.provider,
            model: selection.model,
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
        session_id: &str,
        input: UserTurnInput,
        cancellation: CancellationToken,
        delta_sender: mpsc::UnboundedSender<StreamItem>,
        origin: TurnOrigin,
    ) -> Result<Response, RuntimeError> {
        let UserTurnInput {
            prompt,
            attachments,
        } = input;
        let TurnOrigin {
            channel_id,
            channel_context_id,
            agent_name,
            channel_capabilities,
        } = origin;
        let effective_agent_name = agent_name.as_deref().unwrap_or("default");

        let mut context = {
            let mut contexts = self.contexts.lock().await;
            contexts
                .entry(session_id.to_owned())
                .or_insert_with(|| self.base_context(effective_agent_name))
                .clone()
        };
        let tool_context = types::ToolExecutionContext {
            user_id: Some(user_id.to_owned()),
            session_id: Some(session_id.to_owned()),
            provider: Some(context.provider.clone()),
            model: Some(context.model.clone()),
            channel_capabilities,
            event_sender: None,
            channel_id,
            channel_context_id,
        };

        // Strip attachment bytes from older user messages to prevent unbounded
        // memory growth when users send many images/audio clips.
        for msg in &mut context.messages {
            if msg.role == MessageRole::User && !msg.attachments.is_empty() {
                msg.attachments.clear();
            }
        }

        // Save the message count before this turn so we can roll back on failure.
        // This prevents a failed turn from leaving dangling tool-call state in the
        // history that would confuse the LLM on the next user message.
        let pre_turn_message_count = context.messages.len();

        context.messages.push(Message {
            role: MessageRole::User,
            content: Some(prompt),
            tool_calls: Vec::new(),
            tool_call_id: None,
            attachments,
        });

        let (stream_events_tx, mut stream_events_rx): (RuntimeStreamEventSender, _) =
            mpsc::unbounded_channel();
        let runtime = Arc::clone(&self.runtime);
        let session_id_owned = session_id.to_owned();
        let runtime_cancellation = cancellation.clone();
        let runtime_future = async move {
            let mut run_context = context;
            let result = runtime
                .run_session_for_session_with_stream_events(
                    &session_id_owned,
                    &mut run_context,
                    &runtime_cancellation,
                    stream_events_tx,
                    &tool_context,
                )
                .await;
            (result, run_context)
        };
        tokio::pin!(runtime_future);

        let (result, mut context) = loop {
            tokio::select! {
                maybe_event = stream_events_rx.recv() => {
                    match maybe_event {
                        // Forward text deltas, progress events, and media
                        // attachments to the gateway.
                        Some(item @ StreamItem::Text(_))
                        | Some(item @ StreamItem::Progress(_))
                        | Some(item @ StreamItem::Media(_)) => {
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
        contexts.insert(session_id.to_owned(), context);
        result
    }

    async fn drop_session_context(&self, session_id: &str) {
        self.contexts.lock().await.remove(session_id);
    }
}

#[async_trait]
impl ScheduledTurnRunner for RuntimeGatewayTurnRunner {
    async fn run_scheduled_turn(
        &self,
        user_id: &str,
        session_id: &str,
        prompt: String,
        cancellation: CancellationToken,
    ) -> Result<String, RuntimeError> {
        let (delta_tx, _delta_rx) = mpsc::unbounded_channel();
        let input = UserTurnInput {
            prompt,
            attachments: Vec::new(),
        };
        let response = self
            .run_turn(
                user_id,
                session_id,
                input,
                cancellation,
                delta_tx,
                TurnOrigin::default(),
            )
            .await?;
        Ok(response.message.content.unwrap_or_default())
    }
}
