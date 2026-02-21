use std::{
    collections::VecDeque,
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
