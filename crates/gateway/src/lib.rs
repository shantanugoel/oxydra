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
    StreamItem,
};

const WS_ROUTE: &str = "/ws";
const GATEWAY_CHANNEL_ID: &str = "tui";
const EVENT_BUFFER_CAPACITY: usize = 256;

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

pub struct GatewayServer {
    turn_runner: Arc<dyn GatewayTurnRunner>,
    sessions: RwLock<HashMap<String, Arc<UserSessionState>>>,
    next_connection_id: AtomicU64,
}

impl GatewayServer {
    pub fn new(turn_runner: Arc<dyn GatewayTurnRunner>) -> Self {
        Self {
            turn_runner,
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
        GatewayServerFrame::HealthStatus(GatewayHealthStatus {
            request_id: health_check.request_id,
            healthy: true,
            session: Some(session.gateway_session()),
            active_turn: session.active_turn_status().await,
            message: Some(format!("{GATEWAY_CHANNEL_ID} gateway ready")),
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

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, net::SocketAddr, time::Duration};

    use super::*;
    use tokio::net::TcpListener;
    use tokio::time::timeout;
    use tokio_tungstenite::{
        MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message as WsMessage,
    };
    use types::ProviderError;
    use url::Url;

    type ClientSocket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

    #[derive(Clone)]
    struct ScriptedTurnRunner {
        scripted_turns: Arc<Mutex<VecDeque<ScriptedTurn>>>,
        recorded_calls: Arc<Mutex<Vec<(String, String)>>>,
    }

    impl ScriptedTurnRunner {
        fn new(scripted_turns: Vec<ScriptedTurn>) -> Self {
            Self {
                scripted_turns: Arc::new(Mutex::new(scripted_turns.into())),
                recorded_calls: Arc::new(Mutex::new(Vec::new())),
            }
        }

        async fn recorded_calls(&self) -> Vec<(String, String)> {
            self.recorded_calls.lock().await.clone()
        }
    }

    #[async_trait]
    impl GatewayTurnRunner for ScriptedTurnRunner {
        async fn run_turn(
            &self,
            runtime_session_id: &str,
            prompt: String,
            cancellation: CancellationToken,
            delta_sender: mpsc::UnboundedSender<String>,
        ) -> Result<Response, RuntimeError> {
            self.recorded_calls
                .lock()
                .await
                .push((runtime_session_id.to_owned(), prompt));

            let scripted_turn = self
                .scripted_turns
                .lock()
                .await
                .pop_front()
                .expect("test turn runner expected another scripted turn");

            for (delay, delta) in scripted_turn.deltas {
                tokio::select! {
                    _ = cancellation.cancelled() => return Err(RuntimeError::Cancelled),
                    _ = tokio::time::sleep(delay) => {
                        let _ = delta_sender.send(delta);
                    }
                }
            }

            if scripted_turn.wait_for_cancellation {
                cancellation.cancelled().await;
                return Err(RuntimeError::Cancelled);
            }

            if cancellation.is_cancelled() {
                return Err(RuntimeError::Cancelled);
            }

            tokio::time::sleep(scripted_turn.completion_delay).await;

            match scripted_turn.result {
                ScriptedTurnResult::Complete(content) => Ok(Response {
                    message: Message {
                        role: MessageRole::Assistant,
                        content: Some(content),
                        tool_calls: Vec::new(),
                        tool_call_id: None,
                    },
                    tool_calls: Vec::new(),
                    finish_reason: Some("stop".to_owned()),
                    usage: None,
                }),
                ScriptedTurnResult::Fail(message) => {
                    Err(RuntimeError::Provider(ProviderError::RequestFailed {
                        provider: ProviderId::from("test"),
                        message,
                    }))
                }
            }
        }
    }

    struct ScriptedTurn {
        deltas: Vec<(Duration, String)>,
        wait_for_cancellation: bool,
        completion_delay: Duration,
        result: ScriptedTurnResult,
    }

    enum ScriptedTurnResult {
        Complete(String),
        Fail(String),
    }

    impl ScriptedTurn {
        fn completed(deltas: Vec<(Duration, &str)>, completion: &str) -> Self {
            Self {
                deltas: deltas
                    .into_iter()
                    .map(|(delay, delta)| (delay, delta.to_owned()))
                    .collect(),
                wait_for_cancellation: false,
                completion_delay: Duration::ZERO,
                result: ScriptedTurnResult::Complete(completion.to_owned()),
            }
        }

        fn cancellation_aware(deltas: Vec<(Duration, &str)>) -> Self {
            Self {
                deltas: deltas
                    .into_iter()
                    .map(|(delay, delta)| (delay, delta.to_owned()))
                    .collect(),
                wait_for_cancellation: true,
                completion_delay: Duration::ZERO,
                result: ScriptedTurnResult::Complete("unused".to_owned()),
            }
        }

        fn completed_with_delay(
            deltas: Vec<(Duration, &str)>,
            completion: &str,
            delay: Duration,
        ) -> Self {
            Self {
                deltas: deltas
                    .into_iter()
                    .map(|(delay, delta)| (delay, delta.to_owned()))
                    .collect(),
                wait_for_cancellation: false,
                completion_delay: delay,
                result: ScriptedTurnResult::Complete(completion.to_owned()),
            }
        }
    }

    async fn spawn_gateway_server(
        runtime: Arc<dyn GatewayTurnRunner>,
    ) -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let gateway = Arc::new(GatewayServer::new(runtime));
        let app = Arc::clone(&gateway).router();
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test listener should bind");
        let address = listener
            .local_addr()
            .expect("test listener should expose local address");

        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("gateway server should serve test websocket traffic");
        });

        (address, task)
    }

