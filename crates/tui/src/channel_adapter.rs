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
    GatewayClientHello, GatewayHealthCheck, GatewayServerFrame, GatewayTurnState,
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
    pub runtime_session_id: Option<String>,
    pub active_turn_id: Option<String>,
    pub prompt_buffer: String,
    pub rendered_output: String,
    pub last_error: Option<String>,
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
}

impl TuiChannelAdapter {
    pub fn new(user_id: impl Into<String>, connection_id: impl Into<String>) -> Self {
        let (inbound_sender, _) = broadcast::channel(EVENT_BUFFER_CAPACITY);
        Self {
            user_id: user_id.into(),
            connection_id: connection_id.into(),
            inbound_sender,
            state: Arc::new(Mutex::new(TuiUiState::default())),
        }
    }

    pub async fn state_snapshot(&self) -> TuiUiState {
        self.state.lock().await.clone()
    }

    pub async fn mark_disconnected(&self) {
        self.state.lock().await.connected = false;
    }

    pub async fn build_hello_frame(&self, request_id: impl Into<String>) -> GatewayClientFrame {
        let runtime_session_id = self.state.lock().await.runtime_session_id.clone();
        GatewayClientFrame::Hello(GatewayClientHello {
            request_id: request_id.into(),
            protocol_version: GATEWAY_PROTOCOL_VERSION,
            user_id: self.user_id.clone(),
            runtime_session_id,
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
        let runtime_session_id = {
            let mut state = self.state.lock().await;
            state.prompt_buffer = prompt.clone();
            state
                .runtime_session_id
                .clone()
                .ok_or_else(|| ChannelError::Unavailable {
                    channel: TUI_CHANNEL_ID.to_owned(),
                })?
        };

        self.enqueue_client_frame(GatewayClientFrame::SendTurn(types::GatewaySendTurn {
            request_id,
            runtime_session_id,
            turn_id,
            prompt,
        }))
    }

    pub async fn handle_ctrl_c(
        &self,
        request_id: impl Into<String>,
    ) -> Result<TuiCtrlCOutcome, ChannelError> {
        let request_id = request_id.into();
        let (runtime_session_id, turn_id) = {
            let state = self.state.lock().await;
            match (&state.runtime_session_id, &state.active_turn_id) {
                (Some(runtime_session_id), Some(turn_id)) => {
                    (runtime_session_id.clone(), turn_id.clone())
                }
                _ => return Ok(TuiCtrlCOutcome::Exit),
            }
        };

        self.enqueue_client_frame(GatewayClientFrame::CancelActiveTurn(
            GatewayCancelActiveTurn {
                request_id,
                runtime_session_id,
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

    pub async fn apply_gateway_frame(&self, frame: &GatewayServerFrame) {
        let mut state = self.state.lock().await;
        match frame {
            GatewayServerFrame::HelloAck(ack) => {
                state.connected = true;
                state.runtime_session_id = Some(ack.session.runtime_session_id.clone());
                state.last_error = None;
                if let Some(turn) = &ack.active_turn {
                    state.mark_active_turn(turn);
                } else {
                    state.clear_active_turn();
                }
            }
            GatewayServerFrame::TurnStarted(started) => {
                state.connected = true;
                state.runtime_session_id = Some(started.session.runtime_session_id.clone());
                state.mark_active_turn(&started.turn);
                state.rendered_output.clear();
                state.last_error = None;
            }
            GatewayServerFrame::AssistantDelta(delta) => {
                state.connected = true;
                state.runtime_session_id = Some(delta.session.runtime_session_id.clone());
                state.mark_active_turn(&delta.turn);
                state.rendered_output.push_str(&delta.delta);
                state.last_error = None;
            }
            GatewayServerFrame::TurnCompleted(completed) => {
                state.connected = true;
                state.runtime_session_id = Some(completed.session.runtime_session_id.clone());
                state.clear_active_turn();
                if state.rendered_output.is_empty()
                    && let Some(content) = completed.response.message.content.as_deref()
                {
                    state.rendered_output.push_str(content);
                }
                state.prompt_buffer.clear();
                state.last_error = None;
            }
            GatewayServerFrame::TurnCancelled(cancelled) => {
                state.connected = true;
                state.runtime_session_id = Some(cancelled.session.runtime_session_id.clone());
                state.clear_active_turn();
                state.last_error = None;
            }
            GatewayServerFrame::Error(error) => {
                state.connected = true;
                if let Some(session) = &error.session {
                    state.runtime_session_id = Some(session.runtime_session_id.clone());
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
            }
            GatewayServerFrame::HealthStatus(status) => {
                state.connected = status.healthy;
                if let Some(session) = &status.session {
                    state.runtime_session_id = Some(session.runtime_session_id.clone());
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

    fn hello_ack(
        runtime_session_id: &str,
        active_turn: Option<GatewayTurnStatus>,
    ) -> GatewayServerFrame {
        GatewayServerFrame::HelloAck(GatewayHelloAck {
            request_id: "req-hello".to_owned(),
            protocol_version: GATEWAY_PROTOCOL_VERSION,
            session: GatewaySession {
                user_id: "alice".to_owned(),
                runtime_session_id: runtime_session_id.to_owned(),
            },
            active_turn,
        })
    }

    fn turn_started(turn_id: &str, runtime_session_id: &str) -> GatewayServerFrame {
        GatewayServerFrame::TurnStarted(GatewayTurnStarted {
            request_id: "req-turn".to_owned(),
            session: GatewaySession {
                user_id: "alice".to_owned(),
                runtime_session_id: runtime_session_id.to_owned(),
            },
            turn: GatewayTurnStatus {
                turn_id: turn_id.to_owned(),
                state: GatewayTurnState::Running,
            },
        })
    }

    fn assistant_delta(turn_id: &str, runtime_session_id: &str, delta: &str) -> GatewayServerFrame {
        GatewayServerFrame::AssistantDelta(GatewayAssistantDelta {
            request_id: "req-turn".to_owned(),
            session: GatewaySession {
                user_id: "alice".to_owned(),
                runtime_session_id: runtime_session_id.to_owned(),
            },
            turn: GatewayTurnStatus {
                turn_id: turn_id.to_owned(),
                state: GatewayTurnState::Running,
            },
            delta: delta.to_owned(),
        })
    }

    fn turn_completed(
        turn_id: &str,
        runtime_session_id: &str,
        message: &str,
    ) -> GatewayServerFrame {
        GatewayServerFrame::TurnCompleted(GatewayTurnCompleted {
            request_id: "req-turn".to_owned(),
            session: GatewaySession {
                user_id: "alice".to_owned(),
                runtime_session_id: runtime_session_id.to_owned(),
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
            runtime_session_id: "runtime-alice".to_owned(),
            turn_id: "turn-1".to_owned(),
            prompt: "hello".to_owned(),
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
        assert_eq!(state.runtime_session_id.as_deref(), Some("runtime-alice"));
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
                assert_eq!(cancel.runtime_session_id, "runtime-alice");
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
                        runtime_session_id: "runtime-alice".to_owned(),
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
                assert_eq!(hello.runtime_session_id.as_deref(), Some("runtime-alice"));
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
        assert_eq!(state.runtime_session_id.as_deref(), Some("runtime-alice"));
        assert_eq!(state.rendered_output, "firstsecond");
    }
}
