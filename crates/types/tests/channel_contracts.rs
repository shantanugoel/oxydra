use serde_json::json;
use types::{
    GATEWAY_PROTOCOL_VERSION, GatewayAssistantDelta, GatewayCancelActiveTurn, GatewayClientFrame,
    GatewayClientHello, GatewayErrorFrame, GatewayHealthCheck, GatewayHealthStatus,
    GatewayHelloAck, GatewaySendTurn, GatewayServerFrame, GatewaySession, GatewayTurnCancelled,
    GatewayTurnCompleted, GatewayTurnProgress, GatewayTurnStarted, GatewayTurnState,
    GatewayTurnStatus, Message, MessageRole, Response, RuntimeProgressEvent, RuntimeProgressKind,
    SandboxTier, StartupStatusReport,
};

fn sample_session() -> GatewaySession {
    GatewaySession {
        user_id: "user-1".to_owned(),
        runtime_session_id: "session-1".to_owned(),
    }
}

fn sample_turn(turn_id: &str, state: GatewayTurnState) -> GatewayTurnStatus {
    GatewayTurnStatus {
        turn_id: turn_id.to_owned(),
        state,
    }
}

fn sample_response() -> Response {
    Response {
        message: Message {
            role: MessageRole::Assistant,
            content: Some("Done".to_owned()),
            tool_calls: vec![],
            tool_call_id: None,
        },
        tool_calls: vec![],
        finish_reason: Some("stop".to_owned()),
        usage: None,
    }
}

#[test]
fn gateway_client_frames_round_trip_through_serde() {
    let frames = vec![
        GatewayClientFrame::Hello(GatewayClientHello {
            request_id: "req-hello".to_owned(),
            protocol_version: GATEWAY_PROTOCOL_VERSION,
            user_id: "user-1".to_owned(),
            runtime_session_id: Some("session-1".to_owned()),
        }),
        GatewayClientFrame::SendTurn(GatewaySendTurn {
            request_id: "req-send".to_owned(),
            runtime_session_id: "session-1".to_owned(),
            turn_id: "turn-1".to_owned(),
            prompt: "Summarize README".to_owned(),
        }),
        GatewayClientFrame::CancelActiveTurn(GatewayCancelActiveTurn {
            request_id: "req-cancel".to_owned(),
            runtime_session_id: "session-1".to_owned(),
            turn_id: "turn-1".to_owned(),
        }),
        GatewayClientFrame::HealthCheck(GatewayHealthCheck {
            request_id: "req-health".to_owned(),
        }),
    ];

    for frame in frames {
        let encoded = serde_json::to_string(&frame).expect("client frame should serialize");
        let decoded: GatewayClientFrame =
            serde_json::from_str(&encoded).expect("client frame should deserialize");
        assert_eq!(decoded, frame);
    }
}

#[test]
fn gateway_server_frames_round_trip_through_serde() {
    let session = sample_session();
    let running_turn = sample_turn("turn-1", GatewayTurnState::Running);

    let frames = vec![
        GatewayServerFrame::HelloAck(GatewayHelloAck {
            request_id: "req-hello".to_owned(),
            protocol_version: GATEWAY_PROTOCOL_VERSION,
            session: session.clone(),
            active_turn: Some(running_turn.clone()),
        }),
        GatewayServerFrame::TurnStarted(GatewayTurnStarted {
            request_id: "req-send".to_owned(),
            session: session.clone(),
            turn: running_turn.clone(),
        }),
        GatewayServerFrame::AssistantDelta(GatewayAssistantDelta {
            request_id: "req-send".to_owned(),
            session: session.clone(),
            turn: running_turn.clone(),
            delta: "Partial output".to_owned(),
        }),
        GatewayServerFrame::TurnCompleted(GatewayTurnCompleted {
            request_id: "req-send".to_owned(),
            session: session.clone(),
            turn: sample_turn("turn-1", GatewayTurnState::Completed),
            response: sample_response(),
        }),
        GatewayServerFrame::TurnCancelled(GatewayTurnCancelled {
            request_id: "req-cancel".to_owned(),
            session: session.clone(),
            turn: sample_turn("turn-2", GatewayTurnState::Cancelled),
        }),
        GatewayServerFrame::Error(GatewayErrorFrame {
            request_id: Some("req-send".to_owned()),
            session: Some(session.clone()),
            turn: Some(sample_turn("turn-3", GatewayTurnState::Failed)),
            message: "provider timeout".to_owned(),
        }),
        GatewayServerFrame::HealthStatus(GatewayHealthStatus {
            request_id: "req-health".to_owned(),
            healthy: true,
            session: Some(session.clone()),
            active_turn: Some(running_turn.clone()),
            startup_status: Some(StartupStatusReport {
                sandbox_tier: SandboxTier::Container,
                sidecar_available: true,
                shell_available: true,
                browser_available: true,
                degraded_reasons: Vec::new(),
            }),
            message: Some("ok".to_owned()),
        }),
        GatewayServerFrame::TurnProgress(GatewayTurnProgress {
            request_id: "req-send".to_owned(),
            session: session.clone(),
            turn: running_turn.clone(),
            progress: RuntimeProgressEvent {
                kind: RuntimeProgressKind::ToolExecution {
                    tool_names: vec!["file_read".to_owned()],
                },
                message: "[2/8] Executing tools: file_read".to_owned(),
                turn: 2,
                max_turns: 8,
            },
        }),
    ];

    for frame in frames {
        let encoded = serde_json::to_string(&frame).expect("server frame should serialize");
        let decoded: GatewayServerFrame =
            serde_json::from_str(&encoded).expect("server frame should deserialize");
        assert_eq!(decoded, frame);
    }
}

#[test]
fn gateway_client_frames_reject_missing_required_fields() {
    let invalid = json!({
        "type": "hello",
        "payload": {
            "request_id": "req-hello",
            "user_id": "user-1"
        }
    });

    let error = serde_json::from_value::<GatewayClientFrame>(invalid)
        .expect_err("missing protocol_version should fail");
    assert!(
        error.to_string().contains("protocol_version"),
        "expected missing field error, got: {error}"
    );
}

#[test]
fn gateway_server_frames_reject_invalid_payloads() {
    let unknown_variant = json!({
        "type": "unknown",
        "payload": {}
    });
    assert!(serde_json::from_value::<GatewayServerFrame>(unknown_variant).is_err());

    let missing_required_field = json!({
        "type": "health_status",
        "payload": {
            "request_id": "req-health"
        }
    });
    let error = serde_json::from_value::<GatewayServerFrame>(missing_required_field)
        .expect_err("missing healthy should fail");
    assert!(
        error.to_string().contains("healthy"),
        "expected missing field error, got: {error}"
    );
}
