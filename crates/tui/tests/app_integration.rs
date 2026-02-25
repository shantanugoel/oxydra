//! Integration tests for the TUI application layer.
//!
//! These tests spin up a mock WebSocket server that speaks the gateway protocol
//! (sending scripted `GatewayServerFrame`s), then drive the `TuiChannelAdapter`
//! and `TuiViewModel` through a full handshake → prompt → delta → completion
//! cycle and reconnection scenario.

use std::net::SocketAddr;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use types::{
    GATEWAY_PROTOCOL_VERSION, GatewayAssistantDelta, GatewayClientFrame, GatewayHelloAck,
    GatewayServerFrame, GatewaySession, GatewayTurnCompleted, GatewayTurnStarted, GatewayTurnState,
    GatewayTurnStatus, Message, MessageRole, Response,
};

use tui::{ChatMessage, ConnectionState, TuiChannelAdapter, TuiViewModel};

// ── Test helpers ────────────────────────────────────────────────────────

fn session(session_id: &str) -> GatewaySession {
    GatewaySession {
        user_id: "alice".to_owned(),
        session_id: session_id.to_owned(),
    }
}

fn turn_status(turn_id: &str, state: GatewayTurnState) -> GatewayTurnStatus {
    GatewayTurnStatus {
        turn_id: turn_id.to_owned(),
        state,
    }
}

fn hello_ack_frame(
    session_id: &str,
    active_turn: Option<GatewayTurnStatus>,
) -> GatewayServerFrame {
    GatewayServerFrame::HelloAck(GatewayHelloAck {
        request_id: "req-hello".to_owned(),
        protocol_version: GATEWAY_PROTOCOL_VERSION,
        session: session(session_id),
        active_turn,
    })
}

fn turn_started_frame(turn_id: &str, session_id: &str) -> GatewayServerFrame {
    GatewayServerFrame::TurnStarted(GatewayTurnStarted {
        request_id: "req-turn".to_owned(),
        session: session(session_id),
        turn: turn_status(turn_id, GatewayTurnState::Running),
    })
}

fn delta_frame(turn_id: &str, session_id: &str, text: &str) -> GatewayServerFrame {
    GatewayServerFrame::AssistantDelta(GatewayAssistantDelta {
        request_id: "req-turn".to_owned(),
        session: session(session_id),
        turn: turn_status(turn_id, GatewayTurnState::Running),
        delta: text.to_owned(),
    })
}

fn completed_frame(
    turn_id: &str,
    session_id: &str,
    final_text: &str,
) -> GatewayServerFrame {
    GatewayServerFrame::TurnCompleted(GatewayTurnCompleted {
        request_id: "req-turn".to_owned(),
        session: session(session_id),
        turn: turn_status(turn_id, GatewayTurnState::Completed),
        response: Response {
            message: Message {
                role: MessageRole::Assistant,
                content: Some(final_text.to_owned()),
                tool_calls: Vec::new(),
                tool_call_id: None,
            },
            tool_calls: Vec::new(),
            finish_reason: Some("stop".to_owned()),
            usage: None,
        },
    })
}

/// Spawn a mock WebSocket server that accepts one connection, reads the
/// client `Hello`, and then sends a scripted sequence of server frames.
///
/// Returns the address and a join handle. The server closes after sending
/// all frames.
async fn spawn_mock_server(
    frames: Vec<GatewayServerFrame>,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("mock server should bind");
    let address = listener.local_addr().expect("should have local address");

    let handle = tokio::spawn(async move {
        let (stream, _) = listener
            .accept()
            .await
            .expect("should accept tcp connection");
        let mut ws = tokio_tungstenite::accept_async(stream)
            .await
            .expect("websocket accept should succeed");

        // Wait for the client Hello frame.
        let msg = timeout(Duration::from_secs(2), ws.next())
            .await
            .expect("should receive client hello within timeout")
            .expect("should have a message")
            .expect("message should decode");

        // Verify it's a Hello frame.
        if let WsMessage::Text(payload) = msg {
            let client_frame: GatewayClientFrame =
                serde_json::from_str(payload.as_ref()).expect("client frame should deserialize");
            assert!(
                matches!(client_frame, GatewayClientFrame::Hello(_)),
                "first client frame should be Hello"
            );
        }

        // Send scripted server frames.
        for frame in &frames {
            let payload = serde_json::to_string(frame).expect("server frame should serialize");
            ws.send(WsMessage::Text(payload.into()))
                .await
                .expect("should send frame to client");
        }

        // Keep connection open briefly so client can process all frames.
        tokio::time::sleep(Duration::from_millis(100)).await;
    });

    (address, handle)
}