    async fn connect_gateway(address: SocketAddr) -> ClientSocket {
        let url = Url::parse(&format!("ws://{address}{WS_ROUTE}"))
            .expect("test websocket URL should parse");
        let (socket, _) = connect_async(url.as_str())
            .await
            .expect("test websocket connection should succeed");
        socket
    }

    async fn send_client_frame(socket: &mut ClientSocket, frame: GatewayClientFrame) {
        let payload = serde_json::to_string(&frame).expect("client frame should serialize");
        socket
            .send(WsMessage::Text(payload.into()))
            .await
            .expect("client frame should be sent");
    }

    async fn receive_server_frame(socket: &mut ClientSocket) -> GatewayServerFrame {
        timeout(Duration::from_secs(2), async {
            loop {
                let message = socket
                    .next()
                    .await
                    .expect("expected websocket frame")
                    .expect("websocket frame should decode");
                match message {
                    WsMessage::Text(payload) => {
                        return serde_json::from_str::<GatewayServerFrame>(payload.as_ref())
                            .expect("server frame should deserialize");
                    }
                    WsMessage::Binary(payload) => {
                        return serde_json::from_slice::<GatewayServerFrame>(&payload)
                            .expect("binary server frame should deserialize");
                    }
                    WsMessage::Ping(payload) => {
                        socket
                            .send(WsMessage::Pong(payload))
                            .await
                            .expect("pong should be sent");
                    }
                    WsMessage::Pong(_) => {}
                    WsMessage::Close(_) => panic!("websocket closed before expected frame"),
                    _ => {}
                }
            }
        })
        .await
        .expect("timed out waiting for server frame")
    }

    #[tokio::test]
    async fn handshake_returns_hello_ack_with_stable_runtime_session() {
        let runtime = Arc::new(ScriptedTurnRunner::new(Vec::new()));
        let (address, server_task) = spawn_gateway_server(runtime).await;
        let mut socket = connect_gateway(address).await;

        send_client_frame(
            &mut socket,
            GatewayClientFrame::Hello(GatewayClientHello {
                request_id: "req-hello".to_owned(),
                protocol_version: GATEWAY_PROTOCOL_VERSION,
                user_id: "alice".to_owned(),
                runtime_session_id: None,
            }),
        )
        .await;

        let frame = receive_server_frame(&mut socket).await;
        match frame {
            GatewayServerFrame::HelloAck(ack) => {
                assert_eq!(ack.protocol_version, GATEWAY_PROTOCOL_VERSION);
                assert_eq!(ack.session.user_id, "alice");
                assert_eq!(ack.session.runtime_session_id, "runtime-alice");
                assert!(ack.active_turn.is_none());
            }
            other => panic!("expected hello_ack, got {other:?}"),
        }

        server_task.abort();
    }

