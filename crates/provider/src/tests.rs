use std::{
    collections::VecDeque,
    env,
    io::{Read, Write},
    net::TcpListener,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use insta::assert_json_snapshot;
use serde_json::json;
use types::{
    ModelDescriptor, ModelId, Provider, ProviderCaps, StreamItem, ToolCallDelta, UsageUpdate,
};

use super::*;

#[test]
fn api_key_resolution_uses_expected_precedence() {
    let resolved = resolve_api_key_from_sources(
        Some("explicit".to_owned()),
        Some("provider".to_owned()),
        Some("fallback".to_owned()),
    );
    assert_eq!(resolved.as_deref(), Some("explicit"));

    let resolved = resolve_api_key_from_sources(
        None,
        Some("provider".to_owned()),
        Some("fallback".to_owned()),
    );
    assert_eq!(resolved.as_deref(), Some("provider"));

    let resolved = resolve_api_key_from_sources(None, None, Some("fallback".to_owned()));
    assert_eq!(resolved.as_deref(), Some("fallback"));

    let resolved = resolve_api_key_from_sources(
        Some("   ".to_owned()),
        Some("provider".to_owned()),
        Some("fallback".to_owned()),
    );
    assert_eq!(resolved.as_deref(), Some("provider"));

    let resolved = resolve_api_key_from_sources(None, None, None);
    assert!(resolved.is_none());
}

#[test]
fn default_openai_config_uses_openai_base_url() {
    assert_eq!(OpenAIConfig::default().base_url, OPENAI_DEFAULT_BASE_URL);
}

#[test]
fn default_anthropic_config_uses_anthropic_base_url() {
    let config = AnthropicConfig::default();
    assert_eq!(config.base_url, ANTHROPIC_DEFAULT_BASE_URL);
    assert_eq!(config.max_tokens, DEFAULT_ANTHROPIC_MAX_TOKENS);
}

#[test]
fn anthropic_api_key_resolution_uses_expected_precedence() {
    let resolved = resolve_api_key_from_sources(
        Some("explicit".to_owned()),
        Some("anthropic-env".to_owned()),
        Some("fallback".to_owned()),
    );
    assert_eq!(resolved.as_deref(), Some("explicit"));

    let resolved = resolve_api_key_from_sources(
        None,
        Some("anthropic-env".to_owned()),
        Some("fallback".to_owned()),
    );
    assert_eq!(resolved.as_deref(), Some("anthropic-env"));

    let resolved = resolve_api_key_from_sources(None, None, Some("fallback".to_owned()));
    assert_eq!(resolved.as_deref(), Some("fallback"));
}

#[test]
fn anthropic_request_normalization_snapshot_is_stable() {
    let context = Context {
        provider: ProviderId::from("anthropic"),
        model: ModelId::from("claude-3-5-sonnet-latest"),
        tools: vec![FunctionDecl::new(
            "file_read",
            Some("Read UTF-8 text from a file".to_owned()),
            JsonSchema::object(
                std::collections::BTreeMap::from([(
                    "path".to_owned(),
                    JsonSchema::new(types::JsonSchemaType::String),
                )]),
                vec!["path".to_owned()],
            ),
        )],
        messages: vec![
            Message {
                role: MessageRole::System,
                content: Some("You are concise".to_owned()),
                tool_calls: vec![],
                tool_call_id: None,
            },
            Message {
                role: MessageRole::User,
                content: Some("List project files".to_owned()),
                tool_calls: vec![],
                tool_call_id: None,
            },
            Message {
                role: MessageRole::Assistant,
                content: None,
                tool_calls: vec![ToolCall {
                    id: "call_1".to_owned(),
                    name: "file_read".to_owned(),
                    arguments: json!({"path": "Cargo.toml"}),
                }],
                tool_call_id: None,
            },
            Message {
                role: MessageRole::Tool,
                content: Some("{\"ok\":true}".to_owned()),
                tool_calls: vec![],
                tool_call_id: Some("call_1".to_owned()),
            },
        ],
    };
    let request = AnthropicMessagesRequest::from_context(&context, DEFAULT_ANTHROPIC_MAX_TOKENS)
        .expect("request should normalize");
    let request_json = serde_json::to_value(request).expect("request should serialize");
    assert_json_snapshot!(
        request_json,
        @r###"
        {
          "max_tokens": 1024,
          "messages": [
            {
              "content": [
                {
                  "text": "List project files",
                  "type": "text"
                }
              ],
              "role": "user"
            },
            {
              "content": [
                {
                  "id": "call_1",
                  "input": {
                    "path": "Cargo.toml"
                  },
                  "name": "file_read",
                  "type": "tool_use"
                }
              ],
              "role": "assistant"
            },
            {
              "content": [
                {
                  "content": "{\"ok\":true}",
                  "tool_use_id": "call_1",
                  "type": "tool_result"
                }
              ],
              "role": "user"
            }
          ],
          "model": "claude-3-5-sonnet-latest",
          "system": "You are concise",
          "tool_choice": {
            "type": "auto"
          },
          "tools": [
            {
              "description": "Read UTF-8 text from a file",
              "input_schema": {
                "additionalProperties": false,
                "properties": {
                  "path": {
                    "type": "string"
                  }
                },
                "required": [
                  "path"
                ],
                "type": "object"
              },
              "name": "file_read"
            }
          ]
        }
        "###
    );
}

#[test]
fn anthropic_streaming_request_includes_stream_field() {
    let context = Context {
        provider: ProviderId::from("anthropic"),
        model: ModelId::from("claude-3-5-sonnet-latest"),
        tools: vec![],
        messages: vec![Message {
            role: MessageRole::User,
            content: Some("Hello".to_owned()),
            tool_calls: vec![],
            tool_call_id: None,
        }],
    };
    let request = AnthropicMessagesRequest::from_context_with_stream(
        &context,
        DEFAULT_ANTHROPIC_MAX_TOKENS,
        true,
    )
    .expect("streaming request should normalize");
    let request_json = serde_json::to_value(&request).expect("request should serialize");
    assert_json_snapshot!(
        request_json,
        @r###"
        {
          "max_tokens": 1024,
          "messages": [
            {
              "content": [
                {
                  "text": "Hello",
                  "type": "text"
                }
              ],
              "role": "user"
            }
          ],
          "model": "claude-3-5-sonnet-latest",
          "stream": true
        }
        "###
    );

    // Non-streaming request should omit the stream field entirely (skip_serializing_if)
    let non_stream_request =
        AnthropicMessagesRequest::from_context(&context, DEFAULT_ANTHROPIC_MAX_TOKENS)
            .expect("request should normalize");
    let non_stream_json =
        serde_json::to_value(&non_stream_request).expect("request should serialize");
    assert!(
        non_stream_json.get("stream").is_none(),
        "stream field should be omitted when false"
    );
}

#[test]
fn request_normalization_maps_messages_and_tools() {
    let context = Context {
        provider: ProviderId::from("openai"),
        model: ModelId::from("gpt-4o-mini"),
        tools: vec![FunctionDecl::new(
            "file_read",
            Some("Read UTF-8 text from a file".to_owned()),
            JsonSchema::object(
                std::collections::BTreeMap::from([(
                    "path".to_owned(),
                    JsonSchema::new(types::JsonSchemaType::String),
                )]),
                vec!["path".to_owned()],
            ),
        )],
        messages: vec![
            Message {
                role: MessageRole::User,
                content: Some("List project files".to_owned()),
                tool_calls: vec![],
                tool_call_id: None,
            },
            Message {
                role: MessageRole::Assistant,
                content: None,
                tool_calls: vec![ToolCall {
                    id: "call_1".to_owned(),
                    name: "file_read".to_owned(),
                    arguments: json!({"path": "Cargo.toml"}),
                }],
                tool_call_id: None,
            },
        ],
    };

    let request =
        OpenAIChatCompletionRequest::from_context(&context).expect("request should normalize");
    let request_json = serde_json::to_value(request).expect("request should serialize");
    assert_eq!(request_json["model"], "gpt-4o-mini");
    assert_eq!(request_json["messages"][0]["role"], "user");
    assert_eq!(
        request_json["messages"][1]["tool_calls"][0]["function"]["name"],
        "file_read"
    );
    assert_eq!(request_json["tools"][0]["type"], "function");
    assert_eq!(request_json["tools"][0]["function"]["name"], "file_read");
    assert_eq!(request_json["tool_choice"], "auto");
    assert_eq!(
        request_json["messages"][1]["tool_calls"][0]["function"]["arguments"],
        "{\"path\":\"Cargo.toml\"}"
    );
}

#[test]
fn streaming_request_normalization_snapshot_is_stable() {
    let request = OpenAIChatCompletionRequest::from_stream_context(&test_context("gpt-4o-mini"))
        .expect("request should normalize");
    let request_json = serde_json::to_value(request).expect("request should serialize");

    assert_json_snapshot!(
        request_json,
        @r###"
        {
          "messages": [
            {
              "content": "Ping",
              "role": "user"
            }
          ],
          "model": "gpt-4o-mini",
          "stream": true,
          "stream_options": {
            "include_usage": true
          }
        }
        "###
    );
}

#[test]
fn streaming_request_normalization_enables_stream_and_usage() {
    let request = OpenAIChatCompletionRequest::from_stream_context(&test_context("gpt-4o-mini"))
        .expect("request should normalize");
    let request_json = serde_json::to_value(request).expect("request should serialize");

    assert_eq!(request_json["stream"], true);
    assert_eq!(request_json["stream_options"]["include_usage"], true);
}

#[test]
fn stream_payload_normalization_maps_text_tool_usage_and_finish_reason() {
    let payload = json!({
        "choices": [{
            "delta": {
                "content": "Done",
                "tool_calls": [{
                    "index": 0,
                    "id": "call_1",
                    "function": {
                        "name": "file_read",
                        "arguments": "{\"path\":\"Cargo.toml\"}"
                    }
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": {
            "prompt_tokens": 5,
            "completion_tokens": 2,
            "total_tokens": 7
        }
    })
    .to_string();

    let provider = ProviderId::from("openai");
    let mut accumulator = ToolCallAccumulator::default();
    let chunk = parse_openai_stream_payload(&payload, &provider)
        .expect("payload should parse")
        .expect("payload should not terminate stream");
    let items = normalize_openai_stream_chunk(chunk, &mut accumulator);

    assert_eq!(
        items,
        vec![
            StreamItem::UsageUpdate(UsageUpdate {
                prompt_tokens: Some(5),
                completion_tokens: Some(2),
                total_tokens: Some(7),
            }),
            StreamItem::Text("Done".to_owned()),
            StreamItem::ToolCallDelta(ToolCallDelta {
                index: 0,
                id: Some("call_1".to_owned()),
                name: Some("file_read".to_owned()),
                arguments: Some("{\"path\":\"Cargo.toml\"}".to_owned()),
            }),
            StreamItem::FinishReason("tool_calls".to_owned()),
        ]
    );
}

#[test]
fn tool_call_deltas_reassemble_arguments_across_payloads() {
    let first_payload = json!({
        "choices": [{
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": "call_1",
                    "function": {
                        "name": "file_read",
                        "arguments": "{\"path\":\"Car"
                    }
                }]
            }
        }]
    })
    .to_string();
    let second_payload = json!({
        "choices": [{
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "function": {
                        "arguments": "go.toml\"}"
                    }
                }]
            }
        }]
    })
    .to_string();

    let provider = ProviderId::from("openai");
    let mut accumulator = ToolCallAccumulator::default();
    let first_chunk = parse_openai_stream_payload(&first_payload, &provider)
        .expect("first payload should parse")
        .expect("first payload should not terminate stream");
    let first_items = normalize_openai_stream_chunk(first_chunk, &mut accumulator);
    assert!(matches!(
        first_items.first(),
        Some(StreamItem::ToolCallDelta(ToolCallDelta { arguments: Some(arguments), .. }))
            if arguments == "{\"path\":\"Car"
    ));

    let second_chunk = parse_openai_stream_payload(&second_payload, &provider)
        .expect("second payload should parse")
        .expect("second payload should not terminate stream");
    let second_items = normalize_openai_stream_chunk(second_chunk, &mut accumulator);
    let arguments = match second_items.first() {
        Some(StreamItem::ToolCallDelta(delta)) => delta
            .arguments
            .as_ref()
            .expect("reassembled arguments should be present"),
        _ => panic!("expected tool-call delta item"),
    };
    let parsed: serde_json::Value =
        serde_json::from_str(arguments).expect("reassembled arguments should be valid JSON");
    assert_eq!(parsed, json!({"path": "Cargo.toml"}));
}

