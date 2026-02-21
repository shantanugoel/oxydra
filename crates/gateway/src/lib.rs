use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use async_trait::async_trait;
use axum::{
    Router,
    extract::{
        State,
        ws::{Message as AxumWsMessage, WebSocket, WebSocketUpgrade},
    },
    response::Response as AxumResponse,
    routing::get,
};
use futures_util::{SinkExt, StreamExt, stream::SplitSink};
use runtime::{AgentRuntime, RuntimeStreamEventSender};
use tokio::sync::{Mutex, RwLock, broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use types::{
    Context, GATEWAY_PROTOCOL_VERSION, GatewayAssistantDelta, GatewayCancelActiveTurn,
    GatewayClientFrame, GatewayClientHello, GatewayErrorFrame, GatewayHealthCheck,
    GatewayHealthStatus, GatewayHelloAck, GatewaySendTurn, GatewayServerFrame, GatewaySession,
    GatewayTurnCancelled, GatewayTurnCompleted, GatewayTurnStarted, GatewayTurnState,
    GatewayTurnStatus, Message, MessageRole, ModelId, ProviderId, Response, RuntimeError,
    StartupStatusReport, StreamItem,
};

mod turn_runner;

#[cfg(test)]
mod tests;

pub use turn_runner::{GatewayTurnRunner, RuntimeGatewayTurnRunner};

const WS_ROUTE: &str = "/ws";
const GATEWAY_CHANNEL_ID: &str = "tui";
const EVENT_BUFFER_CAPACITY: usize = 256;

pub struct GatewayServer {
    turn_runner: Arc<dyn GatewayTurnRunner>,
    startup_status: Option<StartupStatusReport>,
    sessions: RwLock<HashMap<String, Arc<UserSessionState>>>,
    next_connection_id: AtomicU64,
}

impl GatewayServer {
    pub fn new(turn_runner: Arc<dyn GatewayTurnRunner>) -> Self {
        Self::with_optional_startup_status(turn_runner, None)
    }

    pub fn with_startup_status(
        turn_runner: Arc<dyn GatewayTurnRunner>,
        startup_status: StartupStatusReport,
    ) -> Self {
        Self::with_optional_startup_status(turn_runner, Some(startup_status))
    }

    fn with_optional_startup_status(
        turn_runner: Arc<dyn GatewayTurnRunner>,
        startup_status: Option<StartupStatusReport>,
    ) -> Self {
        Self {
            turn_runner,
            startup_status,
            sessions: RwLock::new(HashMap::new()),
            next_connection_id: AtomicU64::new(1),
        }
    }

    pub fn router(self: Arc<Self>) -> Router {
        Router::new()
            .route(WS_ROUTE, get(Self::upgrade_websocket))
            .with_state(self)
    }

    async fn upgrade_websocket(
        State(server): State<Arc<Self>>,
        ws: WebSocketUpgrade,
    ) -> AxumResponse {
        ws.on_upgrade(move |socket| async move {
            server.handle_socket(socket).await;
        })
    }

    async fn handle_socket(self: Arc<Self>, socket: WebSocket) {
        let connection_id = self.next_connection_id.fetch_add(1, Ordering::Relaxed);
        let (mut sender, mut receiver) = socket.split();

        let Some(first_message) = receiver.next().await else {
            return;
        };

        let first_message = match first_message {
            Ok(message) => message,
            Err(error) => {
                tracing::debug!(%connection_id, %error, "failed to read first websocket message");
                return;
            }
        };

        let hello = match parse_client_frame(first_message) {
            Ok(GatewayClientFrame::Hello(hello)) => hello,
            Ok(_) => {
                let _ = send_error_frame(
                    &mut sender,
                    None,
                    None,
                    None,
                    "expected hello as first frame",
                )
                .await;
                return;
            }
            Err(error) => {
                let _ = send_error_frame(&mut sender, None, None, None, error).await;
                return;
            }
        };

        if hello.protocol_version != GATEWAY_PROTOCOL_VERSION {
            let _ = send_error_frame(
                &mut sender,
                Some(hello.request_id),
                None,
                None,
                format!(
                    "unsupported protocol version {}; expected {}",
                    hello.protocol_version, GATEWAY_PROTOCOL_VERSION
                ),
            )
            .await;
            return;
        }

        let session = match self.resolve_session(&hello).await {
            Ok(session) => session,
            Err(message) => {
                let _ = send_error_frame(&mut sender, Some(hello.request_id), None, None, message)
                    .await;
                return;
            }
        };

        let mut updates = session.events.subscribe();
        let active_turn = session.active_turn_status().await;
        let gateway_session = session.gateway_session();

        let hello_ack = GatewayServerFrame::HelloAck(GatewayHelloAck {
            request_id: hello.request_id,
            protocol_version: GATEWAY_PROTOCOL_VERSION,
            session: gateway_session.clone(),
            active_turn,
        });
        if send_server_frame(&mut sender, &hello_ack).await.is_err() {
            return;
        }

        if session.active_turn_status().await.is_none()
            && let Some(outcome) = session.latest_terminal_frame().await
            && send_server_frame(&mut sender, &outcome).await.is_err()
        {
            return;
        }

        loop {
            tokio::select! {
                incoming = receiver.next() => {
                    let Some(incoming) = incoming else {
                        break;
                    };
                    let incoming = match incoming {
                        Ok(message) => message,
                        Err(error) => {
                            tracing::debug!(%connection_id, %error, "websocket receive failed");
                            break;
                        }
                    };

                    match incoming {
                        AxumWsMessage::Close(_) => break,
                        AxumWsMessage::Ping(payload) => {
                            if sender.send(AxumWsMessage::Pong(payload)).await.is_err() {
                                break;
                            }
                        }
                        AxumWsMessage::Pong(_) => {}
                        message @ (AxumWsMessage::Text(_) | AxumWsMessage::Binary(_)) => {
                            let frame = match parse_client_frame(message) {
                                Ok(frame) => frame,
                                Err(error) => {
                                    if send_error_frame(&mut sender, None, Some(gateway_session.clone()), None, error).await.is_err() {
                                        break;
                                    }
                                    continue;
                                }
                            };

                            if self
                                .handle_client_frame(
                                    frame,
                                    Arc::clone(&session),
                                    gateway_session.clone(),
                                    &mut sender,
                                )
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                }
                update = updates.recv() => {
                    match update {
                        Ok(frame) => {
                            if send_server_frame(&mut sender, &frame).await.is_err() {
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            tracing::debug!(%connection_id, "gateway subscriber lagged; dropping stale updates");
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        }
    }

    async fn handle_client_frame(
        &self,
        frame: GatewayClientFrame,
        session: Arc<UserSessionState>,
        gateway_session: GatewaySession,
        sender: &mut SplitSink<WebSocket, AxumWsMessage>,
    ) -> Result<(), ()> {
        match frame {
            GatewayClientFrame::Hello(_) => {
                send_error_frame(
                    sender,
                    None,
                    Some(gateway_session),
                    None,
                    "hello already negotiated for this websocket connection",
                )
                .await
            }
            GatewayClientFrame::SendTurn(send_turn) => {
                if let Some(error_frame) = self.start_turn(session, send_turn).await {
                    send_server_frame(sender, &error_frame).await
                } else {
                    Ok(())
                }
            }
            GatewayClientFrame::CancelActiveTurn(cancel_turn) => {
                if let Some(error_frame) = self.cancel_turn(session, cancel_turn).await {
                    send_server_frame(sender, &error_frame).await
                } else {
                    Ok(())
                }
            }
            GatewayClientFrame::HealthCheck(health_check) => {
                let frame = self.health_status(session, health_check).await;
                send_server_frame(sender, &frame).await
            }
        }
    }

    async fn resolve_session(
        &self,
        hello: &GatewayClientHello,
    ) -> Result<Arc<UserSessionState>, String> {
        let mut sessions = self.sessions.write().await;
        if let Some(existing) = sessions.get(&hello.user_id) {
            if let Some(runtime_session_id) = &hello.runtime_session_id
                && runtime_session_id != &existing.runtime_session_id
            {
                return Err(format!(
                    "runtime_session_id mismatch for user `{}`",
                    hello.user_id
                ));
            }
            return Ok(Arc::clone(existing));
        }

        let runtime_session_id = hello
            .runtime_session_id
            .clone()
            .unwrap_or_else(|| default_runtime_session_id(&hello.user_id));
        let session = Arc::new(UserSessionState::new(
            hello.user_id.clone(),
            runtime_session_id,
        ));
        sessions.insert(hello.user_id.clone(), Arc::clone(&session));
        Ok(session)
    }

    async fn start_turn(
        &self,
        session: Arc<UserSessionState>,
        send_turn: GatewaySendTurn,
    ) -> Option<GatewayServerFrame> {
        if send_turn.runtime_session_id != session.runtime_session_id {
            return Some(GatewayServerFrame::Error(GatewayErrorFrame {
                request_id: Some(send_turn.request_id),
                session: Some(session.gateway_session()),
                turn: None,
                message: "runtime_session_id does not match active session".to_owned(),
            }));
        }

        let cancellation = CancellationToken::new();
        {
            let mut active_turn = session.active_turn.lock().await;
            if let Some(existing) = active_turn.as_ref() {
                return Some(GatewayServerFrame::Error(GatewayErrorFrame {
                    request_id: Some(send_turn.request_id),
                    session: Some(session.gateway_session()),
                    turn: Some(GatewayTurnStatus {
                        turn_id: existing.turn_id.clone(),
                        state: GatewayTurnState::Running,
                    }),
                    message: "an active turn is already running".to_owned(),
                }));
            }
            *active_turn = Some(ActiveTurnState {
                turn_id: send_turn.turn_id.clone(),
                cancellation: cancellation.clone(),
            });
        }
        *session.latest_terminal_frame.lock().await = None;

        let running_turn = GatewayTurnStatus {
            turn_id: send_turn.turn_id.clone(),
            state: GatewayTurnState::Running,
        };
        session.publish(GatewayServerFrame::TurnStarted(GatewayTurnStarted {
            request_id: send_turn.request_id.clone(),
            session: session.gateway_session(),
            turn: running_turn.clone(),
        }));

        let runtime = Arc::clone(&self.turn_runner);
        tokio::spawn(async move {
            let (delta_tx, mut delta_rx) = mpsc::unbounded_channel();
            let runtime_future = runtime.run_turn(
                &session.runtime_session_id,
                send_turn.prompt,
                cancellation,
                delta_tx,
            );
            tokio::pin!(runtime_future);

            loop {
                tokio::select! {
                    maybe_delta = delta_rx.recv() => {
                        if let Some(delta) = maybe_delta {
                            session.publish(GatewayServerFrame::AssistantDelta(GatewayAssistantDelta {
                                request_id: send_turn.request_id.clone(),
                                session: session.gateway_session(),
                                turn: running_turn.clone(),
                                delta,
                            }));
                        }
                    }
                    result = &mut runtime_future => {
                        let terminal_frame = match result {
                            Ok(response) => GatewayServerFrame::TurnCompleted(GatewayTurnCompleted {
                                request_id: send_turn.request_id.clone(),
                                session: session.gateway_session(),
                                turn: GatewayTurnStatus {
                                    turn_id: send_turn.turn_id.clone(),
                                    state: GatewayTurnState::Completed,
                                },
                                response,
                            }),
                            Err(RuntimeError::Cancelled) => GatewayServerFrame::TurnCancelled(
                                GatewayTurnCancelled {
                                    request_id: send_turn.request_id.clone(),
                                    session: session.gateway_session(),
                                    turn: GatewayTurnStatus {
                                        turn_id: send_turn.turn_id.clone(),
                                        state: GatewayTurnState::Cancelled,
                                    },
                                },
                            ),
                            Err(error) => GatewayServerFrame::Error(GatewayErrorFrame {
                                request_id: Some(send_turn.request_id.clone()),
                                session: Some(session.gateway_session()),
                                turn: Some(GatewayTurnStatus {
                                    turn_id: send_turn.turn_id.clone(),
                                    state: GatewayTurnState::Failed,
                                }),
                                message: error.to_string(),
                            }),
                        };

                        {
                            let mut active_turn = session.active_turn.lock().await;
                            if active_turn
                                .as_ref()
                                .is_some_and(|turn| turn.turn_id == send_turn.turn_id)
                            {
                                *active_turn = None;
                            }
                        }
                        *session.latest_terminal_frame.lock().await = Some(terminal_frame.clone());
                        session.publish(terminal_frame);
                        break;
                    }
                }
            }
        });

        None
    }

    async fn cancel_turn(
        &self,
        session: Arc<UserSessionState>,
        cancel_turn: GatewayCancelActiveTurn,
    ) -> Option<GatewayServerFrame> {
        if cancel_turn.runtime_session_id != session.runtime_session_id {
            return Some(GatewayServerFrame::Error(GatewayErrorFrame {
                request_id: Some(cancel_turn.request_id),
                session: Some(session.gateway_session()),
                turn: None,
                message: "runtime_session_id does not match active session".to_owned(),
            }));
        }

        let active_turn = session.active_turn.lock().await.clone();
        match active_turn {
            Some(active_turn) if active_turn.turn_id == cancel_turn.turn_id => {
                active_turn.cancellation.cancel();
                None
            }
            Some(active_turn) => Some(GatewayServerFrame::Error(GatewayErrorFrame {
                request_id: Some(cancel_turn.request_id),
                session: Some(session.gateway_session()),
                turn: Some(GatewayTurnStatus {
                    turn_id: active_turn.turn_id,
                    state: GatewayTurnState::Running,
                }),
                message: "cancel request does not match active turn".to_owned(),
            })),
            None => Some(GatewayServerFrame::Error(GatewayErrorFrame {
                request_id: Some(cancel_turn.request_id),
                session: Some(session.gateway_session()),
                turn: None,
                message: "no active turn to cancel".to_owned(),
            })),
        }
    }

    async fn health_status(
        &self,
        session: Arc<UserSessionState>,
        health_check: GatewayHealthCheck,
    ) -> GatewayServerFrame {
        let startup_status = self.startup_status.clone();
        let message = startup_status
            .as_ref()
            .filter(|status| status.is_degraded())
            .map(|status| {
                format!(
                    "{GATEWAY_CHANNEL_ID} gateway ready with degraded startup: {}",
                    status
                        .degraded_reasons
                        .iter()
                        .map(|reason| reason.detail.as_str())
                        .collect::<Vec<_>>()
                        .join(" | ")
                )
            })
            .unwrap_or_else(|| format!("{GATEWAY_CHANNEL_ID} gateway ready"));
        tracing::info!(
            user_id = %session.user_id,
            runtime_session_id = %session.runtime_session_id,
            startup_degraded = startup_status.as_ref().is_some_and(StartupStatusReport::is_degraded),
            "gateway health check handled"
        );
        GatewayServerFrame::HealthStatus(GatewayHealthStatus {
            request_id: health_check.request_id,
            healthy: true,
            session: Some(session.gateway_session()),
            active_turn: session.active_turn_status().await,
            startup_status,
            message: Some(message),
        })
    }
}

#[derive(Clone)]
struct ActiveTurnState {
    turn_id: String,
    cancellation: CancellationToken,
}

struct UserSessionState {
    user_id: String,
    runtime_session_id: String,
    events: broadcast::Sender<GatewayServerFrame>,
    active_turn: Mutex<Option<ActiveTurnState>>,
    latest_terminal_frame: Mutex<Option<GatewayServerFrame>>,
}

impl UserSessionState {
    fn new(user_id: String, runtime_session_id: String) -> Self {
        let (events, _) = broadcast::channel(EVENT_BUFFER_CAPACITY);
        Self {
            user_id,
            runtime_session_id,
            events,
            active_turn: Mutex::new(None),
            latest_terminal_frame: Mutex::new(None),
        }
    }

    fn gateway_session(&self) -> GatewaySession {
        GatewaySession {
            user_id: self.user_id.clone(),
            runtime_session_id: self.runtime_session_id.clone(),
        }
    }

    async fn active_turn_status(&self) -> Option<GatewayTurnStatus> {
        self.active_turn
            .lock()
            .await
            .as_ref()
            .map(|active_turn| GatewayTurnStatus {
                turn_id: active_turn.turn_id.clone(),
                state: GatewayTurnState::Running,
            })
    }

    async fn latest_terminal_frame(&self) -> Option<GatewayServerFrame> {
        self.latest_terminal_frame.lock().await.clone()
    }

    fn publish(&self, frame: GatewayServerFrame) {
        let _ = self.events.send(frame);
    }
}

fn default_runtime_session_id(user_id: &str) -> String {
    let normalized = user_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>();
    format!("runtime-{normalized}")
}

fn parse_client_frame(message: AxumWsMessage) -> Result<GatewayClientFrame, String> {
    match message {
        AxumWsMessage::Text(payload) => {
            serde_json::from_str::<GatewayClientFrame>(payload.as_ref()).map_err(|error| {
                format!("failed to decode client frame from websocket text payload: {error}")
            })
        }
        AxumWsMessage::Binary(payload) => serde_json::from_slice::<GatewayClientFrame>(&payload)
            .map_err(|error| {
                format!("failed to decode client frame from websocket binary payload: {error}")
            }),
        _ => Err("unsupported websocket message type".to_owned()),
    }
}

async fn send_server_frame(
    sender: &mut SplitSink<WebSocket, AxumWsMessage>,
    frame: &GatewayServerFrame,
) -> Result<(), ()> {
    let payload = serde_json::to_string(frame).map_err(|error| {
        tracing::error!(%error, "failed to encode gateway server frame");
    })?;
    sender
        .send(AxumWsMessage::Text(payload.into()))
        .await
        .map_err(|error| {
            tracing::debug!(%error, "failed to send gateway websocket frame");
        })
}

async fn send_error_frame(
    sender: &mut SplitSink<WebSocket, AxumWsMessage>,
    request_id: Option<String>,
    session: Option<GatewaySession>,
    turn: Option<GatewayTurnStatus>,
    message: impl Into<String>,
) -> Result<(), ()> {
    send_server_frame(
        sender,
        &GatewayServerFrame::Error(GatewayErrorFrame {
            request_id,
            session,
            turn,
            message: message.into(),
        }),
    )
    .await
}