    #[tokio::test]
    async fn handshake_rejects_unsupported_protocol_version() {
        let runtime = Arc::new(ScriptedTurnRunner::new(Vec::new()));
        let (address, server_task) = spawn_gateway_server(runtime).await;
        let mut socket = connect_gateway(address).await;

        send_client_frame(
            &mut socket,
            GatewayClientFrame::Hello(GatewayClientHello {
                request_id: "req-hello".to_owned(),
                protocol_version: GATEWAY_PROTOCOL_VERSION + 1,
                user_id: "alice".to_owned(),
                runtime_session_id: None,
            }),
        )
        .await;

        let frame = receive_server_frame(&mut socket).await;
        match frame {
            GatewayServerFrame::Error(error) => {
                assert_eq!(error.request_id.as_deref(), Some("req-hello"));
                assert!(error.message.contains("unsupported protocol version"));
            }
            other => panic!("expected error frame, got {other:?}"),
        }

        server_task.abort();
    }

    #[tokio::test]
    async fn send_turn_streams_deltas_and_completes() {
        let runtime = Arc::new(ScriptedTurnRunner::new(vec![ScriptedTurn::completed(
            vec![
                (Duration::from_millis(0), "hel"),
                (Duration::from_millis(10), "lo"),
            ],
            "hello",
        )]));
        let (address, server_task) = spawn_gateway_server(runtime).await;
        let mut socket = connect_gateway(address).await;

        send_client_frame(
            &mut socket,
            GatewayClientFrame::Hello(GatewayClientHello {
                request_id: "req-hello".to_owned(),
                protocol_version: GATEWAY_PROTOCOL_VERSION,
                user_id: "alice".to_owned(),
                runtime_session_id: None,
            }),
        )
        .await;

        let hello_ack = receive_server_frame(&mut socket).await;
        let runtime_session_id = match hello_ack {
            GatewayServerFrame::HelloAck(ack) => ack.session.runtime_session_id,
            other => panic!("expected hello_ack, got {other:?}"),
        };

        send_client_frame(
            &mut socket,
            GatewayClientFrame::SendTurn(GatewaySendTurn {
                request_id: "req-turn".to_owned(),
                runtime_session_id,
                turn_id: "turn-1".to_owned(),
                prompt: "say hello".to_owned(),
            }),
        )
        .await;

        match receive_server_frame(&mut socket).await {
            GatewayServerFrame::TurnStarted(started) => {
                assert_eq!(started.request_id, "req-turn");
                assert_eq!(started.turn.turn_id, "turn-1");
            }
            other => panic!("expected turn_started, got {other:?}"),
        }

        match receive_server_frame(&mut socket).await {
            GatewayServerFrame::AssistantDelta(delta) => assert_eq!(delta.delta, "hel"),
            other => panic!("expected assistant_delta, got {other:?}"),
        }

        match receive_server_frame(&mut socket).await {
            GatewayServerFrame::AssistantDelta(delta) => assert_eq!(delta.delta, "lo"),
            other => panic!("expected assistant_delta, got {other:?}"),
        }

        match receive_server_frame(&mut socket).await {
            GatewayServerFrame::TurnCompleted(completed) => {
                assert_eq!(completed.turn.state, GatewayTurnState::Completed);
                assert_eq!(completed.response.message.content.as_deref(), Some("hello"));
            }
            other => panic!("expected turn_completed, got {other:?}"),
        }

        server_task.abort();
    }