#[test]
fn tool_call_deltas_reassemble_split_unicode_escape_sequences() {
    let first_payload = json!({
        "choices": [{
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": "call_1",
                    "function": {
                        "name": "echo",
                        "arguments": "{\"emoji\":\"\\uD83D"
                    }
                }]
            }
        }]
    })
    .to_string();
    let second_payload = json!({
        "choices": [{
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "function": {
                        "arguments": "\\uDE00\"}"
                    }
                }]
            }
        }]
    })
    .to_string();

    let provider = ProviderId::from("openai");
    let mut accumulator = ToolCallAccumulator::default();
    let first_chunk = parse_openai_stream_payload(&first_payload, &provider)
        .expect("first payload should parse")
        .expect("first payload should not terminate stream");
    let _ = normalize_openai_stream_chunk(first_chunk, &mut accumulator);
    let second_chunk = parse_openai_stream_payload(&second_payload, &provider)
        .expect("second payload should parse")
        .expect("second payload should not terminate stream");
    let second_items = normalize_openai_stream_chunk(second_chunk, &mut accumulator);
    let arguments = match second_items.first() {
        Some(StreamItem::ToolCallDelta(delta)) => delta
            .arguments
            .as_ref()
            .expect("reassembled arguments should be present"),
        _ => panic!("expected tool-call delta item"),
    };
    let parsed: serde_json::Value =
        serde_json::from_str(arguments).expect("reassembled arguments should be valid JSON");
    assert_eq!(parsed, json!({"emoji": "😀"}));
}

