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