    #[tokio::test]
    async fn cancel_active_turn_cancels_only_current_turn() {
        let runtime = Arc::new(ScriptedTurnRunner::new(vec![
            ScriptedTurn::cancellation_aware(vec![(Duration::from_millis(5), "working")]),
            ScriptedTurn::completed(vec![(Duration::from_millis(0), "done")], "done"),
        ]));
        let (address, server_task) = spawn_gateway_server(runtime).await;
        let mut socket = connect_gateway(address).await;

        send_client_frame(
            &mut socket,
            GatewayClientFrame::Hello(GatewayClientHello {
                request_id: "req-hello".to_owned(),
                protocol_version: GATEWAY_PROTOCOL_VERSION,
                user_id: "alice".to_owned(),
                runtime_session_id: None,
            }),
        )
        .await;

        let hello_ack = receive_server_frame(&mut socket).await;
        let runtime_session_id = match hello_ack {
            GatewayServerFrame::HelloAck(ack) => ack.session.runtime_session_id,
            other => panic!("expected hello_ack, got {other:?}"),
        };

        send_client_frame(
            &mut socket,
            GatewayClientFrame::SendTurn(GatewaySendTurn {
                request_id: "req-turn-1".to_owned(),
                runtime_session_id: runtime_session_id.clone(),
                turn_id: "turn-1".to_owned(),
                prompt: "long task".to_owned(),
            }),
        )
        .await;

        let _ = receive_server_frame(&mut socket).await;
        let _ = receive_server_frame(&mut socket).await;

        send_client_frame(
            &mut socket,
            GatewayClientFrame::CancelActiveTurn(GatewayCancelActiveTurn {
                request_id: "req-cancel".to_owned(),
                runtime_session_id: runtime_session_id.clone(),
                turn_id: "turn-1".to_owned(),
            }),
        )
        .await;

        let mut cancelled = false;
        for _ in 0..4 {
            if let GatewayServerFrame::TurnCancelled(cancelled_frame) =
                receive_server_frame(&mut socket).await
            {
                assert_eq!(cancelled_frame.turn.turn_id, "turn-1");
                cancelled = true;
                break;
            }
        }
        assert!(cancelled, "expected turn_cancelled frame");

        send_client_frame(
            &mut socket,
            GatewayClientFrame::SendTurn(GatewaySendTurn {
                request_id: "req-turn-2".to_owned(),
                runtime_session_id,
                turn_id: "turn-2".to_owned(),
                prompt: "short task".to_owned(),
            }),
        )
        .await;

        let _ = receive_server_frame(&mut socket).await;
        let _ = receive_server_frame(&mut socket).await;
        match receive_server_frame(&mut socket).await {
            GatewayServerFrame::TurnCompleted(completed) => {
                assert_eq!(completed.turn.turn_id, "turn-2");
                assert_eq!(completed.turn.state, GatewayTurnState::Completed);
            }
            other => panic!("expected turn_completed for second turn, got {other:?}"),
        }

        server_task.abort();
    }