#[test]
fn sse_parser_handles_fragmented_frames() {
    let mut parser = SseDataParser::default();
    let payloads = parser
        .push_chunk(br#"data: {"choices":[{"delta":{"content":"Hel"#)
        .expect("first chunk should parse");
    assert!(payloads.is_empty());

    let payloads = parser
        .push_chunk(
            br#"lo"}}]}

data: [DONE]

"#,
        )
        .expect("second chunk should parse");
    assert_eq!(payloads.len(), 2);

    let provider = ProviderId::from("openai");
    let mut accumulator = ToolCallAccumulator::default();
    let chunk = parse_openai_stream_payload(&payloads[0], &provider)
        .expect("payload should parse")
        .expect("payload should not terminate stream");
    let items = normalize_openai_stream_chunk(chunk, &mut accumulator);
    assert_eq!(items, vec![StreamItem::Text("Hello".to_owned())]);

    let done =
        parse_openai_stream_payload(&payloads[1], &provider).expect("done payload should parse");
    assert!(done.is_none());
}

#[test]
fn stream_emits_connection_lost_when_done_sentinel_missing() {
    let base_url = spawn_one_shot_server(
        "200 OK",
        "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n",
        "text/event-stream",
    );
    let provider = OpenAIProvider::with_catalog(
        OpenAIConfig {
            api_key: Some("test-key".to_owned()),
            base_url,
        },
        test_model_catalog(),
    )
    .expect("provider should initialize");

    let items = run_stream_collect(&provider, &test_context("gpt-4o-mini"));
    assert!(
        items
            .iter()
            .any(|item| matches!(item, Ok(StreamItem::Text(text)) if text == "Hi"))
    );
    assert!(matches!(
        items.last(),
        Some(Ok(StreamItem::ConnectionLost(message))) if message.contains("[DONE]")
    ));
}

#[test]
fn stream_finishes_without_connection_lost_when_done_is_received() {
    let base_url = spawn_one_shot_server(
        "200 OK",
        "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\ndata: [DONE]\n\n",
        "text/event-stream",
    );
    let provider = OpenAIProvider::with_catalog(
        OpenAIConfig {
            api_key: Some("test-key".to_owned()),
            base_url,
        },
        test_model_catalog(),
    )
    .expect("provider should initialize");

    let items = run_stream_collect(&provider, &test_context("gpt-4o-mini"));
    assert!(items.iter().all(Result::is_ok));
    assert!(
        items
            .iter()
            .any(|item| matches!(item, Ok(StreamItem::Text(text)) if text == "Hi"))
    );
    assert!(
        !items
            .iter()
            .any(|item| matches!(item, Ok(StreamItem::ConnectionLost(_))))
    );
}

#[test]
fn response_normalization_maps_message_and_finish_reason() {
    let response = OpenAIChatCompletionResponse {
        choices: vec![OpenAIChoice {
            message: OpenAIChatMessageResponse {
                role: "assistant".to_owned(),
                content: Some("Done".to_owned()),
                tool_calls: vec![OpenAIResponseToolCall {
                    id: "call_1".to_owned(),
                    kind: "function".to_owned(),
                    function: OpenAIResponseFunction {
                        name: "file_read".to_owned(),
                        arguments: "{\"path\":\"Cargo.toml\"}".to_owned(),
                    },
                }],
            },
            finish_reason: Some("tool_calls".to_owned()),
        }],
        usage: Some(OpenAIUsage {
            prompt_tokens: Some(5),
            completion_tokens: Some(2),
            total_tokens: Some(7),
        }),
    };

    let normalized =
        normalize_openai_response(response, &ProviderId::from("openai")).expect("should parse");
    assert_eq!(normalized.message.role, MessageRole::Assistant);
    assert_eq!(normalized.finish_reason.as_deref(), Some("tool_calls"));
    assert_eq!(
        normalized.usage,
        Some(UsageUpdate {
            prompt_tokens: Some(5),
            completion_tokens: Some(2),
            total_tokens: Some(7),
        })
    );
    assert_eq!(normalized.tool_calls.len(), 1);
    assert_eq!(normalized.tool_calls[0].name, "file_read");
    assert_eq!(
        normalized.tool_calls[0].arguments,
        json!({"path": "Cargo.toml"})
    );
}

#[test]
fn unknown_models_are_rejected_before_network_request() {
    let provider = OpenAIProvider::with_catalog(
        OpenAIConfig {
            api_key: Some("test-key".to_owned()),
            base_url: "http://127.0.0.1:9".to_owned(),
        },
        test_model_catalog(),
    )
    .expect("provider should initialize");

    let context = test_context("unknown-model");
    let validation = run_complete(&provider, &context);
    assert!(matches!(
        validation,
        Err(ProviderError::UnknownModel { .. })
    ));
}

#[test]
fn transport_errors_are_mapped() {
    let provider = OpenAIProvider::with_catalog(
        OpenAIConfig {
            api_key: Some("test-key".to_owned()),
            base_url: "http://127.0.0.1:9".to_owned(),
        },
        test_model_catalog(),
    )
    .expect("provider should initialize");
    let completion = run_complete(&provider, &test_context("gpt-4o-mini"));
    assert!(matches!(completion, Err(ProviderError::Transport { .. })));
}

#[test]
fn http_status_errors_are_mapped() {
    let base_url = spawn_one_shot_server(
        "401 Unauthorized",
        r#"{"error":{"message":"invalid API key"}}"#,
        "application/json",
    );
    let provider = OpenAIProvider::with_catalog(
        OpenAIConfig {
            api_key: Some("test-key".to_owned()),
            base_url,
        },
        test_model_catalog(),
    )
    .expect("provider should initialize");
    let completion = run_complete(&provider, &test_context("gpt-4o-mini"));
    assert!(matches!(
        completion,
        Err(ProviderError::HttpStatus {
            status: 401,
            message,
            ..
        }) if message == "invalid API key"
    ));
}

#[test]
fn response_parse_errors_are_mapped() {
    let base_url = spawn_one_shot_server("200 OK", "not-json", "text/plain");
    let provider = OpenAIProvider::with_catalog(
        OpenAIConfig {
            api_key: Some("test-key".to_owned()),
            base_url,
        },
        test_model_catalog(),
    )
    .expect("provider should initialize");
    let completion = run_complete(&provider, &test_context("gpt-4o-mini"));
    assert!(matches!(
        completion,
        Err(ProviderError::ResponseParse { .. })
    ));
}

#[test]
fn anthropic_response_normalization_maps_text_and_tool_use() {
    let response = AnthropicMessagesResponse {
        role: "assistant".to_owned(),
        content: vec![
            AnthropicResponseContentBlock {
                kind: "text".to_owned(),
                text: Some("Done".to_owned()),
                id: None,
                name: None,
                input: None,
            },
            AnthropicResponseContentBlock {
                kind: "tool_use".to_owned(),
                text: None,
                id: Some("call_1".to_owned()),
                name: Some("file_read".to_owned()),
                input: Some(json!({"path":"Cargo.toml"})),
            },
        ],
        stop_reason: Some("tool_use".to_owned()),
        usage: Some(AnthropicUsage {
            input_tokens: Some(5),
            output_tokens: Some(2),
        }),
    };

    let normalized = normalize_anthropic_response(response, &ProviderId::from("anthropic"))
        .expect("should parse");
    assert_eq!(normalized.message.role, MessageRole::Assistant);
    assert_eq!(normalized.message.content.as_deref(), Some("Done"));
    assert_eq!(normalized.finish_reason.as_deref(), Some("tool_use"));
    assert_eq!(
        normalized.usage,
        Some(UsageUpdate {
            prompt_tokens: Some(5),
            completion_tokens: Some(2),
            total_tokens: Some(7),
        })
    );
    assert_eq!(normalized.tool_calls.len(), 1);
    assert_eq!(normalized.tool_calls[0].id, "call_1");
    assert_eq!(normalized.tool_calls[0].name, "file_read");
    assert_eq!(
        normalized.tool_calls[0].arguments,
        json!({"path": "Cargo.toml"})
    );
}

#[test]
fn anthropic_unknown_models_are_rejected_before_network_request() {
    let provider = AnthropicProvider::with_catalog(
        AnthropicConfig {
            api_key: Some("test-key".to_owned()),
            base_url: "http://127.0.0.1:9".to_owned(),
            max_tokens: 64,
        },
        test_model_catalog_for("anthropic", "claude-3-5-sonnet-latest", "Claude 3.5 Sonnet"),
    )
    .expect("provider should initialize");

    let context = test_context_for("anthropic", "unknown-model", "Ping");
    let validation = run_complete_anthropic(&provider, &context);
    assert!(matches!(
        validation,
        Err(ProviderError::UnknownModel { .. })
    ));
}

#[test]
fn anthropic_transport_errors_are_mapped() {
    let provider = AnthropicProvider::with_catalog(
        AnthropicConfig {
            api_key: Some("test-key".to_owned()),
            base_url: "http://127.0.0.1:9".to_owned(),
            max_tokens: 64,
        },
        test_model_catalog_for("anthropic", "claude-3-5-sonnet-latest", "Claude 3.5 Sonnet"),
    )
    .expect("provider should initialize");
    let completion = run_complete_anthropic(
        &provider,
        &test_context_for("anthropic", "claude-3-5-sonnet-latest", "Ping"),
    );
    assert!(matches!(completion, Err(ProviderError::Transport { .. })));
}

#[test]
fn anthropic_http_status_errors_are_mapped() {
    let base_url = spawn_one_shot_server(
        "429 Too Many Requests",
        r#"{"type":"error","error":{"type":"rate_limit_error","message":"rate limited"}}"#,
        "application/json",
    );
    let provider = AnthropicProvider::with_catalog(
        AnthropicConfig {
            api_key: Some("test-key".to_owned()),
            base_url,
            max_tokens: 64,
        },
        test_model_catalog_for("anthropic", "claude-3-5-sonnet-latest", "Claude 3.5 Sonnet"),
    )
    .expect("provider should initialize");
    let completion = run_complete_anthropic(
        &provider,
        &test_context_for("anthropic", "claude-3-5-sonnet-latest", "Ping"),
    );
    assert!(matches!(
        completion,
        Err(ProviderError::HttpStatus {
            status: 429,
            message,
            ..
        }) if message == "rate limited"
    ));
}

#[test]
fn anthropic_response_parse_errors_are_mapped() {
    let base_url = spawn_one_shot_server("200 OK", "not-json", "text/plain");
    let provider = AnthropicProvider::with_catalog(
        AnthropicConfig {
            api_key: Some("test-key".to_owned()),
            base_url,
            max_tokens: 64,
        },
        test_model_catalog_for("anthropic", "claude-3-5-sonnet-latest", "Claude 3.5 Sonnet"),
    )
    .expect("provider should initialize");
    let completion = run_complete_anthropic(
        &provider,
        &test_context_for("anthropic", "claude-3-5-sonnet-latest", "Ping"),
    );
    assert!(matches!(
        completion,
        Err(ProviderError::ResponseParse { .. })
    ));
}

#[test]
fn reliable_provider_retries_transport_failures_then_succeeds() {
    let provider = Arc::new(SequencedProvider::new(
        "openai",
        "gpt-4o-mini",
        vec![
            Err(ProviderError::Transport {
                provider: ProviderId::from("openai"),
                message: "timeout".to_owned(),
            }),
            Ok(sample_response("Recovered")),
        ],
    ));
    let reliable = ReliableProvider::from_arc(
        provider.clone(),
        RetryPolicy {
            max_attempts: 3,
            backoff_base: Duration::from_millis(1),
            backoff_max: Duration::from_millis(1),
        },
    );

    let response =
        run_complete_with(&reliable, &test_context("gpt-4o-mini")).expect("retry should recover");
    assert_eq!(response.message.content.as_deref(), Some("Recovered"));
    assert_eq!(provider.complete_call_count(), 2);
}

#[test]
fn reliable_provider_does_not_retry_non_retriable_errors() {
    let provider = Arc::new(SequencedProvider::new(
        "openai",
        "gpt-4o-mini",
        vec![
            Err(ProviderError::ResponseParse {
                provider: ProviderId::from("openai"),
                message: "schema mismatch".to_owned(),
            }),
            Ok(sample_response("ignored")),
        ],
    ));
    let reliable = ReliableProvider::from_arc(
        provider.clone(),
        RetryPolicy {
            max_attempts: 3,
            backoff_base: Duration::from_millis(1),
            backoff_max: Duration::from_millis(1),
        },
    );

    let error = run_complete_with(&reliable, &test_context("gpt-4o-mini"))
        .expect_err("non-retriable error should be surfaced");
    assert!(matches!(error, ProviderError::ResponseParse { .. }));
    assert_eq!(provider.complete_call_count(), 1);
}

#[test]
fn reliable_provider_enforces_max_attempts() {
    let provider = Arc::new(SequencedProvider::new(
        "openai",
        "gpt-4o-mini",
        vec![
            Err(ProviderError::Transport {
                provider: ProviderId::from("openai"),
                message: "timeout-1".to_owned(),
            }),
            Err(ProviderError::Transport {
                provider: ProviderId::from("openai"),
                message: "timeout-2".to_owned(),
            }),
            Ok(sample_response("should not be reached")),
        ],
    ));
    let reliable = ReliableProvider::from_arc(
        provider.clone(),
        RetryPolicy {
            max_attempts: 2,
            backoff_base: Duration::from_millis(1),
            backoff_max: Duration::from_millis(1),
        },
    );

    let error = run_complete_with(&reliable, &test_context("gpt-4o-mini"))
        .expect_err("max attempts should stop retries");
    assert!(matches!(error, ProviderError::Transport { .. }));
    assert_eq!(provider.complete_call_count(), 2);
}

#[test]
#[ignore = "requires OPENAI_API_KEY and network access"]
fn live_openai_smoke_normalizes_response() {
    let _api_key = env::var("OPENAI_API_KEY")
        .expect("set OPENAI_API_KEY to run live_openai_smoke_normalizes_response");
    let provider = OpenAIProvider::new(OpenAIConfig::default())
        .expect("provider should initialize from OPENAI_API_KEY");
    let context = test_context_with_prompt("gpt-4o-mini", "Reply with exactly: PONG");
    let response = run_complete(&provider, &context).expect("live completion should succeed");

    assert_eq!(response.message.role, MessageRole::Assistant);
    assert!(
        response
            .message
            .content
            .as_deref()
            .is_some_and(|content| !content.trim().is_empty())
            || !response.tool_calls.is_empty(),
        "live response should include content or tool calls"
    );
}

#[test]
#[ignore = "requires ANTHROPIC_API_KEY and network access"]
fn live_anthropic_stream_smoke_emits_text_or_tool_calls() {
    let _api_key = env::var("ANTHROPIC_API_KEY").expect(
        "set ANTHROPIC_API_KEY to run live_anthropic_stream_smoke_emits_text_or_tool_calls",
    );
    let provider = AnthropicProvider::new(AnthropicConfig::default())
        .expect("provider should initialize from ANTHROPIC_API_KEY");
    let context = test_context_for(
        "anthropic",
        "claude-3-5-sonnet-latest",
        "Reply with exactly: PONG",
    );
    let items = run_stream_collect_anthropic(&provider, &context);

    assert!(
        items.iter().any(|item| {
            matches!(
                item,
                Ok(StreamItem::Text(text)) if !text.trim().is_empty()
            )
        }) || items
            .iter()
            .any(|item| matches!(item, Ok(StreamItem::ToolCallDelta(_)))),
        "live stream should include text or tool call deltas"
    );
}

struct SequencedProvider {
    provider_id: ProviderId,
    model_catalog: ModelCatalog,
    complete_steps: Mutex<VecDeque<Result<Response, ProviderError>>>,
    complete_calls: AtomicUsize,
}

impl SequencedProvider {
    fn new(
        provider_id: &str,
        model_id: &str,
        complete_steps: Vec<Result<Response, ProviderError>>,
    ) -> Self {
        Self {
            provider_id: ProviderId::from(provider_id),
            model_catalog: test_model_catalog_for(provider_id, model_id, model_id),
            complete_steps: Mutex::new(VecDeque::from(complete_steps)),
            complete_calls: AtomicUsize::new(0),
        }
    }

    fn complete_call_count(&self) -> usize {
        self.complete_calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl Provider for SequencedProvider {
    fn provider_id(&self) -> &ProviderId {
        &self.provider_id
    }

    fn model_catalog(&self) -> &ModelCatalog {
        &self.model_catalog
    }

    async fn complete(&self, _context: &Context) -> Result<Response, ProviderError> {
        self.complete_calls.fetch_add(1, Ordering::SeqCst);
        let mut steps = self
            .complete_steps
            .lock()
            .expect("sequenced provider complete steps mutex should not be poisoned");
        steps.pop_front().unwrap_or_else(|| {
            Err(ProviderError::RequestFailed {
                provider: self.provider_id.clone(),
                message: "sequenced provider had no remaining completion steps".to_owned(),
            })
        })
    }

    async fn stream(
        &self,
        _context: &Context,
        _buffer_size: usize,
    ) -> Result<types::ProviderStream, ProviderError> {
        Err(ProviderError::RequestFailed {
            provider: self.provider_id.clone(),
            message: "sequenced provider stream is not configured".to_owned(),
        })
    }
}

fn sample_response(content: &str) -> Response {
    Response {
        message: Message {
            role: MessageRole::Assistant,
            content: Some(content.to_owned()),
            tool_calls: vec![],
            tool_call_id: None,
        },
        tool_calls: vec![],
        finish_reason: Some("stop".to_owned()),
        usage: None,
    }
}

fn test_model_catalog() -> ModelCatalog {
    test_model_catalog_for("openai", "gpt-4o-mini", "GPT-4o mini")
}

fn test_model_catalog_for(provider: &str, model: &str, display_name: &str) -> ModelCatalog {
    ModelCatalog::new(vec![ModelDescriptor {
        provider: ProviderId::from(provider),
        model: ModelId::from(model),
        display_name: Some(display_name.to_owned()),
        caps: ProviderCaps::default(),
        deprecated: false,
    }])
}

fn test_context(model: &str) -> Context {
    test_context_with_prompt(model, "Ping")
}

fn test_context_with_prompt(model: &str, prompt: &str) -> Context {
    test_context_for("openai", model, prompt)
}

fn test_context_for(provider: &str, model: &str, prompt: &str) -> Context {
    Context {
        provider: ProviderId::from(provider),
        model: ModelId::from(model),
        tools: vec![],
        messages: vec![Message {
            role: MessageRole::User,
            content: Some(prompt.to_owned()),
            tool_calls: vec![],
            tool_call_id: None,
        }],
    }
}

fn run_complete(provider: &OpenAIProvider, context: &Context) -> Result<Response, ProviderError> {
    run_complete_with(provider, context)
}

fn run_complete_anthropic(
    provider: &AnthropicProvider,
    context: &Context,
) -> Result<Response, ProviderError> {
    run_complete_with(provider, context)
}

fn run_complete_with<P: Provider>(
    provider: &P,
    context: &Context,
) -> Result<Response, ProviderError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime should build")
        .block_on(provider.complete(context))
}

fn run_stream_collect(
    provider: &OpenAIProvider,
    context: &Context,
) -> Vec<Result<StreamItem, ProviderError>> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime should build")
        .block_on(async {
            let mut stream = provider
                .stream(context, DEFAULT_STREAM_BUFFER_SIZE)
                .await
                .expect("stream should start");
            let mut items = Vec::new();
            while let Some(item) = stream.recv().await {
                items.push(item);
            }
            items
        })
}

// ---------------------------------------------------------------------------
// Anthropic SSE stream payload parsing tests
// ---------------------------------------------------------------------------

fn anthropic_provider_id() -> ProviderId {
    ProviderId::from("anthropic")
}

/// Helper to extract items from an `AnthropicEventAction::Items` variant.
fn expect_items(action: AnthropicEventAction) -> Vec<StreamItem> {
    match action {
        AnthropicEventAction::Items(items) => items,
        other => panic!("expected Items, got: {other:?}"),
    }
}

/// Helper to extract a `ProviderError` from an `AnthropicEventAction::Error`.
fn expect_error(action: AnthropicEventAction) -> ProviderError {
    match action {
        AnthropicEventAction::Error(err) => err,
        other => panic!("expected Error, got: {other:?}"),
    }
}

#[test]
fn anthropic_stream_text_delta_emits_text() {
    let payload =
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#;
    let provider = anthropic_provider_id();
    let mut acc = AnthropicToolCallAccumulator::default();
    let mut input_tokens = None;

    let items = expect_items(parse_anthropic_stream_payload(
        payload,
        &provider,
        &mut acc,
        &mut input_tokens,
    ));
    assert_eq!(items, vec![StreamItem::Text("Hello".to_owned())]);
}

#[test]
fn anthropic_stream_tool_call_accumulation() {
    let provider = anthropic_provider_id();
    let mut acc = AnthropicToolCallAccumulator::default();
    let mut input_tokens = None;

    // Start tool_use block at content block index 1.
    let start_payload = r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"get_weather"}}"#;
    let items = expect_items(parse_anthropic_stream_payload(
        start_payload,
        &provider,
        &mut acc,
        &mut input_tokens,
    ));
    assert_eq!(
        items,
        vec![StreamItem::ToolCallDelta(ToolCallDelta {
            index: 0,
            id: Some("toolu_1".to_owned()),
            name: Some("get_weather".to_owned()),
            arguments: None,
        })]
    );

    // First argument fragment.
    let delta1 = r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"loc"}}"#;
    let items = expect_items(parse_anthropic_stream_payload(
        delta1,
        &provider,
        &mut acc,
        &mut input_tokens,
    ));
    assert_eq!(
        items,
        vec![StreamItem::ToolCallDelta(ToolCallDelta {
            index: 0,
            id: Some("toolu_1".to_owned()),
            name: Some("get_weather".to_owned()),
            arguments: Some(r#"{"loc"#.to_owned()),
        })]
    );

    // Second argument fragment — arguments accumulate.
    let delta2 = r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"ation\":\"NYC\"}"}}"#;
    let items = expect_items(parse_anthropic_stream_payload(
        delta2,
        &provider,
        &mut acc,
        &mut input_tokens,
    ));
    assert_eq!(
        items,
        vec![StreamItem::ToolCallDelta(ToolCallDelta {
            index: 0,
            id: Some("toolu_1".to_owned()),
            name: Some("get_weather".to_owned()),
            arguments: Some(r#"{"location":"NYC"}"#.to_owned()),
        })]
    );
}

