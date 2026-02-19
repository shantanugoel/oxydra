use serde_json::json;
use types::{
    Context, Message, MessageRole, ModelId, ProviderError, ProviderId, Response, RuntimeError,
    ToolCall, ToolError, init_tracing,
};

#[test]
fn serde_round_trip_for_phase1_types() {
    let tool_call = ToolCall {
        id: "call_1".to_owned(),
        name: "read_file".to_owned(),
        arguments: json!({ "path": "Cargo.toml" }),
    };

    let message = Message {
        role: MessageRole::Assistant,
        content: Some("Reading workspace manifest".to_owned()),
        tool_calls: vec![tool_call.clone()],
        tool_call_id: None,
    };

    let context = Context {
        provider: ProviderId::from("openai"),
        model: ModelId::from("gpt-4o-mini"),
        messages: vec![message.clone()],
    };

    let response = Response {
        message,
        tool_calls: vec![tool_call],
        finish_reason: Some("stop".to_owned()),
    };

    let context_json = serde_json::to_string(&context).expect("context should serialize");
    let parsed_context: Context =
        serde_json::from_str(&context_json).expect("context should deserialize");
    assert_eq!(parsed_context, context);

    let response_json = serde_json::to_string(&response).expect("response should serialize");
    let parsed_response: Response =
        serde_json::from_str(&response_json).expect("response should deserialize");
    assert_eq!(parsed_response, response);
}

#[test]
fn runtime_error_composes_provider_and_tool_errors() {
    let provider_error = ProviderError::UnknownModel {
        model: ModelId::from("does-not-exist"),
    };
    let runtime_from_provider: RuntimeError = provider_error.into();
    assert!(matches!(runtime_from_provider, RuntimeError::Provider(_)));

    let tool_error = ToolError::InvalidArguments {
        tool: "read_file".to_owned(),
        message: "path is required".to_owned(),
    };
    let runtime_from_tool: RuntimeError = tool_error.into();
    assert!(matches!(runtime_from_tool, RuntimeError::Tool(_)));
}

#[test]
fn tracing_subscriber_initializes_and_emits() {
    init_tracing();
    let span = tracing::info_span!("phase1_tracing_smoke");
    let _guard = span.enter();
    tracing::info!("phase1 tracing smoke");
}