    #[tokio::test]
    async fn reconnect_during_active_turn_reports_running_and_continues_streaming() {
        let runtime = Arc::new(ScriptedTurnRunner::new(vec![
            ScriptedTurn::completed_with_delay(
                vec![
                    (Duration::from_millis(0), "first"),
                    (Duration::from_millis(120), "second"),
                ],
                "final",
                Duration::from_millis(10),
            ),
        ]));
        let (address, server_task) = spawn_gateway_server(runtime).await;

        let mut first_socket = connect_gateway(address).await;
        send_client_frame(
            &mut first_socket,
            GatewayClientFrame::Hello(GatewayClientHello {
                request_id: "req-hello-1".to_owned(),
                protocol_version: GATEWAY_PROTOCOL_VERSION,
                user_id: "alice".to_owned(),
                runtime_session_id: None,
            }),
        )
        .await;

        let hello_ack = receive_server_frame(&mut first_socket).await;
        let runtime_session_id = match hello_ack {
            GatewayServerFrame::HelloAck(ack) => ack.session.runtime_session_id,
            other => panic!("expected hello_ack, got {other:?}"),
        };

        send_client_frame(
            &mut first_socket,
            GatewayClientFrame::SendTurn(GatewaySendTurn {
                request_id: "req-turn".to_owned(),
                runtime_session_id: runtime_session_id.clone(),
                turn_id: "turn-1".to_owned(),
                prompt: "stream".to_owned(),
            }),
        )
        .await;

        let _ = receive_server_frame(&mut first_socket).await;
        let _ = receive_server_frame(&mut first_socket).await;
        drop(first_socket);

        tokio::time::sleep(Duration::from_millis(30)).await;

        let mut second_socket = connect_gateway(address).await;
        send_client_frame(
            &mut second_socket,
            GatewayClientFrame::Hello(GatewayClientHello {
                request_id: "req-hello-2".to_owned(),
                protocol_version: GATEWAY_PROTOCOL_VERSION,
                user_id: "alice".to_owned(),
                runtime_session_id: Some(runtime_session_id),
            }),
        )
        .await;

        match receive_server_frame(&mut second_socket).await {
            GatewayServerFrame::HelloAck(ack) => {
                let active_turn = ack
                    .active_turn
                    .expect("expected active turn to remain running after reconnect");
                assert_eq!(active_turn.turn_id, "turn-1");
                assert_eq!(active_turn.state, GatewayTurnState::Running);
            }
            other => panic!("expected hello_ack, got {other:?}"),
        }

        match receive_server_frame(&mut second_socket).await {
            GatewayServerFrame::AssistantDelta(delta) => assert_eq!(delta.delta, "second"),
            other => panic!("expected assistant_delta after reconnect, got {other:?}"),
        }

        match receive_server_frame(&mut second_socket).await {
            GatewayServerFrame::TurnCompleted(completed) => {
                assert_eq!(completed.turn.turn_id, "turn-1");
                assert_eq!(completed.response.message.content.as_deref(), Some("final"));
            }
            other => panic!("expected turn_completed after reconnect, got {other:?}"),
        }

        server_task.abort();
    }

    #[tokio::test]
    async fn reconnect_after_disconnected_completion_receives_terminal_outcome() {
        let runtime = Arc::new(ScriptedTurnRunner::new(vec![
            ScriptedTurn::completed_with_delay(
                vec![(Duration::from_millis(0), "first")],
                "done",
                Duration::from_millis(30),
            ),
        ]));
        let (address, server_task) = spawn_gateway_server(runtime).await;

        let mut first_socket = connect_gateway(address).await;
        send_client_frame(
            &mut first_socket,
            GatewayClientFrame::Hello(GatewayClientHello {
                request_id: "req-hello-1".to_owned(),
                protocol_version: GATEWAY_PROTOCOL_VERSION,
                user_id: "alice".to_owned(),
                runtime_session_id: None,
            }),
        )
        .await;

        let hello_ack = receive_server_frame(&mut first_socket).await;
        let runtime_session_id = match hello_ack {
            GatewayServerFrame::HelloAck(ack) => ack.session.runtime_session_id,
            other => panic!("expected hello_ack, got {other:?}"),
        };

        send_client_frame(
            &mut first_socket,
            GatewayClientFrame::SendTurn(GatewaySendTurn {
                request_id: "req-turn".to_owned(),
                runtime_session_id: runtime_session_id.clone(),
                turn_id: "turn-1".to_owned(),
                prompt: "stream".to_owned(),
            }),
        )
        .await;

        let _ = receive_server_frame(&mut first_socket).await;
        let _ = receive_server_frame(&mut first_socket).await;
        drop(first_socket);

        tokio::time::sleep(Duration::from_millis(120)).await;

        let mut second_socket = connect_gateway(address).await;
        send_client_frame(
            &mut second_socket,
            GatewayClientFrame::Hello(GatewayClientHello {
                request_id: "req-hello-2".to_owned(),
                protocol_version: GATEWAY_PROTOCOL_VERSION,
                user_id: "alice".to_owned(),
                runtime_session_id: Some(runtime_session_id),
            }),
        )
        .await;

        match receive_server_frame(&mut second_socket).await {
            GatewayServerFrame::HelloAck(ack) => {
                assert!(ack.active_turn.is_none());
            }
            other => panic!("expected hello_ack, got {other:?}"),
        }

        match receive_server_frame(&mut second_socket).await {
            GatewayServerFrame::TurnCompleted(completed) => {
                assert_eq!(completed.turn.turn_id, "turn-1");
                assert_eq!(completed.response.message.content.as_deref(), Some("done"));
            }
            other => panic!("expected cached turn_completed frame, got {other:?}"),
        }

        server_task.abort();
    }