#[test]
fn anthropic_stream_tool_call_index_remapping() {
    // Simulates: text@0, tool_use@1, text@2, tool_use@3
    let provider = anthropic_provider_id();
    let mut acc = AnthropicToolCallAccumulator::default();
    let mut input_tokens = None;

    // text block at index 0 — no emission
    let text_start =
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#;
    let items = expect_items(parse_anthropic_stream_payload(
        text_start,
        &provider,
        &mut acc,
        &mut input_tokens,
    ));
    assert!(items.is_empty());

    // tool_use at index 1 → ordinal 0
    let tool1 = r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"t1","name":"fn1"}}"#;
    let items = expect_items(parse_anthropic_stream_payload(
        tool1,
        &provider,
        &mut acc,
        &mut input_tokens,
    ));
    assert_eq!(
        items[0],
        StreamItem::ToolCallDelta(ToolCallDelta {
            index: 0,
            id: Some("t1".to_owned()),
            name: Some("fn1".to_owned()),
            arguments: None,
        })
    );

    // tool_use at index 3 → ordinal 1
    let tool2 = r#"{"type":"content_block_start","index":3,"content_block":{"type":"tool_use","id":"t2","name":"fn2"}}"#;
    let items = expect_items(parse_anthropic_stream_payload(
        tool2,
        &provider,
        &mut acc,
        &mut input_tokens,
    ));
    assert_eq!(
        items[0],
        StreamItem::ToolCallDelta(ToolCallDelta {
            index: 1,
            id: Some("t2".to_owned()),
            name: Some("fn2".to_owned()),
            arguments: None,
        })
    );
}

