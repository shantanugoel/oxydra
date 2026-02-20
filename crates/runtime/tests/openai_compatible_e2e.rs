use std::{
    env, fs,
    io::{Read, Write},
    net::TcpListener,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use provider::{OpenAIConfig, OpenAIProvider};
use runtime::{AgentRuntime, RuntimeLimits};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use tools::{ReadTool, ToolRegistry};
use types::{
    Context, Message, MessageRole, ModelCatalog, ModelDescriptor, ModelId, ProviderCaps, ProviderId,
};

#[tokio::test]
async fn openai_compatible_runtime_e2e_exposes_tools_and_executes_loop() {
    let temp_path = temp_file_path("runtime-e2e");
    fs::write(&temp_path, "tokio = \"1\"\nserde = \"1\"\n").expect("test file should be writable");
    let temp_path_str = temp_path.to_string_lossy().to_string();

    let first_response = json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "read_file",
                        "arguments": format!(r#"{{"path":"{}"}}"#, temp_path_str)
                    }
                }]
            },
            "finish_reason": "tool_calls"
        }]
    })
    .to_string();
    let second_response = json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "Summary complete",
                "tool_calls": []
            },
            "finish_reason": "stop"
        }]
    })
    .to_string();

    let captured_requests = Arc::new(Mutex::new(Vec::<String>::new()));
    let base_url = spawn_scripted_server(
        vec![
            ("200 OK", first_response, "application/json"),
            ("200 OK", second_response, "application/json"),
        ],
        Arc::clone(&captured_requests),
    );

    let provider = OpenAIProvider::with_catalog(
        OpenAIConfig {
            api_key: Some("test-key".to_owned()),
            base_url,
        },
        test_catalog(ModelId::from("gpt-4o-mini"), false),
    )
    .expect("provider should initialize");

    let mut tools = ToolRegistry::default();
    tools.register("read_file", ReadTool);
    let runtime = AgentRuntime::new(
        Box::new(provider),
        tools,
        RuntimeLimits {
            turn_timeout: Duration::from_secs(5),
            max_turns: 4,
            max_cost: None,
            ..RuntimeLimits::default()
        },
    );

    let mut context = Context {
        provider: ProviderId::from("openai"),
        model: ModelId::from("gpt-4o-mini"),
        tools: vec![],
        messages: vec![Message {
            role: MessageRole::User,
            content: Some("Read the file and summarize it.".to_owned()),
            tool_calls: vec![],
            tool_call_id: None,
        }],
    };

    let response = runtime
        .run_session(&mut context, &CancellationToken::new())
        .await
        .expect("runtime e2e loop should complete");

    assert!(
        response.tool_calls.is_empty() && response.message.tool_calls.is_empty(),
        "final runtime response should represent the terminal assistant turn without tool calls"
    );
    assert_eq!(
        response.message.content.as_deref(),
        Some("Summary complete")
    );
    assert!(
        context.messages.iter().any(|message| {
            message.role == MessageRole::Tool
                && message
                    .content
                    .as_deref()
                    .is_some_and(|content| content.contains("tokio = \"1\""))
        }),
        "runtime should append tool output to context"
    );

    let requests = captured_requests
        .lock()
        .expect("request capture mutex should not be poisoned");
    assert_eq!(requests.len(), 2);
    let first_request_json: Value =
        serde_json::from_str(&requests[0]).expect("first request body should be valid JSON");
    assert_eq!(first_request_json["tools"][0]["type"], "function");
    assert_eq!(
        first_request_json["tools"][0]["function"]["name"],
        "read_file"
    );
    assert_eq!(first_request_json["tool_choice"], "auto");
    let second_request_json: Value =
        serde_json::from_str(&requests[1]).expect("second request body should be valid JSON");
    assert_eq!(second_request_json["messages"][2]["role"], "tool");
    assert_eq!(second_request_json["messages"][2]["tool_call_id"], "call_1");
    assert!(
        second_request_json["messages"][2]["content"]
            .as_str()
            .is_some_and(|content| content.contains("tokio = \"1\""))
    );

    let _ = fs::remove_file(&temp_path);
}

