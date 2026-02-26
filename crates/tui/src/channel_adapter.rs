use std::sync::Arc;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use tokio::{
    net::TcpStream,
    sync::{Mutex, broadcast, mpsc},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message as WsMessage,
};
use types::{
    Channel, ChannelError, ChannelHealthStatus, ChannelInboundEvent, ChannelListenStream,
    ChannelOutboundEvent, GATEWAY_PROTOCOL_VERSION, GatewayCancelActiveTurn, GatewayClientFrame,
    GatewayClientHello, GatewayCreateSession, GatewayHealthCheck, GatewayListSessions,
    GatewayServerFrame, GatewaySwitchSession, GatewayTurnProgress, GatewayTurnState,
    GatewayTurnStatus,
};

const TUI_CHANNEL_ID: &str = "tui";
const EVENT_BUFFER_CAPACITY: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TuiCtrlCOutcome {
    CancelActiveTurn,
    Exit,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TuiUiState {
    pub connected: bool,
    pub session_id: Option<String>,
    pub active_turn_id: Option<String>,
    pub prompt_buffer: String,
    pub rendered_output: String,
    pub last_error: Option<String>,
    /// Most recent runtime progress message received during an active turn,
    /// e.g. `"[2/8] Executing tools: file_read"`.  Cleared when the turn
    /// completes, is cancelled, or errors.  Used to populate the input bar
    /// title while the user waits for the agent.
    pub activity_status: Option<String>,
    /// Most recent notification from a completed scheduled task. Displayed
    /// once and then cleared on the next render cycle.
    pub last_scheduled_notification: Option<String>,
}

impl TuiUiState {
    fn mark_active_turn(&mut self, turn: &GatewayTurnStatus) {
        if turn.state == GatewayTurnState::Running {
            self.active_turn_id = Some(turn.turn_id.clone());
        }
    }

    fn clear_active_turn(&mut self) {
        self.active_turn_id = None;
    }
}

#[derive(Clone)]
pub struct TuiChannelAdapter {
    user_id: String,
    connection_id: String,
    inbound_sender: broadcast::Sender<Result<ChannelInboundEvent, ChannelError>>,
    state: Arc<Mutex<TuiUiState>>,
    /// If set by `--session`, this session ID is used in the first Hello.
    /// After connection, the gateway's HelloAck session_id takes over.
    initial_session_id: Arc<Mutex<Option<String>>>,
    /// Whether the first connection should request a new session.
    create_new_session_on_connect: bool,
}

impl TuiChannelAdapter {
    pub fn new(user_id: impl Into<String>, connection_id: impl Into<String>) -> Self {
        let (inbound_sender, _) = broadcast::channel(EVENT_BUFFER_CAPACITY);
        Self {
            user_id: user_id.into(),
            connection_id: connection_id.into(),
            inbound_sender,
            state: Arc::new(Mutex::new(TuiUiState::default())),
            initial_session_id: Arc::new(Mutex::new(None)),
            create_new_session_on_connect: true,
        }
    }

    /// Create a new adapter with a specific session ID to join on connect.
    pub fn with_session_id(
        user_id: impl Into<String>,
        connection_id: impl Into<String>,
        session_id: String,
    ) -> Self {
        let (inbound_sender, _) = broadcast::channel(EVENT_BUFFER_CAPACITY);
        Self {
            user_id: user_id.into(),
            connection_id: connection_id.into(),
            inbound_sender,
            state: Arc::new(Mutex::new(TuiUiState::default())),
            initial_session_id: Arc::new(Mutex::new(Some(session_id))),
            create_new_session_on_connect: false,
        }
    }

    pub async fn state_snapshot(&self) -> TuiUiState {
        self.state.lock().await.clone()
    }

    pub async fn mark_disconnected(&self) {
        self.state.lock().await.connected = false;
    }

    pub async fn build_hello_frame(&self, request_id: impl Into<String>) -> GatewayClientFrame {
        let state = self.state.lock().await;
        // If we have a session_id from a prior HelloAck (reconnection), use it.
        if let Some(ref sid) = state.session_id {
            return GatewayClientFrame::Hello(GatewayClientHello {
                request_id: request_id.into(),
                protocol_version: GATEWAY_PROTOCOL_VERSION,
                user_id: self.user_id.clone(),
                session_id: Some(sid.clone()),
                create_new_session: false,
            });
        }
        drop(state);

        // First connection: check for --session flag.
        let initial = self.initial_session_id.lock().await.clone();
        if let Some(sid) = initial {
            return GatewayClientFrame::Hello(GatewayClientHello {
                request_id: request_id.into(),
                protocol_version: GATEWAY_PROTOCOL_VERSION,
                user_id: self.user_id.clone(),
                session_id: Some(sid),
                create_new_session: false,
            });
        }

        // Default behavior: create a new session.
        GatewayClientFrame::Hello(GatewayClientHello {
            request_id: request_id.into(),
            protocol_version: GATEWAY_PROTOCOL_VERSION,
            user_id: self.user_id.clone(),
            session_id: None,
            create_new_session: self.create_new_session_on_connect,
        })
    }

    pub async fn submit_prompt(
        &self,
        request_id: impl Into<String>,
        turn_id: impl Into<String>,
        prompt: impl Into<String>,
    ) -> Result<(), ChannelError> {
        let request_id = request_id.into();
        let turn_id = turn_id.into();
        let prompt = prompt.into();
        let session_id = {
            let mut state = self.state.lock().await;
            state.prompt_buffer = prompt.clone();
            state
                .session_id
                .clone()
                .ok_or_else(|| ChannelError::Unavailable {
                    channel: TUI_CHANNEL_ID.to_owned(),
                })?
        };

        self.enqueue_client_frame(GatewayClientFrame::SendTurn(types::GatewaySendTurn {
            request_id,
            session_id,
            turn_id,
            prompt,
            attachments: Vec::new(),
        }))
    }

    pub async fn handle_ctrl_c(
        &self,
        request_id: impl Into<String>,
    ) -> Result<TuiCtrlCOutcome, ChannelError> {
        let request_id = request_id.into();
        let (session_id, turn_id) = {
            let state = self.state.lock().await;
            match (&state.session_id, &state.active_turn_id) {
                (Some(session_id), Some(turn_id)) => (session_id.clone(), turn_id.clone()),
                _ => return Ok(TuiCtrlCOutcome::Exit),
            }
        };

        self.enqueue_client_frame(GatewayClientFrame::CancelActiveTurn(
            GatewayCancelActiveTurn {
                request_id,
                session_id,
                turn_id,
            },
        ))?;
        Ok(TuiCtrlCOutcome::CancelActiveTurn)
    }

    fn enqueue_client_frame(&self, frame: GatewayClientFrame) -> Result<(), ChannelError> {
        self.inbound_sender
            .send(Ok(ChannelInboundEvent {
                channel_id: TUI_CHANNEL_ID.to_owned(),
                connection_id: self.connection_id.clone(),
                frame,
            }))
            .map(|_| ())
            .map_err(|_| ChannelError::Unavailable {
                channel: TUI_CHANNEL_ID.to_owned(),
            })
    }

    /// Send a CreateSession frame to the gateway.
    pub fn create_session(
        &self,
        request_id: impl Into<String>,
        display_name: Option<String>,
    ) -> Result<(), ChannelError> {
        self.enqueue_client_frame(GatewayClientFrame::CreateSession(GatewayCreateSession {
            request_id: request_id.into(),
            display_name,
            agent_name: None,
        }))
    }

    /// Send a ListSessions frame to the gateway.
    pub fn list_sessions(&self, request_id: impl Into<String>) -> Result<(), ChannelError> {
        self.enqueue_client_frame(GatewayClientFrame::ListSessions(GatewayListSessions {
            request_id: request_id.into(),
            include_archived: false,
            include_subagent_sessions: false,
        }))
    }

    /// Send a SwitchSession frame to the gateway.
    pub fn switch_session(
        &self,
        request_id: impl Into<String>,
        session_id: impl Into<String>,
    ) -> Result<(), ChannelError> {
        self.enqueue_client_frame(GatewayClientFrame::SwitchSession(GatewaySwitchSession {
            request_id: request_id.into(),
            session_id: session_id.into(),
        }))
    }

    pub async fn apply_gateway_frame(&self, frame: &GatewayServerFrame) {
        let mut state = self.state.lock().await;
        match frame {
            GatewayServerFrame::HelloAck(ack) => {
                state.connected = true;
                state.session_id = Some(ack.session.session_id.clone());
                state.last_error = None;
                if let Some(turn) = &ack.active_turn {
                    state.mark_active_turn(turn);
                } else {
                    state.clear_active_turn();
                }
            }
            GatewayServerFrame::TurnStarted(started) => {
                state.connected = true;
                state.session_id = Some(started.session.session_id.clone());
                state.mark_active_turn(&started.turn);
                state.rendered_output.clear();
                state.last_error = None;
                state.activity_status = None;
            }
            GatewayServerFrame::AssistantDelta(delta) => {
                state.connected = true;
                state.session_id = Some(delta.session.session_id.clone());
                state.mark_active_turn(&delta.turn);
                state.rendered_output.push_str(&delta.delta);
                state.last_error = None;
            }
            GatewayServerFrame::TurnCompleted(completed) => {
                state.connected = true;
                state.session_id = Some(completed.session.session_id.clone());
                state.clear_active_turn();
                if state.rendered_output.is_empty()
                    && let Some(content) = completed.response.message.content.as_deref()
                {
                    state.rendered_output.push_str(content);
                }
                state.prompt_buffer.clear();
                state.last_error = None;
                state.activity_status = None;
            }
            GatewayServerFrame::TurnCancelled(cancelled) => {
                state.connected = true;
                state.session_id = Some(cancelled.session.session_id.clone());
                state.clear_active_turn();
                state.last_error = None;
                state.activity_status = None;
            }
            GatewayServerFrame::Error(error) => {
                state.connected = true;
                if let Some(session) = &error.session {
                    state.session_id = Some(session.session_id.clone());
                }
                if let Some(turn) = &error.turn {
                    if turn.state == GatewayTurnState::Running {
                        state.mark_active_turn(turn);
                    } else {
                        state.clear_active_turn();
                    }
                } else {
                    state.clear_active_turn();
                }
                state.last_error = Some(error.message.clone());
                state.activity_status = None;
            }
            GatewayServerFrame::HealthStatus(status) => {
                state.connected = status.healthy;
                if let Some(session) = &status.session {
                    state.session_id = Some(session.session_id.clone());
                }
                if let Some(turn) = &status.active_turn {
                    state.mark_active_turn(turn);
                } else if status.active_turn.is_none() {
                    state.clear_active_turn();
                }
                state.last_error = if status.healthy {
                    None
                } else {
                    status
                        .message
                        .clone()
                        .or_else(|| Some("gateway unhealthy".to_owned()))
                };
            }
            GatewayServerFrame::TurnProgress(GatewayTurnProgress {
                session,
                turn,
                progress,
                ..
            }) => {
                state.connected = true;
                state.session_id = Some(session.session_id.clone());
                state.mark_active_turn(turn);
                state.activity_status = Some(progress.message.clone());
            }
            GatewayServerFrame::ScheduledNotification(notification) => {
                // Scheduled notifications are informational — show the message
                // but don't alter turn state.
                let label = notification
                    .schedule_name
                    .as_deref()
                    .unwrap_or(&notification.schedule_id);
                state.last_scheduled_notification =
                    Some(format!("[Scheduled: {label}] {}", notification.message));
            }
            GatewayServerFrame::SessionCreated(created) => {
                state.connected = true;
                state.session_id = Some(created.session.session_id.clone());
                state.last_error = None;
                // Clear active turn since we just switched to a new session.
                state.clear_active_turn();
                state.activity_status = None;
            }
            GatewayServerFrame::SessionList(_) => {
                // Session list doesn't alter protocol state — it's displayed
                // by the view model in message history.
            }
            GatewayServerFrame::SessionSwitched(switched) => {
                state.connected = true;
                state.session_id = Some(switched.session.session_id.clone());
                state.last_error = None;
                state.rendered_output.clear();
                state.activity_status = None;
                if let Some(turn) = &switched.active_turn {
                    state.mark_active_turn(turn);
                } else {
                    state.clear_active_turn();
                }
            }
            GatewayServerFrame::MediaAttachment(_) => {
                // Media attachments are handled by rich channel adapters
                // (Telegram, etc.). The TUI ignores them — the text response
                // from the agent already describes what was sent.
            }
        }
    }
}

#[async_trait]
impl Channel for TuiChannelAdapter {
    async fn send(&self, event: ChannelOutboundEvent) -> Result<(), ChannelError> {
        if event.channel_id != TUI_CHANNEL_ID {
            return Err(ChannelError::Protocol {
                channel: TUI_CHANNEL_ID.to_owned(),
                message: format!("unexpected outbound channel id `{}`", event.channel_id),
            });
        }
        if event.connection_id != self.connection_id {
            return Err(ChannelError::Protocol {
                channel: TUI_CHANNEL_ID.to_owned(),
                message: format!(
                    "unexpected outbound connection id `{}`",
                    event.connection_id
                ),
            });
        }
        self.apply_gateway_frame(&event.frame).await;
        Ok(())
    }

    async fn listen(&self, buffer_size: usize) -> Result<ChannelListenStream, ChannelError> {
        let (sender, receiver) = mpsc::channel(buffer_size.max(1));
        let mut subscription = self.inbound_sender.subscribe();
        tokio::spawn(async move {
            loop {
                match subscription.recv().await {
                    Ok(event) => {
                        if sender.send(event).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        Ok(receiver)
    }

    async fn health_check(&self) -> Result<ChannelHealthStatus, ChannelError> {
        let state = self.state.lock().await;
        Ok(ChannelHealthStatus {
            healthy: state.connected && state.last_error.is_none(),
            message: state.last_error.clone().or_else(|| {
                Some(
                    if state.connected {
                        "connected"
                    } else {
                        "disconnected"
                    }
                    .to_owned(),
                )
            }),
        })
    }
}

type GatewaySocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub struct GatewayWebSocketClient {
    endpoint: String,
    socket: Mutex<GatewaySocket>,
}

impl GatewayWebSocketClient {
    pub async fn connect(endpoint: impl Into<String>) -> Result<Self, ChannelError> {
        let endpoint = endpoint.into();
        let (socket, _) =
            connect_async(&endpoint)
                .await
                .map_err(|error| ChannelError::Transport {
                    channel: TUI_CHANNEL_ID.to_owned(),
                    message: format!("failed to connect websocket endpoint `{endpoint}`: {error}"),
                })?;
        Ok(Self {
            endpoint,
            socket: Mutex::new(socket),
        })
    }

    pub async fn send_frame(&self, frame: &GatewayClientFrame) -> Result<(), ChannelError> {
        let payload = encode_gateway_client_frame(frame)?;
        self.socket
            .lock()
            .await
            .send(WsMessage::Text(payload.into()))
            .await
            .map_err(|error| ChannelError::Transport {
                channel: TUI_CHANNEL_ID.to_owned(),
                message: format!("failed to send websocket frame: {error}"),
            })
    }

    pub async fn receive_frame(&self) -> Result<GatewayServerFrame, ChannelError> {
        let mut socket = self.socket.lock().await;
        loop {
            let Some(message) = socket.next().await else {
                return Err(ChannelError::Unavailable {
                    channel: TUI_CHANNEL_ID.to_owned(),
                });
            };

            match message {
                Ok(WsMessage::Text(payload)) => {
                    return decode_gateway_server_frame(payload.as_ref());
                }
                Ok(WsMessage::Binary(payload)) => {
                    return serde_json::from_slice::<GatewayServerFrame>(&payload).map_err(
                        |error| ChannelError::Protocol {
                            channel: TUI_CHANNEL_ID.to_owned(),
                            message: format!(
                                "failed to decode gateway server frame from binary payload: {error}"
                            ),
                        },
                    );
                }
                Ok(WsMessage::Ping(payload)) => {
                    socket
                        .send(WsMessage::Pong(payload))
                        .await
                        .map_err(|error| ChannelError::Transport {
                            channel: TUI_CHANNEL_ID.to_owned(),
                            message: format!("failed to send websocket pong: {error}"),
                        })?;
                }
                Ok(WsMessage::Pong(_)) => {}
                Ok(WsMessage::Close(_)) => {
                    return Err(ChannelError::Unavailable {
                        channel: TUI_CHANNEL_ID.to_owned(),
                    });
                }
                Ok(_) => {
                    return Err(ChannelError::Protocol {
                        channel: TUI_CHANNEL_ID.to_owned(),
                        message: "unsupported websocket message type".to_owned(),
                    });
                }
                Err(error) => {
                    return Err(ChannelError::Transport {
                        channel: TUI_CHANNEL_ID.to_owned(),
                        message: format!(
                            "failed to receive websocket frame from `{}`: {error}",
                            self.endpoint
                        ),
                    });
                }
            }
        }
    }

    pub async fn health_check(&self, request_id: impl Into<String>) -> Result<bool, ChannelError> {
        self.send_frame(&GatewayClientFrame::HealthCheck(GatewayHealthCheck {
            request_id: request_id.into(),
        }))
        .await?;
        loop {
            match self.receive_frame().await? {
                GatewayServerFrame::HealthStatus(status) => return Ok(status.healthy),
                GatewayServerFrame::Error(error) => {
                    return Err(ChannelError::Protocol {
                        channel: TUI_CHANNEL_ID.to_owned(),
                        message: error.message,
                    });
                }
                _ => {}
            }
        }
    }
}

pub fn encode_gateway_client_frame(frame: &GatewayClientFrame) -> Result<String, ChannelError> {
    serde_json::to_string(frame).map_err(|error| ChannelError::Protocol {
        channel: TUI_CHANNEL_ID.to_owned(),
        message: format!("failed to encode gateway client frame: {error}"),
    })
}

pub fn decode_gateway_server_frame(payload: &str) -> Result<GatewayServerFrame, ChannelError> {
    serde_json::from_str(payload).map_err(|error| ChannelError::Protocol {
        channel: TUI_CHANNEL_ID.to_owned(),
        message: format!("failed to decode gateway server frame: {error}"),
    })
}

#[cfg(test)]
mod tests {
    use types::{
        Channel, ChannelOutboundEvent, GatewayAssistantDelta, GatewayHelloAck, GatewaySendTurn,
        GatewaySession, GatewayTurnCompleted, GatewayTurnStarted, Message, MessageRole, Response,
    };

    use super::*;

    fn assert_channel_impl<T: Channel>() {}

    fn hello_ack(session_id: &str, active_turn: Option<GatewayTurnStatus>) -> GatewayServerFrame {
        GatewayServerFrame::HelloAck(GatewayHelloAck {
            request_id: "req-hello".to_owned(),
            protocol_version: GATEWAY_PROTOCOL_VERSION,
            session: GatewaySession {
                user_id: "alice".to_owned(),
                session_id: session_id.to_owned(),
            },
            active_turn,
        })
    }

    fn turn_started(turn_id: &str, session_id: &str) -> GatewayServerFrame {
        GatewayServerFrame::TurnStarted(GatewayTurnStarted {
            request_id: "req-turn".to_owned(),
            session: GatewaySession {
                user_id: "alice".to_owned(),
                session_id: session_id.to_owned(),
            },
            turn: GatewayTurnStatus {
                turn_id: turn_id.to_owned(),
                state: GatewayTurnState::Running,
            },
        })
    }

    fn assistant_delta(turn_id: &str, session_id: &str, delta: &str) -> GatewayServerFrame {
        GatewayServerFrame::AssistantDelta(GatewayAssistantDelta {
            request_id: "req-turn".to_owned(),
            session: GatewaySession {
                user_id: "alice".to_owned(),
                session_id: session_id.to_owned(),
            },
            turn: GatewayTurnStatus {
                turn_id: turn_id.to_owned(),
                state: GatewayTurnState::Running,
            },
            delta: delta.to_owned(),
        })
    }

    fn turn_completed(turn_id: &str, session_id: &str, message: &str) -> GatewayServerFrame {
        GatewayServerFrame::TurnCompleted(GatewayTurnCompleted {
            request_id: "req-turn".to_owned(),
            session: GatewaySession {
                user_id: "alice".to_owned(),
                session_id: session_id.to_owned(),
            },
            turn: GatewayTurnStatus {
                turn_id: turn_id.to_owned(),
                state: GatewayTurnState::Completed,
            },
            response: Response {
                message: Message {
                    role: MessageRole::Assistant,
                    content: Some(message.to_owned()),
                    tool_calls: Vec::new(),
                    tool_call_id: None,
                    attachments: Vec::new(),
                },
                tool_calls: Vec::new(),
                finish_reason: Some("stop".to_owned()),
                usage: None,
            },
        })
    }

    #[test]
    fn adapter_implements_channel_trait() {
        assert_channel_impl::<TuiChannelAdapter>();
    }

    #[tokio::test]
    async fn frame_codec_and_state_transitions_work() {
        let adapter = TuiChannelAdapter::new("alice", "conn-1");
        let encoded = encode_gateway_client_frame(&GatewayClientFrame::SendTurn(GatewaySendTurn {
            request_id: "req-turn".to_owned(),
            session_id: "runtime-alice".to_owned(),
            turn_id: "turn-1".to_owned(),
            prompt: "hello".to_owned(),
            attachments: Vec::new(),
        }))
        .expect("client frame should encode");
        assert!(encoded.contains("\"send_turn\""));

        let hello_payload =
            serde_json::to_string(&hello_ack("runtime-alice", None)).expect("hello_ack encodes");
        let decoded = decode_gateway_server_frame(&hello_payload).expect("hello_ack decodes");
        adapter.apply_gateway_frame(&decoded).await;

        adapter
            .send(ChannelOutboundEvent {
                channel_id: "tui".to_owned(),
                connection_id: "conn-1".to_owned(),
                frame: turn_started("turn-1", "runtime-alice"),
            })
            .await
            .expect("turn_started should update state");
        adapter
            .send(ChannelOutboundEvent {
                channel_id: "tui".to_owned(),
                connection_id: "conn-1".to_owned(),
                frame: assistant_delta("turn-1", "runtime-alice", "hel"),
            })
            .await
            .expect("assistant_delta should update state");
        adapter
            .send(ChannelOutboundEvent {
                channel_id: "tui".to_owned(),
                connection_id: "conn-1".to_owned(),
                frame: assistant_delta("turn-1", "runtime-alice", "lo"),
            })
            .await
            .expect("assistant_delta should update state");
        adapter
            .send(ChannelOutboundEvent {
                channel_id: "tui".to_owned(),
                connection_id: "conn-1".to_owned(),
                frame: turn_completed("turn-1", "runtime-alice", "hello"),
            })
            .await
            .expect("turn_completed should update state");

        let state = adapter.state_snapshot().await;
        assert!(state.connected);
        assert_eq!(state.session_id.as_deref(), Some("runtime-alice"));
        assert_eq!(state.active_turn_id, None);
        assert_eq!(state.rendered_output, "hello");
        assert!(state.last_error.is_none());
    }

    #[tokio::test]
    async fn ctrl_c_cancels_active_turn_or_exits_when_idle() {
        let adapter = TuiChannelAdapter::new("alice", "conn-1");
        adapter
            .apply_gateway_frame(&hello_ack("runtime-alice", None))
            .await;
        adapter
            .apply_gateway_frame(&turn_started("turn-1", "runtime-alice"))
            .await;

        let mut inbound = adapter
            .listen(8)
            .await
            .expect("listen should return stream");
        let outcome = adapter
            .handle_ctrl_c("req-cancel")
            .await
            .expect("ctrl+c should produce cancel frame");
        assert_eq!(outcome, TuiCtrlCOutcome::CancelActiveTurn);

        let event = inbound
            .recv()
            .await
            .expect("cancel event should be available")
            .expect("cancel event should be valid");
        match event.frame {
            GatewayClientFrame::CancelActiveTurn(cancel) => {
                assert_eq!(cancel.request_id, "req-cancel");
                assert_eq!(cancel.session_id, "runtime-alice");
                assert_eq!(cancel.turn_id, "turn-1");
            }
            other => panic!("expected cancel frame, got {other:?}"),
        }

        adapter
            .apply_gateway_frame(&GatewayServerFrame::TurnCancelled(
                types::GatewayTurnCancelled {
                    request_id: "req-cancel".to_owned(),
                    session: GatewaySession {
                        user_id: "alice".to_owned(),
                        session_id: "runtime-alice".to_owned(),
                    },
                    turn: GatewayTurnStatus {
                        turn_id: "turn-1".to_owned(),
                        state: GatewayTurnState::Cancelled,
                    },
                },
            ))
            .await;
        let idle_outcome = adapter
            .handle_ctrl_c("req-exit")
            .await
            .expect("idle ctrl+c should request exit");
        assert_eq!(idle_outcome, TuiCtrlCOutcome::Exit);
    }

    #[tokio::test]
    async fn reconnect_hello_reuses_runtime_session_and_resumes_streaming() {
        let adapter = TuiChannelAdapter::new("alice", "conn-1");
        adapter
            .apply_gateway_frame(&hello_ack("runtime-alice", None))
            .await;
        adapter
            .apply_gateway_frame(&turn_started("turn-1", "runtime-alice"))
            .await;
        adapter
            .apply_gateway_frame(&assistant_delta("turn-1", "runtime-alice", "first"))
            .await;
        adapter.mark_disconnected().await;

        let reconnect_hello = adapter.build_hello_frame("req-hello-2").await;
        match reconnect_hello {
            GatewayClientFrame::Hello(hello) => {
                assert_eq!(hello.session_id.as_deref(), Some("runtime-alice"));
            }
            other => panic!("expected hello frame, got {other:?}"),
        }

        adapter
            .apply_gateway_frame(&hello_ack(
                "runtime-alice",
                Some(GatewayTurnStatus {
                    turn_id: "turn-1".to_owned(),
                    state: GatewayTurnState::Running,
                }),
            ))
            .await;
        adapter
            .apply_gateway_frame(&assistant_delta("turn-1", "runtime-alice", "second"))
            .await;
        adapter
            .apply_gateway_frame(&turn_completed("turn-1", "runtime-alice", "done"))
            .await;

        let state = adapter.state_snapshot().await;
        assert_eq!(state.active_turn_id, None);
        assert_eq!(state.session_id.as_deref(), Some("runtime-alice"));
        assert_eq!(state.rendered_output, "firstsecond");
    }

    #[tokio::test]
    async fn turn_progress_sets_activity_status_and_clears_on_completion() {
        use types::{GatewayTurnProgress, RuntimeProgressEvent, RuntimeProgressKind};

        let adapter = TuiChannelAdapter::new("alice", "conn-1");
        adapter
            .apply_gateway_frame(&hello_ack("runtime-alice", None))
            .await;
        adapter
            .apply_gateway_frame(&turn_started("turn-1", "runtime-alice"))
            .await;

        // Activity status should be None at turn start.
        let state = adapter.state_snapshot().await;
        assert_eq!(state.activity_status, None);

        // Receive a progress event.
        adapter
            .apply_gateway_frame(&GatewayServerFrame::TurnProgress(GatewayTurnProgress {
                request_id: "req-turn".to_owned(),
                session: GatewaySession {
                    user_id: "alice".to_owned(),
                    session_id: "runtime-alice".to_owned(),
                },
                turn: GatewayTurnStatus {
                    turn_id: "turn-1".to_owned(),
                    state: GatewayTurnState::Running,
                },
                progress: RuntimeProgressEvent {
                    kind: RuntimeProgressKind::ProviderCall,
                    message: "[1/8] Calling provider".to_owned(),
                    turn: 1,
                    max_turns: 8,
                },
            }))
            .await;

        let state = adapter.state_snapshot().await;
        assert_eq!(
            state.activity_status.as_deref(),
            Some("[1/8] Calling provider")
        );
        assert_eq!(state.active_turn_id.as_deref(), Some("turn-1"));

        // Activity status clears when the turn completes.
        adapter
            .apply_gateway_frame(&turn_completed("turn-1", "runtime-alice", "done"))
            .await;
        let state = adapter.state_snapshot().await;
        assert_eq!(state.activity_status, None);
        assert_eq!(state.active_turn_id, None);
    }
}