#[test]
fn anthropic_stream_multiple_tools_interleaved_with_text() {
    let provider = anthropic_provider_id();
    let mut acc = AnthropicToolCallAccumulator::default();
    let mut input_tokens = None;

    // tool_use@0
    let _ = parse_anthropic_stream_payload(
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"a","name":"read"}}"#,
        &provider,
        &mut acc,
        &mut input_tokens,
    );
    // text@1
    let _ = parse_anthropic_stream_payload(
        r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
        &provider,
        &mut acc,
        &mut input_tokens,
    );
    // tool_use@2
    let _ = parse_anthropic_stream_payload(
        r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"b","name":"write"}}"#,
        &provider,
        &mut acc,
        &mut input_tokens,
    );

    // Arguments for first tool.
    let items = expect_items(parse_anthropic_stream_payload(
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"p\":1}"}}"#,
        &provider,
        &mut acc,
        &mut input_tokens,
    ));
    assert_eq!(
        items[0],
        StreamItem::ToolCallDelta(ToolCallDelta {
            index: 0,
            id: Some("a".to_owned()),
            name: Some("read".to_owned()),
            arguments: Some(r#"{"p":1}"#.to_owned()),
        })
    );

    // Arguments for second tool.
    let items = expect_items(parse_anthropic_stream_payload(
        r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"q\":2}"}}"#,
        &provider,
        &mut acc,
        &mut input_tokens,
    ));
    assert_eq!(
        items[0],
        StreamItem::ToolCallDelta(ToolCallDelta {
            index: 1,
            id: Some("b".to_owned()),
            name: Some("write".to_owned()),
            arguments: Some(r#"{"q":2}"#.to_owned()),
        })
    );
}

