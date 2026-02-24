use std::{
    collections::BTreeMap,
    env, fs,
    io::{Read, Write},
    net::TcpListener,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use provider::OpenAIProvider;
use runtime::{AgentRuntime, RuntimeLimits};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use tools::{ReadTool, ToolRegistry};
use types::{
    CatalogProvider, Context, Message, MessageRole, ModelCatalog, ModelDescriptor, ModelId,
    ProviderId,
};

#[tokio::test]
async fn openai_compatible_runtime_e2e_exposes_tools_and_executes_loop() {
    let (workspace, temp_path) = temp_file_path("runtime-e2e");
    fs::write(&temp_path, "tokio = \"1\"\nserde = \"1\"\n").expect("test file should be writable");
    let temp_path_str = temp_path.to_string_lossy().to_string();

    let first_response = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({
            "choices": [{
                "index": 0,
                "delta": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "file_read",
                            "arguments": format!(r#"{{"path":"{}"}}"#, temp_path_str)
                        }
                    }]
                },
                "finish_reason": null
            }]
        }),
        json!({
            "choices": [{
                "index": 0,
                "delta": {},
                "finish_reason": "tool_calls"
            }]
        }),
    );
    let second_response = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({
            "choices": [{
                "index": 0,
                "delta": {
                    "role": "assistant",
                    "content": "Summary complete"
                },
                "finish_reason": null
            }]
        }),
        json!({
            "choices": [{
                "index": 0,
                "delta": {},
                "finish_reason": "stop"
            }]
        }),
    );

    let captured_requests = Arc::new(Mutex::new(Vec::<String>::new()));
    let base_url = spawn_scripted_server(
        vec![
            ("200 OK", first_response, "text/event-stream"),
            ("200 OK", second_response, "text/event-stream"),
        ],
        Arc::clone(&captured_requests),
    );

    let provider = OpenAIProvider::new(
        ProviderId::from("openai"),
        ProviderId::from("openai"),
        "test-key".to_owned(),
        base_url,
        BTreeMap::new(),
        test_catalog(ModelId::from("gpt-4o-mini")),
    );

    let mut tools = ToolRegistry::default();
    let wasm_runner: std::sync::Arc<dyn tools::WasmToolRunner> = std::sync::Arc::new(
        tools::HostWasmToolRunner::for_bootstrap_workspace(&workspace),
    );
    tools.register("file_read", ReadTool::new(wasm_runner));
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
        "file_read"
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

    let _ = fs::remove_dir_all(&workspace);
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
    let (workspace, temp_path) = temp_file_path("runtime-live");
    fs::write(&temp_path, "RUNTIME_LIVE_SMOKE_MARKER\n").expect("test file should be writable");

    let provider = OpenAIProvider::new(
        ProviderId::from("openai"),
        ProviderId::from("openai"),
        api_key,
        "https://openrouter.ai/api".to_owned(),
        BTreeMap::new(),
        catalog,
    );

    let mut tools = ToolRegistry::default();
    let wasm_runner: std::sync::Arc<dyn tools::WasmToolRunner> = std::sync::Arc::new(
        tools::HostWasmToolRunner::for_bootstrap_workspace(&workspace),
    );
    tools.register("file_read", ReadTool::new(wasm_runner));
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
                "Use file_read for this exact path: {}. Then answer with the word VERIFIED followed by the first line of the file.",
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
    assert_eq!(first_tool_call.name, "file_read");
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

    let _ = fs::remove_dir_all(&workspace);
}

fn test_catalog(model: ModelId) -> ModelCatalog {
    let mut models = BTreeMap::new();
    models.insert(
        model.0.clone(),
        ModelDescriptor {
            id: model.0.clone(),
            name: "E2E model".to_owned(),
            family: None,
            attachment: false,
            reasoning: false,
            tool_call: true,
            interleaved: None,
            structured_output: false,
            temperature: false,
            knowledge: None,
            release_date: None,
            last_updated: None,
            modalities: Default::default(),
            open_weights: false,
            cost: Default::default(),
            limit: Default::default(),
        },
    );

    let mut providers = BTreeMap::new();
    providers.insert(
        "openai".to_owned(),
        CatalogProvider {
            id: "openai".to_owned(),
            name: "OpenAI".to_owned(),
            env: vec![],
            api: None,
            doc: None,
            models,
        },
    );

    ModelCatalog::new(providers)
}

/// Create a temporary workspace under `/tmp` with `shared/`, `tmp/`, `vault/`
/// subdirectories and return a file path inside `shared/`.
fn temp_file_path(label: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be monotonic")
        .as_nanos();
    let workspace =
        env::temp_dir().join(format!("oxydra-{label}-ws-{}-{unique}", std::process::id()));
    for dir in ["shared", "tmp", "vault"] {
        fs::create_dir_all(workspace.join(dir)).expect("workspace subdirectory should be created");
    }
    let file = workspace.join("shared").join(format!(
        "oxydra-{label}-{}-{unique}.txt",
        std::process::id()
    ));
    (workspace, file)
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