    #[tokio::test]
    async fn runtime_session_id_remains_stable_for_user() {
        let runtime = Arc::new(ScriptedTurnRunner::new(Vec::new()));
        let (address, server_task) =
            spawn_gateway_server(Arc::clone(&runtime) as Arc<dyn GatewayTurnRunner>).await;

        let mut first_socket = connect_gateway(address).await;
        send_client_frame(
            &mut first_socket,
            GatewayClientFrame::Hello(GatewayClientHello {
                request_id: "req-hello-1".to_owned(),
                protocol_version: GATEWAY_PROTOCOL_VERSION,
                user_id: "alice".to_owned(),
                runtime_session_id: None,
            }),
        )
        .await;

        let first_runtime_session_id = match receive_server_frame(&mut first_socket).await {
            GatewayServerFrame::HelloAck(ack) => ack.session.runtime_session_id,
            other => panic!("expected hello_ack, got {other:?}"),
        };
        drop(first_socket);

        let mut second_socket = connect_gateway(address).await;
        send_client_frame(
            &mut second_socket,
            GatewayClientFrame::Hello(GatewayClientHello {
                request_id: "req-hello-2".to_owned(),
                protocol_version: GATEWAY_PROTOCOL_VERSION,
                user_id: "alice".to_owned(),
                runtime_session_id: None,
            }),
        )
        .await;

        let second_runtime_session_id = match receive_server_frame(&mut second_socket).await {
            GatewayServerFrame::HelloAck(ack) => ack.session.runtime_session_id,
            other => panic!("expected hello_ack, got {other:?}"),
        };

        assert_eq!(first_runtime_session_id, second_runtime_session_id);

        let recorded_calls = runtime.recorded_calls().await;
        assert!(recorded_calls.is_empty());

        server_task.abort();
    }

    #[tokio::test]
    async fn runner_failure_maps_to_gateway_error_frame() {
        let runtime = Arc::new(ScriptedTurnRunner::new(vec![ScriptedTurn {
            deltas: Vec::new(),
            wait_for_cancellation: false,
            completion_delay: Duration::ZERO,
            result: ScriptedTurnResult::Fail("boom".to_owned()),
        }]));
        let (address, server_task) = spawn_gateway_server(runtime).await;
        let mut socket = connect_gateway(address).await;

        send_client_frame(
            &mut socket,
            GatewayClientFrame::Hello(GatewayClientHello {
                request_id: "req-hello".to_owned(),
                protocol_version: GATEWAY_PROTOCOL_VERSION,
                user_id: "alice".to_owned(),
                runtime_session_id: None,
            }),
        )
        .await;

        let hello_ack = receive_server_frame(&mut socket).await;
        let runtime_session_id = match hello_ack {
            GatewayServerFrame::HelloAck(ack) => ack.session.runtime_session_id,
            other => panic!("expected hello_ack, got {other:?}"),
        };

        send_client_frame(
            &mut socket,
            GatewayClientFrame::SendTurn(GatewaySendTurn {
                request_id: "req-turn".to_owned(),
                runtime_session_id,
                turn_id: "turn-1".to_owned(),
                prompt: "fail".to_owned(),
            }),
        )
        .await;

        let _ = receive_server_frame(&mut socket).await;
        match receive_server_frame(&mut socket).await {
            GatewayServerFrame::Error(error) => {
                assert_eq!(error.request_id.as_deref(), Some("req-turn"));
                assert!(error.message.contains("boom"));
            }
            other => panic!("expected error frame, got {other:?}"),
        }

        server_task.abort();
    }
}