#[tokio::test]
#[ignore = "requires OPENROUTER_API_KEY and network access"]
async fn live_openrouter_tool_call_smoke() {
    let api_key = env::var("OPENROUTER_API_KEY")
        .or_else(|_| env::var("OPENAI_API_KEY"))
        .expect(
            "set OPENROUTER_API_KEY (or OPENAI_API_KEY) to run live_openrouter_tool_call_smoke",
        );
    let model =
        env::var("OPENROUTER_MODEL").unwrap_or_else(|_| "google/gemini-3-flash-preview".to_owned());
    let model_id = ModelId::from(model.clone());
    let catalog = ModelCatalog::from_pinned_snapshot()
        .expect("pinned model catalog should parse for live smoke");
    catalog
        .validate(&ProviderId::from("openai"), &model_id)
        .expect("OPENROUTER_MODEL must exist in pinned openai model catalog");
    let temp_path = temp_file_path("runtime-live");
    fs::write(&temp_path, "RUNTIME_LIVE_SMOKE_MARKER\n").expect("test file should be writable");

    let provider = OpenAIProvider::with_catalog(
        OpenAIConfig {
            api_key: Some(api_key),
            base_url: "https://openrouter.ai/api".to_owned(),
        },
        catalog,
    )
    .expect("provider should initialize");

    let mut tools = ToolRegistry::default();
    tools.register("read_file", ReadTool);
    let runtime = AgentRuntime::new(
        Box::new(provider),
        tools,
        RuntimeLimits {
            turn_timeout: Duration::from_secs(90),
            max_turns: 6,
            max_cost: Some(8_000.0),
            ..RuntimeLimits::default()
        },
    );

    let mut context = Context {
        provider: ProviderId::from("openai"),
        model: model_id,
        tools: vec![],
        messages: vec![Message {
            role: MessageRole::User,
            content: Some(format!(
                "Use read_file for this exact path: {}. Then answer with the word VERIFIED followed by the first line of the file.",
                temp_path.to_string_lossy()
            )),
            tool_calls: vec![],
            tool_call_id: None,
        }],
    };

    let response = runtime
        .run_session(&mut context, &CancellationToken::new())
        .await
        .expect("live runtime session should complete");

    println!("Response: {response:#?}");

    let assistant_tool_turn = context
        .messages
        .iter()
        .find(|message| message.role == MessageRole::Assistant && !message.tool_calls.is_empty())
        .expect("live run should include an assistant tool-call turn");
    let first_tool_call = assistant_tool_turn
        .tool_calls
        .first()
        .expect("assistant tool-call turn should include at least one tool call");
    assert_eq!(first_tool_call.name, "read_file");
    assert!(
        context.messages.iter().any(|message| {
            message.role == MessageRole::Tool
                && message.tool_call_id.as_deref() == Some(first_tool_call.id.as_str())
                && message
                    .content
                    .as_deref()
                    .is_some_and(|content| content.contains("RUNTIME_LIVE_SMOKE_MARKER"))
        }),
        "live run should include a linked tool result with file content"
    );
    assert!(
        response.tool_calls.is_empty() && response.message.tool_calls.is_empty(),
        "final runtime response should be the post-tool completion turn"
    );
    assert!(
        response
            .message
            .content
            .as_deref()
            .is_some_and(|content| content.contains("VERIFIED")),
        "live run should include a verification marker in final assistant content"
    );
    assert!(
        response
            .message
            .content
            .as_deref()
            .is_some_and(|content| !content.trim().is_empty()),
        "live run should produce assistant content"
    );

    let _ = fs::remove_file(&temp_path);
}

fn test_catalog(model: ModelId, supports_streaming: bool) -> ModelCatalog {
    ModelCatalog::new(vec![ModelDescriptor {
        provider: ProviderId::from("openai"),
        model,
        display_name: Some("E2E model".to_owned()),
        caps: ProviderCaps {
            supports_streaming,
            supports_tools: true,
            supports_json_mode: false,
            supports_reasoning_traces: false,
            max_input_tokens: None,
            max_output_tokens: None,
            max_context_tokens: None,
        },
        deprecated: false,
    }])
}

fn temp_file_path(label: &str) -> std::path::PathBuf {
    let mut path = env::temp_dir();
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be monotonic")
        .as_nanos();
    path.push(format!(
        "oxydra-{label}-{}-{unique}.txt",
        std::process::id()
    ));
    path
}

fn spawn_scripted_server(
    responses: Vec<(&'static str, String, &'static str)>,
    captured_requests: Arc<Mutex<Vec<String>>>,
) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("server should bind");
    let address = listener
        .local_addr()
        .expect("server should expose local address");

    let _server = std::thread::spawn(move || {
        for (status_line, body, content_type) in responses {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut request_buffer = [0_u8; 32_768];
                let read = stream.read(&mut request_buffer).unwrap_or(0);
                let request = String::from_utf8_lossy(&request_buffer[..read]).to_string();
                let body_only = request
                    .split_once("\r\n\r\n")
                    .map_or_else(String::new, |(_, body)| body.to_owned());
                captured_requests
                    .lock()
                    .expect("request capture mutex should not be poisoned")
                    .push(body_only);

                let response = format!(
                    "HTTP/1.1 {status_line}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        }
    });

    format!("http://{address}")
}