#[test]
fn anthropic_stream_prompt_tokens_emitted_at_message_start() {
    let payload = r#"{"type":"message_start","message":{"usage":{"input_tokens":42}}}"#;
    let provider = anthropic_provider_id();
    let mut acc = AnthropicToolCallAccumulator::default();
    let mut input_tokens = None;

    let items = expect_items(parse_anthropic_stream_payload(
        payload,
        &provider,
        &mut acc,
        &mut input_tokens,
    ));
    assert_eq!(
        items,
        vec![StreamItem::UsageUpdate(UsageUpdate {
            prompt_tokens: Some(42),
            completion_tokens: None,
            total_tokens: None,
        })]
    );
    assert_eq!(input_tokens, Some(42));
}

#[test]
fn anthropic_stream_completion_tokens_combined_at_message_delta() {
    let provider = anthropic_provider_id();
    let mut acc = AnthropicToolCallAccumulator::default();
    let mut input_tokens = None;

    // message_start with input_tokens.
    let _ = parse_anthropic_stream_payload(
        r#"{"type":"message_start","message":{"usage":{"input_tokens":10}}}"#,
        &provider,
        &mut acc,
        &mut input_tokens,
    );
    assert_eq!(input_tokens, Some(10));

    // message_delta with output_tokens.
    let delta_payload = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}"#;
    let items = expect_items(parse_anthropic_stream_payload(
        delta_payload,
        &provider,
        &mut acc,
        &mut input_tokens,
    ));

    assert!(items.contains(&StreamItem::FinishReason("end_turn".to_owned())));
    assert!(items.contains(&StreamItem::UsageUpdate(UsageUpdate {
        prompt_tokens: Some(10),
        completion_tokens: Some(5),
        total_tokens: Some(15),
    })));
}

#[test]
fn anthropic_stream_finish_reason_from_message_delta() {
    let provider = anthropic_provider_id();
    let mut acc = AnthropicToolCallAccumulator::default();
    let mut input_tokens = None;

    // With stop_reason.
    let payload = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}"#;
    let items = expect_items(parse_anthropic_stream_payload(
        payload,
        &provider,
        &mut acc,
        &mut input_tokens,
    ));
    assert!(items.contains(&StreamItem::FinishReason("end_turn".to_owned())));

    // Null stop_reason should NOT emit FinishReason.
    let payload_null =
        r#"{"type":"message_delta","delta":{"stop_reason":null},"usage":{"output_tokens":3}}"#;
    let items = expect_items(parse_anthropic_stream_payload(
        payload_null,
        &provider,
        &mut acc,
        &mut input_tokens,
    ));
    assert!(
        !items
            .iter()
            .any(|item| matches!(item, StreamItem::FinishReason(_)))
    );
}

#[test]
fn anthropic_stream_error_event_produces_terminal_error() {
    let payload = r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;
    let provider = anthropic_provider_id();
    let mut acc = AnthropicToolCallAccumulator::default();
    let mut input_tokens = None;

    let action = parse_anthropic_stream_payload(payload, &provider, &mut acc, &mut input_tokens);
    let err = expect_error(action);
    assert!(matches!(err, ProviderError::ResponseParse { message, .. } if message == "Overloaded"));
}

#[test]
fn anthropic_stream_thinking_delta_emits_reasoning() {
    let provider = anthropic_provider_id();
    let mut acc = AnthropicToolCallAccumulator::default();
    let mut input_tokens = None;

    // thinking block start — no emission.
    let start = r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking"}}"#;
    let items = expect_items(parse_anthropic_stream_payload(
        start,
        &provider,
        &mut acc,
        &mut input_tokens,
    ));
    assert!(items.is_empty());

    // thinking delta.
    let delta = r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Let me think..."}}"#;
    let items = expect_items(parse_anthropic_stream_payload(
        delta,
        &provider,
        &mut acc,
        &mut input_tokens,
    ));
    assert_eq!(
        items,
        vec![StreamItem::ReasoningDelta("Let me think...".to_owned())]
    );
}

#[test]
fn anthropic_stream_message_stop_without_prior_message_delta() {
    let provider = anthropic_provider_id();
    let mut acc = AnthropicToolCallAccumulator::default();
    let mut input_tokens = None;

    // message_start
    let _ = parse_anthropic_stream_payload(
        r#"{"type":"message_start","message":{"usage":{"input_tokens":5}}}"#,
        &provider,
        &mut acc,
        &mut input_tokens,
    );

    // content_block_stop (skip message_delta entirely)
    let _ = parse_anthropic_stream_payload(
        r#"{"type":"content_block_stop","index":0}"#,
        &provider,
        &mut acc,
        &mut input_tokens,
    );

    // message_stop — should terminate cleanly.
    let action = parse_anthropic_stream_payload(
        r#"{"type":"message_stop"}"#,
        &provider,
        &mut acc,
        &mut input_tokens,
    );
    assert!(matches!(action, AnthropicEventAction::Done));
}

