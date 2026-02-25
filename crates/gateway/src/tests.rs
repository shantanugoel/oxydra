use std::{collections::VecDeque, net::SocketAddr, time::Duration};

use super::*;
use tokio::net::TcpListener;
use tokio::time::timeout;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message as WsMessage,
};
use types::{ProviderError, StreamItem};
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
        _user_id: &str,
        session_id: &str,
        prompt: String,
        cancellation: CancellationToken,
        delta_sender: mpsc::UnboundedSender<StreamItem>,
    ) -> Result<Response, RuntimeError> {
        self.recorded_calls
            .lock()
            .await
            .push((session_id.to_owned(), prompt));

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
                    let _ = delta_sender.send(StreamItem::Text(delta));
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
    spawn_gateway_server_with_startup_status(runtime, None).await
}

async fn spawn_gateway_server_with_startup_status(
    runtime: Arc<dyn GatewayTurnRunner>,
    startup_status: Option<StartupStatusReport>,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let gateway = match startup_status {
        Some(startup_status) => {
            Arc::new(GatewayServer::with_startup_status(runtime, startup_status))
        }
        None => Arc::new(GatewayServer::new(runtime)),
    };
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
    let url =
        Url::parse(&format!("ws://{address}{WS_ROUTE}")).expect("test websocket URL should parse");
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
async fn handshake_returns_hello_ack_with_stable_session() {
    let runtime = Arc::new(ScriptedTurnRunner::new(Vec::new()));
    let (address, server_task) = spawn_gateway_server(runtime).await;
    let mut socket = connect_gateway(address).await;

    send_client_frame(
        &mut socket,
        GatewayClientFrame::Hello(GatewayClientHello {
            request_id: "req-hello".to_owned(),
            protocol_version: GATEWAY_PROTOCOL_VERSION,
            user_id: "alice".to_owned(),
            session_id: None,
            create_new_session: false,
        }),
    )
    .await;

    let frame = receive_server_frame(&mut socket).await;
    match frame {
        GatewayServerFrame::HelloAck(ack) => {
            assert_eq!(ack.protocol_version, GATEWAY_PROTOCOL_VERSION);
            assert_eq!(ack.session.user_id, "alice");
            assert_eq!(ack.session.session_id, "runtime-alice");
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
            session_id: None,
            create_new_session: false,
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
            session_id: None,
            create_new_session: false,
        }),
    )
    .await;

    let hello_ack = receive_server_frame(&mut socket).await;
    let session_id = match hello_ack {
        GatewayServerFrame::HelloAck(ack) => ack.session.session_id,
        other => panic!("expected hello_ack, got {other:?}"),
    };

    send_client_frame(
        &mut socket,
        GatewayClientFrame::SendTurn(GatewaySendTurn {
            request_id: "req-turn".to_owned(),
            session_id,
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
            session_id: None,
            create_new_session: false,
        }),
    )
    .await;

    let hello_ack = receive_server_frame(&mut socket).await;
    let session_id = match hello_ack {
        GatewayServerFrame::HelloAck(ack) => ack.session.session_id,
        other => panic!("expected hello_ack, got {other:?}"),
    };

    send_client_frame(
        &mut socket,
        GatewayClientFrame::SendTurn(GatewaySendTurn {
            request_id: "req-turn-1".to_owned(),
            session_id: session_id.clone(),
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
            session_id: session_id.clone(),
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
            session_id,
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
            session_id: None,
            create_new_session: false,
        }),
    )
    .await;

    let hello_ack = receive_server_frame(&mut first_socket).await;
    let session_id = match hello_ack {
        GatewayServerFrame::HelloAck(ack) => ack.session.session_id,
        other => panic!("expected hello_ack, got {other:?}"),
    };

    send_client_frame(
        &mut first_socket,
        GatewayClientFrame::SendTurn(GatewaySendTurn {
            request_id: "req-turn".to_owned(),
            session_id: session_id.clone(),
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
            session_id: Some(session_id),
            create_new_session: false,
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
            session_id: None,
            create_new_session: false,
        }),
    )
    .await;

    let hello_ack = receive_server_frame(&mut first_socket).await;
    let session_id = match hello_ack {
        GatewayServerFrame::HelloAck(ack) => ack.session.session_id,
        other => panic!("expected hello_ack, got {other:?}"),
    };

    send_client_frame(
        &mut first_socket,
        GatewayClientFrame::SendTurn(GatewaySendTurn {
            request_id: "req-turn".to_owned(),
            session_id: session_id.clone(),
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
            session_id: Some(session_id),
            create_new_session: false,
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
async fn session_id_remains_stable_for_user() {
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
            session_id: None,
            create_new_session: false,
        }),
    )
    .await;

    let first_session_id = match receive_server_frame(&mut first_socket).await {
        GatewayServerFrame::HelloAck(ack) => ack.session.session_id,
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
            session_id: None,
            create_new_session: false,
        }),
    )
    .await;

    let second_session_id = match receive_server_frame(&mut second_socket).await {
        GatewayServerFrame::HelloAck(ack) => ack.session.session_id,
        other => panic!("expected hello_ack, got {other:?}"),
    };

    assert_eq!(first_session_id, second_session_id);

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
            session_id: None,
            create_new_session: false,
        }),
    )
    .await;

    let hello_ack = receive_server_frame(&mut socket).await;
    let session_id = match hello_ack {
        GatewayServerFrame::HelloAck(ack) => ack.session.session_id,
        other => panic!("expected hello_ack, got {other:?}"),
    };

    send_client_frame(
        &mut socket,
        GatewayClientFrame::SendTurn(GatewaySendTurn {
            request_id: "req-turn".to_owned(),
            session_id,
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

#[tokio::test]
async fn health_check_includes_startup_status_when_gateway_has_bootstrap_state() {
    let runtime = Arc::new(ScriptedTurnRunner::new(Vec::new()));
    let startup_status = types::StartupStatusReport {
        sandbox_tier: types::SandboxTier::Process,
        sidecar_available: false,
        shell_available: false,
        browser_available: false,
        degraded_reasons: vec![types::StartupDegradedReason::new(
            types::StartupDegradedReasonCode::InsecureProcessTier,
            "process tier is insecure: isolation is degraded and not production-safe; shell/browser tools are disabled.",
        )],
    };
    let (address, server_task) = spawn_gateway_server_with_startup_status(
        runtime as Arc<dyn GatewayTurnRunner>,
        Some(startup_status.clone()),
    )
    .await;
    let mut socket = connect_gateway(address).await;

    send_client_frame(
        &mut socket,
        GatewayClientFrame::Hello(GatewayClientHello {
            request_id: "req-hello".to_owned(),
            protocol_version: GATEWAY_PROTOCOL_VERSION,
            user_id: "alice".to_owned(),
            session_id: None,
            create_new_session: false,
        }),
    )
    .await;
    let _ = receive_server_frame(&mut socket).await;

    send_client_frame(
        &mut socket,
        GatewayClientFrame::HealthCheck(GatewayHealthCheck {
            request_id: "req-health".to_owned(),
        }),
    )
    .await;

    match receive_server_frame(&mut socket).await {
        GatewayServerFrame::HealthStatus(status) => {
            assert!(status.healthy);
            assert_eq!(status.request_id, "req-health");
            assert_eq!(status.startup_status, Some(startup_status));
            assert!(
                status
                    .message
                    .as_deref()
                    .is_some_and(|message| message.contains("degraded startup"))
            );
        }
        other => panic!("expected health_status, got {other:?}"),
    }

    server_task.abort();
}

// ===================================================================
// Multi-session tests (Step 3)
// ===================================================================

#[tokio::test]
async fn create_new_session_creates_fresh_session_with_uuid() {
    let runtime = Arc::new(ScriptedTurnRunner::new(Vec::new()));
    let (address, server_task) = spawn_gateway_server(runtime).await;
    let mut socket = connect_gateway(address).await;

    send_client_frame(
        &mut socket,
        GatewayClientFrame::Hello(GatewayClientHello {
            request_id: "req-hello".to_owned(),
            protocol_version: GATEWAY_PROTOCOL_VERSION,
            user_id: "alice".to_owned(),
            session_id: None,
            create_new_session: true,
        }),
    )
    .await;

    let frame = receive_server_frame(&mut socket).await;
    match frame {
        GatewayServerFrame::HelloAck(ack) => {
            assert_eq!(ack.session.user_id, "alice");
            assert_ne!(ack.session.session_id, "runtime-alice");
            assert_eq!(ack.session.session_id.len(), 36);
        }
        other => panic!("expected hello_ack, got {other:?}"),
    }

    server_task.abort();
}

#[tokio::test]
async fn two_sessions_for_same_user_run_turns_concurrently() {
    let runtime = Arc::new(ScriptedTurnRunner::new(vec![
        ScriptedTurn::completed_with_delay(
            vec![(Duration::from_millis(0), "ses1-delta")],
            "ses1-done",
            Duration::from_millis(200),
        ),
        ScriptedTurn::completed(
            vec![(Duration::from_millis(0), "ses2-delta")],
            "ses2-done",
        ),
    ]));
    let (address, server_task) = spawn_gateway_server(runtime).await;

    let mut socket1 = connect_gateway(address).await;
    send_client_frame(
        &mut socket1,
        GatewayClientFrame::Hello(GatewayClientHello {
            request_id: "req-hello-1".to_owned(),
            protocol_version: GATEWAY_PROTOCOL_VERSION,
            user_id: "alice".to_owned(),
            session_id: None,
            create_new_session: true,
        }),
    )
    .await;
    let session_id_1 = match receive_server_frame(&mut socket1).await {
        GatewayServerFrame::HelloAck(ack) => ack.session.session_id,
        other => panic!("expected hello_ack, got {other:?}"),
    };

    let mut socket2 = connect_gateway(address).await;
    send_client_frame(
        &mut socket2,
        GatewayClientFrame::Hello(GatewayClientHello {
            request_id: "req-hello-2".to_owned(),
            protocol_version: GATEWAY_PROTOCOL_VERSION,
            user_id: "alice".to_owned(),
            session_id: None,
            create_new_session: true,
        }),
    )
    .await;
    let session_id_2 = match receive_server_frame(&mut socket2).await {
        GatewayServerFrame::HelloAck(ack) => ack.session.session_id,
        other => panic!("expected hello_ack, got {other:?}"),
    };
    assert_ne!(session_id_1, session_id_2);

    send_client_frame(
        &mut socket1,
        GatewayClientFrame::SendTurn(GatewaySendTurn {
            request_id: "req-turn-1".to_owned(),
            session_id: session_id_1,
            turn_id: "turn-1".to_owned(),
            prompt: "task 1".to_owned(),
        }),
    )
    .await;
    send_client_frame(
        &mut socket2,
        GatewayClientFrame::SendTurn(GatewaySendTurn {
            request_id: "req-turn-2".to_owned(),
            session_id: session_id_2,
            turn_id: "turn-2".to_owned(),
            prompt: "task 2".to_owned(),
        }),
    )
    .await;

    match receive_server_frame(&mut socket1).await {
        GatewayServerFrame::TurnStarted(s) => assert_eq!(s.turn.turn_id, "turn-1"),
        other => panic!("expected turn_started on socket1, got {other:?}"),
    }
    match receive_server_frame(&mut socket2).await {
        GatewayServerFrame::TurnStarted(s) => assert_eq!(s.turn.turn_id, "turn-2"),
        other => panic!("expected turn_started on socket2, got {other:?}"),
    }

    let _ = receive_server_frame(&mut socket2).await;
    match receive_server_frame(&mut socket2).await {
        GatewayServerFrame::TurnCompleted(c) => assert_eq!(c.turn.turn_id, "turn-2"),
        other => panic!("expected turn_completed on socket2, got {other:?}"),
    }

    let _ = receive_server_frame(&mut socket1).await;
    match receive_server_frame(&mut socket1).await {
        GatewayServerFrame::TurnCompleted(c) => assert_eq!(c.turn.turn_id, "turn-1"),
        other => panic!("expected turn_completed on socket1, got {other:?}"),
    }

    server_task.abort();
}

#[tokio::test]
async fn session_id_in_hello_joins_existing_session() {
    let runtime = Arc::new(ScriptedTurnRunner::new(vec![ScriptedTurn::completed(
        vec![(Duration::from_millis(0), "delta")],
        "done",
    )]));
    let (address, server_task) = spawn_gateway_server(runtime).await;

    let mut socket1 = connect_gateway(address).await;
    send_client_frame(
        &mut socket1,
        GatewayClientFrame::Hello(GatewayClientHello {
            request_id: "req-hello-1".to_owned(),
            protocol_version: GATEWAY_PROTOCOL_VERSION,
            user_id: "alice".to_owned(),
            session_id: None,
            create_new_session: true,
        }),
    )
    .await;
    let session_id = match receive_server_frame(&mut socket1).await {
        GatewayServerFrame::HelloAck(ack) => ack.session.session_id,
        other => panic!("expected hello_ack, got {other:?}"),
    };

    send_client_frame(
        &mut socket1,
        GatewayClientFrame::SendTurn(GatewaySendTurn {
            request_id: "req-turn".to_owned(),
            session_id: session_id.clone(),
            turn_id: "turn-1".to_owned(),
            prompt: "hello".to_owned(),
        }),
    )
    .await;
    let _ = receive_server_frame(&mut socket1).await;
    let _ = receive_server_frame(&mut socket1).await;
    let _ = receive_server_frame(&mut socket1).await;
    drop(socket1);

    let mut socket2 = connect_gateway(address).await;
    send_client_frame(
        &mut socket2,
        GatewayClientFrame::Hello(GatewayClientHello {
            request_id: "req-hello-2".to_owned(),
            protocol_version: GATEWAY_PROTOCOL_VERSION,
            user_id: "alice".to_owned(),
            session_id: Some(session_id.clone()),
            create_new_session: false,
        }),
    )
    .await;
    match receive_server_frame(&mut socket2).await {
        GatewayServerFrame::HelloAck(ack) => assert_eq!(ack.session.session_id, session_id),
        other => panic!("expected hello_ack, got {other:?}"),
    }

    server_task.abort();
}

#[tokio::test]
async fn concurrent_turn_limit_enforced() {
    let runtime = Arc::new(ScriptedTurnRunner::new(vec![
        ScriptedTurn::cancellation_aware(vec![(Duration::from_millis(5), "working")]),
    ]));
    let gateway = Arc::new(GatewayServer::with_options(
        runtime as Arc<dyn GatewayTurnRunner>,
        None,
        None,
        1,
    ));
    let app = Arc::clone(&gateway).router();
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("local addr");
    let server_task = tokio::spawn(async move { axum::serve(listener, app).await.ok(); });

    let mut socket1 = connect_gateway(address).await;
    send_client_frame(
        &mut socket1,
        GatewayClientFrame::Hello(GatewayClientHello {
            request_id: "req-hello-1".to_owned(),
            protocol_version: GATEWAY_PROTOCOL_VERSION,
            user_id: "alice".to_owned(),
            session_id: None,
            create_new_session: true,
        }),
    )
    .await;
    let sid1 = match receive_server_frame(&mut socket1).await {
        GatewayServerFrame::HelloAck(ack) => ack.session.session_id,
        other => panic!("expected hello_ack, got {other:?}"),
    };
    send_client_frame(
        &mut socket1,
        GatewayClientFrame::SendTurn(GatewaySendTurn {
            request_id: "req-turn-1".to_owned(),
            session_id: sid1,
            turn_id: "turn-1".to_owned(),
            prompt: "long".to_owned(),
        }),
    )
    .await;
    let _ = receive_server_frame(&mut socket1).await;

    let mut socket2 = connect_gateway(address).await;
    send_client_frame(
        &mut socket2,
        GatewayClientFrame::Hello(GatewayClientHello {
            request_id: "req-hello-2".to_owned(),
            protocol_version: GATEWAY_PROTOCOL_VERSION,
            user_id: "alice".to_owned(),
            session_id: None,
            create_new_session: true,
        }),
    )
    .await;
    let sid2 = match receive_server_frame(&mut socket2).await {
        GatewayServerFrame::HelloAck(ack) => ack.session.session_id,
        other => panic!("expected hello_ack, got {other:?}"),
    };
    send_client_frame(
        &mut socket2,
        GatewayClientFrame::SendTurn(GatewaySendTurn {
            request_id: "req-turn-2".to_owned(),
            session_id: sid2,
            turn_id: "turn-2".to_owned(),
            prompt: "blocked".to_owned(),
        }),
    )
    .await;

    match receive_server_frame(&mut socket2).await {
        GatewayServerFrame::Error(e) => {
            assert!(e.message.contains("too many concurrent turns"), "got: {}", e.message);
        }
        other => panic!("expected error frame, got {other:?}"),
    }

    server_task.abort();
}

// ===================================================================
// Session persistence tests (Step 4)
// ===================================================================

async fn build_gateway_with_session_store(
    runtime: Arc<dyn GatewayTurnRunner>,
) -> (Arc<GatewayServer>, Arc<dyn types::SessionStore>) {
    let db = libsql::Builder::new_local(":memory:").build().await.expect("db");
    let conn = db.connect().expect("connect");
    conn.execute_batch(include_str!("../../memory/migrations/0020_create_gateway_sessions.sql"))
        .await
        .expect("migration");
    let store: Arc<dyn types::SessionStore> = Arc::new(memory::LibsqlSessionStore::new(conn));
    let gateway = Arc::new(GatewayServer::with_session_store(runtime, None, store.clone()));
    (gateway, store)
}

async fn spawn_gateway_with_session_store(
    runtime: Arc<dyn GatewayTurnRunner>,
) -> (SocketAddr, tokio::task::JoinHandle<()>, Arc<dyn types::SessionStore>) {
    let (gateway, store) = build_gateway_with_session_store(runtime).await;
    let app = Arc::clone(&gateway).router();
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("addr");
    let task = tokio::spawn(async move { axum::serve(listener, app).await.ok(); });
    (address, task, store)
}

#[tokio::test]
async fn session_persisted_to_store_on_creation() {
    let runtime = Arc::new(ScriptedTurnRunner::new(Vec::new()));
    let (address, server_task, store) =
        spawn_gateway_with_session_store(runtime as Arc<dyn GatewayTurnRunner>).await;
    let mut socket = connect_gateway(address).await;
    send_client_frame(
        &mut socket,
        GatewayClientFrame::Hello(GatewayClientHello {
            request_id: "req-hello".to_owned(),
            protocol_version: GATEWAY_PROTOCOL_VERSION,
            user_id: "alice".to_owned(),
            session_id: None,
            create_new_session: true,
        }),
    )
    .await;
    let sid = match receive_server_frame(&mut socket).await {
        GatewayServerFrame::HelloAck(ack) => ack.session.session_id,
        other => panic!("expected hello_ack, got {other:?}"),
    };
    let record = store.get_session(&sid).await.expect("get").expect("exists");
    assert_eq!(record.user_id, "alice");
    assert_eq!(record.agent_name, "default");
    assert!(!record.archived);
    server_task.abort();
}

#[tokio::test]
async fn touch_session_called_on_turn_completion() {
    let runtime = Arc::new(ScriptedTurnRunner::new(vec![ScriptedTurn::completed(
        vec![(Duration::from_millis(0), "hi")],
        "hello",
    )]));
    let (address, server_task, store) =
        spawn_gateway_with_session_store(runtime as Arc<dyn GatewayTurnRunner>).await;
    let mut socket = connect_gateway(address).await;
    send_client_frame(
        &mut socket,
        GatewayClientFrame::Hello(GatewayClientHello {
            request_id: "req-hello".to_owned(),
            protocol_version: GATEWAY_PROTOCOL_VERSION,
            user_id: "alice".to_owned(),
            session_id: None,
            create_new_session: true,
        }),
    )
    .await;
    let sid = match receive_server_frame(&mut socket).await {
        GatewayServerFrame::HelloAck(ack) => ack.session.session_id,
        other => panic!("expected hello_ack, got {other:?}"),
    };
    let before = store.get_session(&sid).await.unwrap().unwrap().last_active_at.clone();
    send_client_frame(
        &mut socket,
        GatewayClientFrame::SendTurn(GatewaySendTurn {
            request_id: "req-turn".to_owned(),
            session_id: sid.clone(),
            turn_id: "turn-1".to_owned(),
            prompt: "greet".to_owned(),
        }),
    )
    .await;
    let _ = receive_server_frame(&mut socket).await;
    let _ = receive_server_frame(&mut socket).await;
    match receive_server_frame(&mut socket).await {
        GatewayServerFrame::TurnCompleted(_) => {}
        other => panic!("expected turn_completed, got {other:?}"),
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let after = store.get_session(&sid).await.unwrap().unwrap().last_active_at;
    assert!(after >= before, "last_active_at should be updated");
    server_task.abort();
}

#[tokio::test]
async fn internal_api_create_and_list_sessions() {
    let runtime = Arc::new(ScriptedTurnRunner::new(Vec::new()));
    let (gateway, store) =
        build_gateway_with_session_store(runtime as Arc<dyn GatewayTurnRunner>).await;
    let _s1 = gateway.create_or_get_session("alice", None, "default", "tui").await.unwrap();
    let _s2 = gateway.create_or_get_session("alice", None, "default", "tui").await.unwrap();
    assert_eq!(store.list_sessions("alice", false).await.unwrap().len(), 2);
    assert_eq!(gateway.list_user_sessions("alice", false).await.unwrap().len(), 2);
}

#[tokio::test]
async fn internal_api_get_existing_session_by_id() {
    let runtime = Arc::new(ScriptedTurnRunner::new(Vec::new()));
    let (gateway, _store) =
        build_gateway_with_session_store(runtime as Arc<dyn GatewayTurnRunner>).await;
    let ses = gateway.create_or_get_session("alice", None, "default", "tui").await.unwrap();
    let sid = ses.session_id.clone();
    let same = gateway.create_or_get_session("alice", Some(&sid), "default", "tui").await.unwrap();
    assert_eq!(same.session_id, sid);
}

#[tokio::test]
async fn default_session_backward_compat() {
    let runtime = Arc::new(ScriptedTurnRunner::new(Vec::new()));
    let (address, server_task) = spawn_gateway_server(runtime).await;
    let mut socket = connect_gateway(address).await;
    send_client_frame(
        &mut socket,
        GatewayClientFrame::Hello(GatewayClientHello {
            request_id: "req-hello".to_owned(),
            protocol_version: GATEWAY_PROTOCOL_VERSION,
            user_id: "alice".to_owned(),
            session_id: None,
            create_new_session: false,
        }),
    )
    .await;
    match receive_server_frame(&mut socket).await {
        GatewayServerFrame::HelloAck(ack) => assert_eq!(ack.session.session_id, "runtime-alice"),
        other => panic!("expected hello_ack, got {other:?}"),
    }
    server_task.abort();
}