/// Spawn a mock server that handles two sequential connections (for
/// reconnection testing). The first connection sends `first_frames`, then
/// the second connection sends `second_frames`.
async fn spawn_reconnecting_mock_server(
    first_frames: Vec<GatewayServerFrame>,
    second_frames: Vec<GatewayServerFrame>,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("mock server should bind");
    let address = listener.local_addr().expect("should have local address");

    let handle = tokio::spawn(async move {
        // First connection.
        {
            let (stream, _) = listener
                .accept()
                .await
                .expect("should accept first tcp connection");
            let mut ws = tokio_tungstenite::accept_async(stream)
                .await
                .expect("first ws accept should succeed");

            // Read Hello.
            let _ = timeout(Duration::from_secs(2), ws.next())
                .await
                .expect("should receive hello")
                .expect("should have message")
                .expect("should decode");

            for frame in &first_frames {
                let payload = serde_json::to_string(frame).expect("frame should serialize");
                ws.send(WsMessage::Text(payload.into()))
                    .await
                    .expect("should send first-connection frame");
            }

            // Close connection to simulate disconnect.
            let _ = ws.close(None).await;
        }

        // Second connection (reconnect).
        {
            let (stream, _) = listener
                .accept()
                .await
                .expect("should accept second tcp connection");
            let mut ws = tokio_tungstenite::accept_async(stream)
                .await
                .expect("second ws accept should succeed");

            // Read reconnect Hello.
            let msg = timeout(Duration::from_secs(2), ws.next())
                .await
                .expect("should receive reconnect hello")
                .expect("should have message")
                .expect("should decode");

            // Verify the reconnect Hello carries the session_id.
            if let WsMessage::Text(payload) = msg {
                let client_frame: GatewayClientFrame = serde_json::from_str(payload.as_ref())
                    .expect("reconnect client frame should deserialize");
                match client_frame {
                    GatewayClientFrame::Hello(hello) => {
                        assert!(
                            hello.session_id.is_some(),
                            "reconnect Hello should carry session_id"
                        );
                    }
                    other => panic!("expected Hello frame on reconnect, got {other:?}"),
                }
            }

            for frame in &second_frames {
                let payload = serde_json::to_string(frame).expect("frame should serialize");
                ws.send(WsMessage::Text(payload.into()))
                    .await
                    .expect("should send second-connection frame");
            }

            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });

    (address, handle)
}

/// Simulate the client side of connecting to the mock server: open a
/// WebSocket, send a Hello, and return the socket for reading frames.
async fn client_connect(
    address: SocketAddr,
    adapter: &TuiChannelAdapter,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let url = format!("ws://{address}");
    let (mut socket, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("client should connect to mock server");

    // Send Hello.
    let hello = adapter.build_hello_frame("req-hello").await;
    let payload = tui::encode_gateway_client_frame(&hello).expect("hello should encode");
    socket
        .send(WsMessage::Text(payload.into()))
        .await
        .expect("should send hello");

    socket
}

/// Read and decode a single server frame from the WebSocket.
async fn read_server_frame(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> GatewayServerFrame {
    let msg = timeout(Duration::from_secs(2), socket.next())
        .await
        .expect("should receive frame within timeout")
        .expect("should have a message")
        .expect("message should decode");

    match msg {
        WsMessage::Text(payload) => {
            tui::decode_gateway_server_frame(payload.as_ref()).expect("server frame should decode")
        }
        other => panic!("expected text websocket message, got {other:?}"),
    }
}

// ── Integration tests ───────────────────────────────────────────────────

#[tokio::test]
async fn full_handshake_prompt_delta_completion_cycle() {
    let scripted = vec![
        hello_ack_frame("runtime-alice", None),
        turn_started_frame("turn-1", "runtime-alice"),
        delta_frame("turn-1", "runtime-alice", "hel"),
        delta_frame("turn-1", "runtime-alice", "lo"),
        completed_frame("turn-1", "runtime-alice", "hello"),
    ];

    let (address, server_handle) = spawn_mock_server(scripted).await;
    let adapter = TuiChannelAdapter::new("alice", "conn-1");
    let mut model = TuiViewModel::new();
    let mut socket = client_connect(address, &adapter).await;

    // 1. Receive HelloAck.
    let frame = read_server_frame(&mut socket).await;
    adapter.apply_gateway_frame(&frame).await;
    model.apply_server_frame(&frame);

    let state = adapter.state_snapshot().await;
    assert!(state.connected, "should be connected after HelloAck");
    assert_eq!(
        state.session_id.as_deref(),
        Some("runtime-alice"),
        "should have session id"
    );

    // Simulate user sending a prompt.
    model.append_user_message("say hello");

    // 2. Receive TurnStarted.
    let frame = read_server_frame(&mut socket).await;
    adapter.apply_gateway_frame(&frame).await;
    model.apply_server_frame(&frame);

    let state = adapter.state_snapshot().await;
    assert_eq!(
        state.active_turn_id.as_deref(),
        Some("turn-1"),
        "should have active turn after TurnStarted"
    );

    // 3. Receive first delta.
    let frame = read_server_frame(&mut socket).await;
    adapter.apply_gateway_frame(&frame).await;
    model.apply_server_frame(&frame);

    assert_eq!(
        model.message_history.last(),
        Some(&ChatMessage::Assistant("hel".to_owned())),
        "assistant message should contain first delta"
    );

    // 4. Receive second delta.
    let frame = read_server_frame(&mut socket).await;
    adapter.apply_gateway_frame(&frame).await;
    model.apply_server_frame(&frame);

    assert_eq!(
        model.message_history.last(),
        Some(&ChatMessage::Assistant("hello".to_owned())),
        "assistant message should contain accumulated deltas"
    );

    // 5. Receive TurnCompleted.
    let frame = read_server_frame(&mut socket).await;
    adapter.apply_gateway_frame(&frame).await;
    model.apply_server_frame(&frame);

    let state = adapter.state_snapshot().await;
    assert!(
        state.active_turn_id.is_none(),
        "active turn should be cleared after completion"
    );
    assert_eq!(
        state.rendered_output, "hello",
        "rendered output should match final content"
    );

    // View model should have [User("say hello"), Assistant("hello")].
    assert_eq!(model.message_history.len(), 2);
    assert_eq!(
        model.message_history[0],
        ChatMessage::User("say hello".to_owned())
    );
    assert_eq!(
        model.message_history[1],
        ChatMessage::Assistant("hello".to_owned())
    );

    server_handle.abort();
}

#[tokio::test]
async fn reconnection_resumes_session_and_streaming() {
    // First connection: HelloAck + TurnStarted + first delta, then disconnect.
    let first_frames = vec![
        hello_ack_frame("runtime-alice", None),
        turn_started_frame("turn-1", "runtime-alice"),
        delta_frame("turn-1", "runtime-alice", "first"),
    ];

    // Second connection: HelloAck (with active turn) + resumed delta +
    // completion.
    let second_frames = vec![
        hello_ack_frame(
            "runtime-alice",
            Some(turn_status("turn-1", GatewayTurnState::Running)),
        ),
        delta_frame("turn-1", "runtime-alice", "second"),
        completed_frame("turn-1", "runtime-alice", "firstsecond"),
    ];

    let (address, server_handle) =
        spawn_reconnecting_mock_server(first_frames, second_frames).await;
    let adapter = TuiChannelAdapter::new("alice", "conn-1");
    let mut model = TuiViewModel::new();

    // ── First connection ──
    let mut socket = client_connect(address, &adapter).await;

    // HelloAck.
    let frame = read_server_frame(&mut socket).await;
    adapter.apply_gateway_frame(&frame).await;
    model.apply_server_frame(&frame);

    // TurnStarted.
    let frame = read_server_frame(&mut socket).await;
    adapter.apply_gateway_frame(&frame).await;
    model.apply_server_frame(&frame);

    // First delta.
    let frame = read_server_frame(&mut socket).await;
    adapter.apply_gateway_frame(&frame).await;
    model.apply_server_frame(&frame);

    let state = adapter.state_snapshot().await;
    assert_eq!(state.rendered_output, "first");
    assert_eq!(state.active_turn_id.as_deref(), Some("turn-1"));

    // Simulate disconnect.
    drop(socket);
    adapter.mark_disconnected().await;
    model.connection_state = ConnectionState::Disconnected {
        since: std::time::Instant::now(),
    };

    let state = adapter.state_snapshot().await;
    assert!(!state.connected, "should be disconnected");

    // Brief pause to allow server to accept second connection.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // ── Second connection (reconnect) ──
    let mut socket = client_connect(address, &adapter).await;
    model.connection_state = ConnectionState::Connected;

    // HelloAck with active turn.
    let frame = read_server_frame(&mut socket).await;
    adapter.apply_gateway_frame(&frame).await;
    model.apply_server_frame(&frame);

    let state = adapter.state_snapshot().await;
    assert!(state.connected, "should be reconnected");
    assert_eq!(
        state.active_turn_id.as_deref(),
        Some("turn-1"),
        "active turn should be restored on reconnect"
    );

    // Resumed delta.
    let frame = read_server_frame(&mut socket).await;
    adapter.apply_gateway_frame(&frame).await;
    model.apply_server_frame(&frame);

    // TurnCompleted.
    let frame = read_server_frame(&mut socket).await;
    adapter.apply_gateway_frame(&frame).await;
    model.apply_server_frame(&frame);

    let state = adapter.state_snapshot().await;
    assert!(
        state.active_turn_id.is_none(),
        "active turn should be cleared after completion"
    );
    assert_eq!(
        state.rendered_output, "firstsecond",
        "rendered output should accumulate across reconnection"
    );
    assert_eq!(
        state.session_id.as_deref(),
        Some("runtime-alice"),
        "session should be stable across reconnect"
    );

    server_handle.abort();
}