#[test]
fn anthropic_stream_ping_is_ignored() {
    let provider = anthropic_provider_id();
    let mut acc = AnthropicToolCallAccumulator::default();
    let mut input_tokens = None;

    let items = expect_items(parse_anthropic_stream_payload(
        r#"{"type":"ping"}"#,
        &provider,
        &mut acc,
        &mut input_tokens,
    ));
    assert!(items.is_empty());
}

#[test]
fn anthropic_stream_unknown_event_type_is_ignored() {
    let provider = anthropic_provider_id();
    let mut acc = AnthropicToolCallAccumulator::default();
    let mut input_tokens = None;

    let items = expect_items(parse_anthropic_stream_payload(
        r#"{"type":"future_event","data":"whatever"}"#,
        &provider,
        &mut acc,
        &mut input_tokens,
    ));
    assert!(items.is_empty());
}

#[test]
fn anthropic_stream_unknown_delta_type_is_ignored() {
    let provider = anthropic_provider_id();
    let mut acc = AnthropicToolCallAccumulator::default();
    let mut input_tokens = None;

    let items = expect_items(parse_anthropic_stream_payload(
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"future_delta","data":"x"}}"#,
        &provider,
        &mut acc,
        &mut input_tokens,
    ));
    assert!(items.is_empty());
}

#[test]
fn anthropic_stream_empty_payload_returns_empty_items() {
    let provider = anthropic_provider_id();
    let mut acc = AnthropicToolCallAccumulator::default();
    let mut input_tokens = None;

    let items = expect_items(parse_anthropic_stream_payload(
        "  ",
        &provider,
        &mut acc,
        &mut input_tokens,
    ));
    assert!(items.is_empty());
}

#[test]
fn anthropic_stream_invalid_json_returns_error() {
    let provider = anthropic_provider_id();
    let mut acc = AnthropicToolCallAccumulator::default();
    let mut input_tokens = None;

    let action =
        parse_anthropic_stream_payload("not-valid-json", &provider, &mut acc, &mut input_tokens);
    let err = expect_error(action);
    assert!(matches!(err, ProviderError::ResponseParse { .. }));
}

#[test]
fn anthropic_stream_error_without_message_uses_default() {
    let payload = r#"{"type":"error","error":{"type":"server_error"}}"#;
    let provider = anthropic_provider_id();
    let mut acc = AnthropicToolCallAccumulator::default();
    let mut input_tokens = None;

    let err = expect_error(parse_anthropic_stream_payload(
        payload,
        &provider,
        &mut acc,
        &mut input_tokens,
    ));
    assert!(
        matches!(err, ProviderError::ResponseParse { message, .. } if message.contains("unknown streaming error"))
    );
}

#[test]
fn anthropic_stream_message_start_without_usage_emits_nothing() {
    let provider = anthropic_provider_id();
    let mut acc = AnthropicToolCallAccumulator::default();
    let mut input_tokens = None;

    let items = expect_items(parse_anthropic_stream_payload(
        r#"{"type":"message_start","message":{}}"#,
        &provider,
        &mut acc,
        &mut input_tokens,
    ));
    assert!(items.is_empty());
    assert_eq!(input_tokens, None);
}

#[test]
fn anthropic_stream_empty_text_delta_not_emitted() {
    let provider = anthropic_provider_id();
    let mut acc = AnthropicToolCallAccumulator::default();
    let mut input_tokens = None;

    let items = expect_items(parse_anthropic_stream_payload(
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":""}}"#,
        &provider,
        &mut acc,
        &mut input_tokens,
    ));
    assert!(items.is_empty());
}

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

fn spawn_one_shot_server(status_line: &str, body: &str, content_type: &str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("server should bind");
    let address = listener
        .local_addr()
        .expect("server should expose a local address");
    let status_line = status_line.to_owned();
    let body = body.to_owned();
    let content_type = content_type.to_owned();
    let _server = std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut request_buffer = [0_u8; 8_192];
            let _ = stream.read(&mut request_buffer);
            let response = format!(
                "HTTP/1.1 {status_line}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    format!("http://{address}")
}

/// Spawn a one-shot server with SSE-appropriate headers: `Content-Type:
/// text/event-stream`, `Cache-Control: no-cache`, no `Content-Length`.
/// The body is written directly and the connection is closed (signaling
/// EOF to the client), which mirrors how a real SSE endpoint terminates
/// after `message_stop`.
fn spawn_sse_one_shot_server(status_line: &str, sse_body: &str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("server should bind");
    let address = listener
        .local_addr()
        .expect("server should expose a local address");
    let status_line = status_line.to_owned();
    let sse_body = sse_body.to_owned();
    let _server = std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut request_buffer = [0_u8; 8_192];
            let _ = stream.read(&mut request_buffer);
            let header = format!(
                "HTTP/1.1 {status_line}\r\n\
                 Content-Type: text/event-stream\r\n\
                 Cache-Control: no-cache\r\n\
                 Connection: close\r\n\
                 \r\n"
            );
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(sse_body.as_bytes());
            let _ = stream.flush();
        }
    });
    format!("http://{address}")
}

// ---------------------------------------------------------------------------
// End-to-end Anthropic streaming helpers
// ---------------------------------------------------------------------------

/// Build a single SSE event string with `event:` and `data:` lines.
fn sse_event(event_type: &str, data: &serde_json::Value) -> String {
    format!("event: {event_type}\ndata: {data}\n\n")
}

fn test_anthropic_provider(base_url: String) -> AnthropicProvider {
    AnthropicProvider::with_catalog(
        AnthropicConfig {
            api_key: Some("test-key".to_owned()),
            base_url,
            max_tokens: 64,
        },
        test_model_catalog_for("anthropic", "claude-3-5-sonnet-latest", "Claude 3.5 Sonnet"),
    )
    .expect("test provider should initialize")
}

fn test_anthropic_context() -> Context {
    test_context_for("anthropic", "claude-3-5-sonnet-latest", "Ping")
}

fn run_stream_collect_anthropic(
    provider: &AnthropicProvider,
    context: &Context,
) -> Vec<Result<StreamItem, ProviderError>> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime should build")
        .block_on(async {
            let mut stream = provider
                .stream(context, DEFAULT_STREAM_BUFFER_SIZE)
                .await
                .expect("stream should start");
            let mut items = Vec::new();
            while let Some(item) = stream.recv().await {
                items.push(item);
            }
            items
        })
}

// ---------------------------------------------------------------------------
// End-to-end Anthropic streaming tests (8l–8o)
// ---------------------------------------------------------------------------

/// 8l: End-to-end streaming via one-shot server.
///
/// Serves a complete Anthropic SSE event sequence (message_start →
/// content_block_start → content_block_delta → content_block_stop →
/// message_delta → message_stop) and verifies:
/// - Text fragments arrive incrementally
/// - Tool call deltas have accumulated arguments with correct ordinal indices
/// - Usage and finish reason are present
/// - No `ConnectionLost` item when `message_stop` is received
#[test]
fn anthropic_end_to_end_stream_via_one_shot_server() {
    let sse_body = [
        sse_event(
            "message_start",
            &json!({
                "type": "message_start",
                "message": {"usage": {"input_tokens": 10}}
            }),
        ),
        sse_event(
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "text", "text": ""}
            }),
        ),
        sse_event(
            "content_block_delta",
            &json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "text_delta", "text": "Hello"}
            }),
        ),
        sse_event(
            "content_block_delta",
            &json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "text_delta", "text": " world"}
            }),
        ),
        sse_event(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": 0}),
        ),
        sse_event(
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "index": 1,
                "content_block": {"type": "tool_use", "id": "toolu_1", "name": "get_weather"}
            }),
        ),
        sse_event(
            "content_block_delta",
            &json!({
                "type": "content_block_delta",
                "index": 1,
                "delta": {"type": "input_json_delta", "partial_json": "{\"location\":\"NYC\"}"}
            }),
        ),
        sse_event(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": 1}),
        ),
        sse_event(
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn"},
                "usage": {"output_tokens": 5}
            }),
        ),
        sse_event("message_stop", &json!({"type": "message_stop"})),
    ]
    .concat();

    let base_url = spawn_sse_one_shot_server("200 OK", &sse_body);
    let provider = test_anthropic_provider(base_url);
    let context = test_anthropic_context();
    let items = run_stream_collect_anthropic(&provider, &context);

    // All items should be Ok.
    assert!(items.iter().all(Result::is_ok), "all items should be Ok");

    // Text fragments arrive incrementally.
    let text_items: Vec<&str> = items
        .iter()
        .filter_map(|item| match item {
            Ok(StreamItem::Text(text)) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text_items, vec!["Hello", " world"]);

    // Tool call deltas with correct ordinal index.
    let tool_deltas: Vec<&ToolCallDelta> = items
        .iter()
        .filter_map(|item| match item {
            Ok(StreamItem::ToolCallDelta(delta)) => Some(delta),
            _ => None,
        })
        .collect();
    // At least 2: initial start + argument delta.
    assert!(
        tool_deltas.len() >= 2,
        "expected at least 2 tool call deltas, got {}",
        tool_deltas.len()
    );
    // The initial tool call start should have ordinal 0 (remapped from content block index 1).
    assert_eq!(tool_deltas[0].index, 0);
    assert_eq!(tool_deltas[0].id.as_deref(), Some("toolu_1"));
    assert_eq!(tool_deltas[0].name.as_deref(), Some("get_weather"));
    // The argument delta should have accumulated arguments.
    let last_tool = tool_deltas.last().unwrap();
    assert!(
        last_tool.arguments.is_some(),
        "last tool call delta should have arguments"
    );

    // Usage update present (prompt tokens from message_start + combined at message_delta).
    assert!(
        items
            .iter()
            .any(|item| matches!(item, Ok(StreamItem::UsageUpdate(_)))),
        "stream should include usage update"
    );

    // Finish reason present.
    assert!(
        items
            .iter()
            .any(|item| matches!(item, Ok(StreamItem::FinishReason(r)) if r == "end_turn")),
        "stream should include finish reason"
    );

    // No ConnectionLost when message_stop is received.
    assert!(
        !items
            .iter()
            .any(|item| matches!(item, Ok(StreamItem::ConnectionLost(_)))),
        "no ConnectionLost should be emitted after message_stop"
    );
}

/// 8m: Connection lost when stream ends without `message_stop`.
///
/// Serves an incomplete SSE stream (no `message_stop`) and verifies that
/// `StreamItem::ConnectionLost` is emitted.
#[test]
fn anthropic_stream_connection_lost_without_message_stop() {
    let sse_body = [
        sse_event(
            "message_start",
            &json!({
                "type": "message_start",
                "message": {"usage": {"input_tokens": 5}}
            }),
        ),
        sse_event(
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "text", "text": ""}
            }),
        ),
        sse_event(
            "content_block_delta",
            &json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "text_delta", "text": "Hi"}
            }),
        ),
    ]
    .concat();

    let base_url = spawn_sse_one_shot_server("200 OK", &sse_body);
    let provider = test_anthropic_provider(base_url);
    let context = test_anthropic_context();
    let items = run_stream_collect_anthropic(&provider, &context);

    // Text should have arrived.
    assert!(
        items
            .iter()
            .any(|item| matches!(item, Ok(StreamItem::Text(text)) if text == "Hi")),
        "text delta should have arrived"
    );

    // Last item should be ConnectionLost.
    assert!(
        matches!(items.last(), Some(Ok(StreamItem::ConnectionLost(_)))),
        "stream should end with ConnectionLost when message_stop is missing"
    );
}

/// 8n: HTTP error before streaming starts.
///
/// Serves a 429 response. Verifies `ProviderError::HttpStatus` with the
/// correct status code is returned from `stream()` before any items flow.
#[test]
fn anthropic_stream_http_error_before_streaming() {
    let base_url = spawn_one_shot_server(
        "429 Too Many Requests",
        r#"{"type":"error","error":{"type":"rate_limit_error","message":"rate limited"}}"#,
        "application/json",
    );
    let provider = test_anthropic_provider(base_url);
    let context = test_anthropic_context();

    let result = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime should build")
        .block_on(provider.stream(&context, DEFAULT_STREAM_BUFFER_SIZE));

    assert!(matches!(
        result,
        Err(ProviderError::HttpStatus { status: 429, .. })
    ));
}

/// 8o: SSE with comment lines and CRLF line endings.
///
/// Feeds the SSE parser bytes containing `: keepalive\r\n` comment lines
/// and `\r\n` line endings mixed with normal events. Verifies events parse
/// correctly — this tests `SseDataParser` compatibility with Anthropic's
/// actual wire format.
#[test]
fn anthropic_sse_parser_handles_comment_lines_and_crlf() {
    let mut parser = SseDataParser::default();

    // Build an SSE stream with CRLF line endings and comment lines.
    let sse_data = format!(
        ": keepalive\r\n\
         \r\n\
         event: message_start\r\n\
         data: {}\r\n\
         \r\n\
         : another comment\r\n\
         event: content_block_delta\r\n\
         data: {}\r\n\
         \r\n\
         event: message_stop\r\n\
         data: {}\r\n\
         \r\n",
        json!({"type": "message_start", "message": {"usage": {"input_tokens": 5}}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "OK"}}),
        json!({"type": "message_stop"}),
    );

    let payloads = parser
        .push_chunk(sse_data.as_bytes())
        .expect("CRLF SSE stream should parse without error");

    assert_eq!(
        payloads.len(),
        3,
        "expected 3 data payloads from the CRLF stream, got: {payloads:?}"
    );

    // Verify each payload parses correctly through Anthropic event dispatch.
    let provider = anthropic_provider_id();
    let mut acc = AnthropicToolCallAccumulator::default();
    let mut input_tokens = None;

    // First payload: message_start with prompt tokens.
    let items = expect_items(parse_anthropic_stream_payload(
        &payloads[0],
        &provider,
        &mut acc,
        &mut input_tokens,
    ));
    assert_eq!(items.len(), 1);
    assert!(matches!(
        &items[0],
        StreamItem::UsageUpdate(usage) if usage.prompt_tokens == Some(5)
    ));

    // Second payload: content_block_delta with text.
    let items = expect_items(parse_anthropic_stream_payload(
        &payloads[1],
        &provider,
        &mut acc,
        &mut input_tokens,
    ));
    assert_eq!(items, vec![StreamItem::Text("OK".to_owned())]);

    // Third payload: message_stop.
    let action =
        parse_anthropic_stream_payload(&payloads[2], &provider, &mut acc, &mut input_tokens);
    assert!(matches!(action, AnthropicEventAction::Done));
}
