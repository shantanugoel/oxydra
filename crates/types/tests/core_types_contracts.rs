use std::{
    collections::BTreeMap,
    env, fs,
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::json;
use types::{
    CapsOverrideEntry, CapsOverrides, CatalogProvider, Context, MemoryChunkDocument,
    MemoryChunkUpsertRequest, MemoryChunkUpsertResponse, MemoryError, MemoryForgetRequest,
    MemoryHybridQueryRequest, MemoryHybridQueryResult, MemoryRecallRequest, MemoryRecord,
    MemoryStoreRequest, MemorySummaryReadRequest, MemorySummaryState, MemorySummaryWriteRequest,
    MemorySummaryWriteResult, Message, MessageRole, ModelCatalog, ModelDescriptor, ModelId,
    ModelLimits, ProviderCaps, ProviderError, ProviderId, Response, RuntimeError, ToolCall,
    ToolError, derive_caps, init_tracing,
};

#[test]
fn serde_round_trip_for_core_types() {
    let tool_call = ToolCall {
        id: "call_1".to_owned(),
        name: "file_read".to_owned(),
        arguments: json!({ "path": "Cargo.toml" }),
        metadata: None,
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
        tools: vec![],
        messages: vec![message.clone()],
    };

    let response = Response {
        message,
        tool_calls: vec![tool_call],
        finish_reason: Some("stop".to_owned()),
        usage: None,
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
        provider: ProviderId::from("openai"),
        model: ModelId::from("does-not-exist"),
    };
    let runtime_from_provider: RuntimeError = provider_error.into();
    assert!(matches!(runtime_from_provider, RuntimeError::Provider(_)));

    let tool_error = ToolError::InvalidArguments {
        tool: "file_read".to_owned(),
        message: "path is required".to_owned(),
    };
    let runtime_from_tool: RuntimeError = tool_error.into();
    assert!(matches!(runtime_from_tool, RuntimeError::Tool(_)));

    let memory_error = MemoryError::NotFound {
        session_id: "session-1".to_owned(),
    };
    let runtime_from_memory: RuntimeError = memory_error.into();
    assert!(matches!(runtime_from_memory, RuntimeError::Memory(_)));
}

#[test]
fn memory_contract_types_serialize_round_trip() {
    let store = MemoryStoreRequest {
        session_id: "session-1".to_owned(),
        sequence: 7,
        payload: json!({"kind":"assistant","content":"hello"}),
    };
    let recall = MemoryRecallRequest {
        session_id: "session-1".to_owned(),
        limit: Some(50),
    };
    let forget = MemoryForgetRequest {
        session_id: "session-1".to_owned(),
    };
    let record = MemoryRecord {
        session_id: store.session_id.clone(),
        sequence: store.sequence,
        payload: store.payload.clone(),
    };
    let upsert_request = MemoryChunkUpsertRequest {
        session_id: "session-1".to_owned(),
        chunks: vec![MemoryChunkDocument {
            chunk_id: "chunk-1".to_owned(),
            content_hash: "hash-1".to_owned(),
            text: "chunk text".to_owned(),
            file_id: Some(7),
            sequence_start: Some(1),
            sequence_end: Some(2),
            metadata: Some(json!({"role":"assistant"})),
            embedding: Some(vec![0.1, 0.2, 0.3]),
            embedding_model: Some("local-embedding".to_owned()),
        }],
    };
    let upsert_response = MemoryChunkUpsertResponse {
        upserted_chunks: 1,
        skipped_chunks: 0,
    };
    let hybrid_query = MemoryHybridQueryRequest {
        session_id: "session-1".to_owned(),
        query: "hello".to_owned(),
        query_embedding: Some(vec![0.9, 0.1]),
        top_k: Some(5),
        vector_weight: Some(0.7),
        fts_weight: Some(0.3),
    };
    let hybrid_result = MemoryHybridQueryResult {
        chunk_id: "chunk-1".to_owned(),
        session_id: "session-1".to_owned(),
        text: "chunk text".to_owned(),
        score: 0.42,
        vector_score: 0.5,
        fts_score: 0.3,
        file_id: Some(7),
        sequence_start: Some(1),
        sequence_end: Some(2),
        metadata: Some(json!({"source":"conversation"})),
    };
    let summary_read = MemorySummaryReadRequest {
        session_id: "session-1".to_owned(),
    };
    let summary_state = MemorySummaryState {
        session_id: "session-1".to_owned(),
        epoch: 3,
        upper_sequence: 40,
        summary: "summary text".to_owned(),
        metadata: Some(json!({"model":"gpt"})),
    };
    let summary_write = MemorySummaryWriteRequest {
        session_id: "session-1".to_owned(),
        expected_epoch: 3,
        next_epoch: 4,
        upper_sequence: 42,
        summary: "new summary".to_owned(),
        metadata: Some(json!({"reason":"budget"})),
    };
    let summary_write_result = MemorySummaryWriteResult {
        applied: true,
        current_epoch: 4,
    };

    let store_json = serde_json::to_string(&store).expect("store request should serialize");
    let parsed_store: MemoryStoreRequest =
        serde_json::from_str(&store_json).expect("store request should deserialize");
    assert_eq!(parsed_store, store);

    let recall_json = serde_json::to_string(&recall).expect("recall request should serialize");
    let parsed_recall: MemoryRecallRequest =
        serde_json::from_str(&recall_json).expect("recall request should deserialize");
    assert_eq!(parsed_recall, recall);

    let forget_json = serde_json::to_string(&forget).expect("forget request should serialize");
    let parsed_forget: MemoryForgetRequest =
        serde_json::from_str(&forget_json).expect("forget request should deserialize");
    assert_eq!(parsed_forget, forget);

    let record_json = serde_json::to_string(&record).expect("record should serialize");
    let parsed_record: MemoryRecord =
        serde_json::from_str(&record_json).expect("record should deserialize");
    assert_eq!(parsed_record, record);

    let upsert_request_json =
        serde_json::to_string(&upsert_request).expect("upsert request should serialize");
    let parsed_upsert_request: MemoryChunkUpsertRequest =
        serde_json::from_str(&upsert_request_json).expect("upsert request should deserialize");
    assert_eq!(parsed_upsert_request, upsert_request);

    let upsert_response_json =
        serde_json::to_string(&upsert_response).expect("upsert response should serialize");
    let parsed_upsert_response: MemoryChunkUpsertResponse =
        serde_json::from_str(&upsert_response_json).expect("upsert response should deserialize");
    assert_eq!(parsed_upsert_response, upsert_response);

    let hybrid_query_json =
        serde_json::to_string(&hybrid_query).expect("hybrid query should serialize");
    let parsed_hybrid_query: MemoryHybridQueryRequest =
        serde_json::from_str(&hybrid_query_json).expect("hybrid query should deserialize");
    assert_eq!(parsed_hybrid_query, hybrid_query);

    let hybrid_result_json =
        serde_json::to_string(&hybrid_result).expect("hybrid result should serialize");
    let parsed_hybrid_result: MemoryHybridQueryResult =
        serde_json::from_str(&hybrid_result_json).expect("hybrid result should deserialize");
    assert_eq!(parsed_hybrid_result, hybrid_result);

    let summary_read_json =
        serde_json::to_string(&summary_read).expect("summary read should serialize");
    let parsed_summary_read: MemorySummaryReadRequest =
        serde_json::from_str(&summary_read_json).expect("summary read should deserialize");
    assert_eq!(parsed_summary_read, summary_read);

    let summary_state_json =
        serde_json::to_string(&summary_state).expect("summary state should serialize");
    let parsed_summary_state: MemorySummaryState =
        serde_json::from_str(&summary_state_json).expect("summary state should deserialize");
    assert_eq!(parsed_summary_state, summary_state);

    let summary_write_json =
        serde_json::to_string(&summary_write).expect("summary write should serialize");
    let parsed_summary_write: MemorySummaryWriteRequest =
        serde_json::from_str(&summary_write_json).expect("summary write should deserialize");
    assert_eq!(parsed_summary_write, summary_write);

    let summary_write_result_json = serde_json::to_string(&summary_write_result)
        .expect("summary write result should serialize");
    let parsed_summary_write_result: MemorySummaryWriteResult =
        serde_json::from_str(&summary_write_result_json)
            .expect("summary write result should deserialize");
    assert_eq!(parsed_summary_write_result, summary_write_result);
}

#[test]
fn tracing_subscriber_initializes_and_emits() {
    init_tracing();
    let span = tracing::info_span!("core_types_tracing_smoke");
    let _guard = span.enter();
    tracing::info!("core types tracing smoke");
}

#[test]
fn model_catalog_validates_provider_and_model_id() {
    let provider = ProviderId::from("openai");
    let known_model = ModelId::from("gpt-4o-mini");
    let catalog = test_catalog_with_model(
        "openai",
        "gpt-4o-mini",
        "GPT-4o mini",
        ModelLimits {
            context: 128_000,
            output: 16_384,
        },
    );

    let descriptor = catalog
        .validate(&provider, &known_model)
        .expect("known model must validate");
    assert_eq!(descriptor.id, known_model.0);

    let unknown = catalog.validate(&provider, &ModelId::from("unknown-model"));
    assert!(matches!(
        unknown,
        Err(ProviderError::UnknownModel {
            provider: _,
            model: _
        })
    ));
}

#[test]
fn pinned_catalog_snapshot_parses_and_validates_known_model() {
    let catalog = ModelCatalog::from_pinned_snapshot().expect("pinned snapshot must parse");
    assert!(
        !catalog.providers.is_empty(),
        "pinned catalog must not be empty"
    );
    assert!(catalog.all_models().all(|(provider_id, model_id, _)| {
        !provider_id.trim().is_empty() && !model_id.trim().is_empty()
    }));

    let openai_provider = ProviderId::from("openai");
    let known_model_id = catalog
        .providers
        .get("openai")
        .and_then(|p| p.models.keys().next())
        .map(|id| ModelId::from(id.as_str()))
        .expect("pinned snapshot should include at least one openai model");
    let descriptor = catalog
        .validate(&openai_provider, &known_model_id)
        .expect("known pinned model should validate");
    assert_eq!(descriptor.id, known_model_id.0);

    let anthropic_provider = ProviderId::from("anthropic");
    let anthropic_model_id = catalog
        .providers
        .get("anthropic")
        .and_then(|p| p.models.keys().next())
        .map(|id| ModelId::from(id.as_str()))
        .expect("pinned snapshot should include at least one anthropic model");
    let anthropic_descriptor = catalog
        .validate(&anthropic_provider, &anthropic_model_id)
        .expect("known anthropic pinned model should validate");
    assert_eq!(anthropic_descriptor.id, anthropic_model_id.0);
}

#[test]
fn pinned_catalog_snapshot_rejects_missing_required_fields() {
    // CatalogProvider requires `id` and `name`; a model requires `id` and `name`.
    // This JSON has a provider entry missing the `name` field on a model.
    let invalid_snapshot = r#"
    {
      "openai": {
        "id": "openai",
        "name": "OpenAI",
        "models": {
          "missing-name": {
            "id": "missing-name"
          }
        }
      }
    }
    "#;

    let parse_result = ModelCatalog::from_snapshot_str(invalid_snapshot);
    assert!(matches!(parse_result, Err(ProviderError::Serialization(_))));
}

#[test]
fn model_catalog_regenerator_writes_canonical_sorted_snapshot() {
    // Provider keys and model keys are BTreeMap-ordered, so output is deterministic.
    let unsorted_snapshot = r#"
    {
      "zebra": {
        "id": "zebra",
        "name": "Zebra Provider",
        "models": {
          "z-model": { "id": "z-model", "name": "Z Model" },
          "a-model": { "id": "a-model", "name": "A Model" }
        }
      },
      "alpha": {
        "id": "alpha",
        "name": "Alpha Provider",
        "models": {
          "b-model": { "id": "b-model", "name": "B Model" }
        }
      }
    }
    "#;

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time should move forward")
        .as_nanos();
    let output_path = env::temp_dir().join(format!("oxydra-model-catalog-{timestamp}.json"));

    ModelCatalog::regenerate_snapshot(unsorted_snapshot, &output_path)
        .expect("regeneration should write canonical snapshot");

    let regenerated = fs::read_to_string(&output_path).expect("snapshot should be readable");
    fs::remove_file(&output_path).expect("temp snapshot should be removable");

    let catalog =
        ModelCatalog::from_snapshot_str(&regenerated).expect("regenerated snapshot should parse");

    // BTreeMap ordering: "alpha" before "zebra"
    let provider_ids: Vec<&str> = catalog.providers.keys().map(|k| k.as_str()).collect();
    assert_eq!(provider_ids, vec!["alpha", "zebra"]);

    // Within "zebra": "a-model" before "z-model"
    let zebra_model_ids: Vec<&str> = catalog.providers["zebra"]
        .models
        .keys()
        .map(|k| k.as_str())
        .collect();
    assert_eq!(zebra_model_ids, vec!["a-model", "z-model"]);
}

#[test]
fn provider_caps_serialize_round_trip() {
    let caps = ProviderCaps {
        supports_streaming: false,
        supports_tools: true,
        supports_json_mode: true,
        supports_reasoning_traces: false,
        max_input_tokens: Some(128_000),
        max_output_tokens: Some(16_384),
        max_context_tokens: Some(128_000),
    };

    let encoded = serde_json::to_string(&caps).expect("caps should serialize");
    let decoded: ProviderCaps = serde_json::from_str(&encoded).expect("caps should deserialize");
    assert_eq!(decoded, caps);
}

#[test]
fn models_dev_json_snippet_deserializes() {
    // A representative subset of models.dev JSON should parse into the new structs.
    let snippet = r#"
    {
      "openai": {
        "id": "openai",
        "name": "OpenAI",
        "env": ["OPENAI_API_KEY"],
        "api": "https://api.openai.com/v1",
        "doc": "https://platform.openai.com/docs",
        "models": {
          "gpt-4o": {
            "id": "gpt-4o",
            "name": "GPT-4o",
            "family": "gpt-4o",
            "attachment": true,
            "reasoning": false,
            "tool_call": true,
            "structured_output": true,
            "temperature": true,
            "knowledge": "2023-10",
            "release_date": "2024-05-13",
            "modalities": { "input": ["text", "image"], "output": ["text"] },
            "open_weights": false,
            "cost": { "input": 2.5, "output": 10.0, "cache_read": 1.25 },
            "limit": { "context": 128000, "output": 16384 }
          },
          "o3-mini": {
            "id": "o3-mini",
            "name": "OpenAI o3-mini",
            "family": "o3",
            "reasoning": true,
            "tool_call": true,
            "interleaved": { "field": "reasoning_content" },
            "structured_output": true,
            "modalities": { "input": ["text"], "output": ["text"] },
            "cost": { "input": 1.1, "output": 4.4 },
            "limit": { "context": 200000, "output": 100000 }
          }
        }
      }
    }
    "#;

    let catalog = ModelCatalog::from_snapshot_str(snippet).expect("snippet should parse");
    assert_eq!(catalog.providers.len(), 1);

    let openai = &catalog.providers["openai"];
    assert_eq!(openai.id, "openai");
    assert_eq!(openai.name, "OpenAI");
    assert_eq!(openai.env, vec!["OPENAI_API_KEY"]);
    assert_eq!(openai.api.as_deref(), Some("https://api.openai.com/v1"));
    assert_eq!(openai.models.len(), 2);

    let gpt4o = &openai.models["gpt-4o"];
    assert_eq!(gpt4o.id, "gpt-4o");
    assert_eq!(gpt4o.name, "GPT-4o");
    assert_eq!(gpt4o.family.as_deref(), Some("gpt-4o"));
    assert!(gpt4o.attachment);
    assert!(!gpt4o.reasoning);
    assert!(gpt4o.tool_call);
    assert!(gpt4o.structured_output);
    assert!(gpt4o.temperature);
    assert_eq!(gpt4o.knowledge.as_deref(), Some("2023-10"));
    assert_eq!(gpt4o.modalities.input, vec!["text", "image"]);
    assert_eq!(gpt4o.modalities.output, vec!["text"]);
    assert!(!gpt4o.open_weights);
    assert!((gpt4o.cost.input - 2.5).abs() < f64::EPSILON);
    assert!((gpt4o.cost.output - 10.0).abs() < f64::EPSILON);
    assert!((gpt4o.cost.cache_read.unwrap() - 1.25).abs() < f64::EPSILON);
    assert_eq!(gpt4o.limit.context, 128_000);
    assert_eq!(gpt4o.limit.output, 16_384);

    let o3_mini = &openai.models["o3-mini"];
    assert!(o3_mini.reasoning);
    assert!(o3_mini.interleaved.is_some());
    assert_eq!(
        o3_mini.interleaved.as_ref().unwrap().field,
        "reasoning_content"
    );

    // Verify to_provider_caps derivation
    let caps = gpt4o.to_provider_caps();
    assert!(caps.supports_streaming); // default true
    assert!(caps.supports_tools);
    assert!(caps.supports_json_mode);
    assert!(!caps.supports_reasoning_traces);
    assert_eq!(caps.max_context_tokens, Some(128_000));
    assert_eq!(caps.max_output_tokens, Some(16_384));

    let o3_caps = o3_mini.to_provider_caps();
    assert!(o3_caps.supports_reasoning_traces);
}

#[test]
fn models_dev_interleaved_boolean_deserializes() {
    let snippet = r#"
    {
      "openai": {
        "id": "openai",
        "name": "OpenAI",
        "env": ["OPENAI_API_KEY"],
        "models": {
          "o3-mini": {
            "id": "o3-mini",
            "name": "OpenAI o3-mini",
            "reasoning": true,
            "interleaved": true,
            "limit": { "context": 200000, "output": 100000 }
          },
          "o3-mini-false": {
            "id": "o3-mini-false",
            "name": "OpenAI o3-mini false",
            "reasoning": true,
            "interleaved": false,
            "limit": { "context": 200000, "output": 100000 }
          }
        }
      }
    }
    "#;

    let catalog = ModelCatalog::from_snapshot_str(snippet).expect("snippet should parse");
    let openai = &catalog.providers["openai"];

    let o3_mini = &openai.models["o3-mini"];
    assert!(o3_mini.interleaved.is_some());
    assert_eq!(
        o3_mini.interleaved.as_ref().unwrap().field,
        "reasoning_content"
    );

    let o3_mini_false = &openai.models["o3-mini-false"];
    assert!(o3_mini_false.interleaved.is_none());
}

#[test]
fn catalog_serialization_round_trip_is_deterministic() {
    let catalog = ModelCatalog::from_pinned_snapshot().expect("pinned snapshot must parse");

    let first_pass = serde_json::to_string_pretty(&catalog).expect("first serialize");
    let reparsed: ModelCatalog = serde_json::from_str(&first_pass).expect("reparse should succeed");
    let second_pass = serde_json::to_string_pretty(&reparsed).expect("second serialize");

    assert_eq!(
        first_pass, second_pass,
        "serialize → deserialize → serialize must produce identical output"
    );
}

#[test]
fn all_models_iterator_yields_all_entries() {
    let catalog = ModelCatalog::from_pinned_snapshot().expect("pinned snapshot must parse");

    let expected_count: usize = catalog.providers.values().map(|p| p.models.len()).sum();
    let actual_count = catalog.all_models().count();
    assert_eq!(actual_count, expected_count);
    assert!(
        actual_count > 0,
        "catalog should contain at least one model"
    );

    // Every yielded triple should be consistent with the underlying maps
    for (provider_id, model_id, descriptor) in catalog.all_models() {
        assert_eq!(descriptor.id, model_id);
        assert!(catalog.providers.contains_key(provider_id));
        assert!(catalog.providers[provider_id].models.contains_key(model_id));
    }
}

#[test]
fn to_provider_caps_maps_fields_correctly() {
    let descriptor = ModelDescriptor {
        id: "test-model".to_owned(),
        name: "Test Model".to_owned(),
        family: None,
        attachment: false,
        reasoning: false,
        tool_call: true,
        interleaved: None,
        structured_output: true,
        temperature: true,
        knowledge: None,
        release_date: None,
        last_updated: None,
        modalities: Default::default(),
        open_weights: false,
        cost: Default::default(),
        limit: ModelLimits {
            context: 100_000,
            output: 8192,
        },
    };

    let caps = descriptor.to_provider_caps();
    assert!(caps.supports_streaming);
    assert!(caps.supports_tools);
    assert!(caps.supports_json_mode);
    assert!(!caps.supports_reasoning_traces);
    assert_eq!(caps.max_input_tokens, Some(100_000));
    assert_eq!(caps.max_output_tokens, Some(8192));
    assert_eq!(caps.max_context_tokens, Some(100_000));

    // With reasoning + interleaved
    let reasoning_descriptor = ModelDescriptor {
        reasoning: true,
        interleaved: Some(types::InterleavedSpec {
            field: "thinking".to_owned(),
        }),
        ..descriptor
    };
    let reasoning_caps = reasoning_descriptor.to_provider_caps();
    assert!(reasoning_caps.supports_reasoning_traces);
}

// ---------------------------------------------------------------------------
// Step 2 verification: ProviderCaps derivation + Oxydra overlay
// ---------------------------------------------------------------------------

#[test]
fn caps_derived_from_model_descriptor() {
    let descriptor = ModelDescriptor {
        id: "test-model".to_owned(),
        name: "Test Model".to_owned(),
        family: None,
        attachment: false,
        reasoning: false,
        tool_call: true,
        interleaved: None,
        structured_output: false,
        temperature: true,
        knowledge: None,
        release_date: None,
        last_updated: None,
        modalities: Default::default(),
        open_weights: false,
        cost: Default::default(),
        limit: ModelLimits {
            context: 64_000,
            output: 4096,
        },
    };

    let overrides = CapsOverrides::default();
    let caps = derive_caps("test-provider", &descriptor, &overrides);

    assert!(caps.supports_tools, "tool_call=true → supports_tools=true");
    assert!(
        !caps.supports_json_mode,
        "structured_output=false → supports_json_mode=false"
    );
    assert!(
        !caps.supports_reasoning_traces,
        "reasoning=false → supports_reasoning_traces=false"
    );
    assert_eq!(caps.max_context_tokens, Some(64_000));
    assert_eq!(caps.max_output_tokens, Some(4096));
}

#[test]
fn caps_overlay_applies() {
    let descriptor = ModelDescriptor {
        id: "claude-3-5-sonnet-latest".to_owned(),
        name: "Claude 3.5 Sonnet".to_owned(),
        family: None,
        attachment: false,
        reasoning: false,
        tool_call: true,
        interleaved: None,
        structured_output: false,
        temperature: true,
        knowledge: None,
        release_date: None,
        last_updated: None,
        modalities: Default::default(),
        open_weights: false,
        cost: Default::default(),
        limit: ModelLimits {
            context: 200_000,
            output: 8192,
        },
    };

    // Baseline: to_provider_caps defaults streaming to true
    let baseline = descriptor.to_provider_caps();
    assert!(baseline.supports_streaming);

    // Model-specific overlay sets streaming to false
    let mut overrides = CapsOverrides::default();
    overrides.overrides.insert(
        "anthropic/claude-3-5-sonnet-latest".to_owned(),
        CapsOverrideEntry {
            supports_streaming: Some(false),
            ..Default::default()
        },
    );

    let caps = derive_caps("anthropic", &descriptor, &overrides);
    assert!(
        !caps.supports_streaming,
        "model-specific overlay should override baseline"
    );
    assert!(
        caps.supports_tools,
        "non-overridden field should keep baseline value"
    );
}

#[test]
fn caps_default_by_provider() {
    let descriptor = ModelDescriptor {
        id: "some-model".to_owned(),
        name: "Some Model".to_owned(),
        family: None,
        attachment: false,
        reasoning: false,
        tool_call: false,
        interleaved: None,
        structured_output: false,
        temperature: true,
        knowledge: None,
        release_date: None,
        last_updated: None,
        modalities: Default::default(),
        open_weights: false,
        cost: Default::default(),
        limit: ModelLimits {
            context: 100_000,
            output: 4096,
        },
    };

    // Provider default sets supports_tools to true
    let mut overrides = CapsOverrides::default();
    overrides.provider_defaults.insert(
        "my-provider".to_owned(),
        CapsOverrideEntry {
            supports_tools: Some(true),
            ..Default::default()
        },
    );

    let caps = derive_caps("my-provider", &descriptor, &overrides);
    assert!(
        caps.supports_tools,
        "provider default should override baseline when no model override exists"
    );
}

#[test]
fn caps_model_override_takes_precedence_over_provider_default() {
    let descriptor = ModelDescriptor {
        id: "special-model".to_owned(),
        name: "Special Model".to_owned(),
        family: None,
        attachment: false,
        reasoning: false,
        tool_call: false,
        interleaved: None,
        structured_output: false,
        temperature: true,
        knowledge: None,
        release_date: None,
        last_updated: None,
        modalities: Default::default(),
        open_weights: false,
        cost: Default::default(),
        limit: ModelLimits {
            context: 100_000,
            output: 4096,
        },
    };

    let mut overrides = CapsOverrides::default();
    // Provider default: streaming = false
    overrides.provider_defaults.insert(
        "my-provider".to_owned(),
        CapsOverrideEntry {
            supports_streaming: Some(false),
            ..Default::default()
        },
    );
    // Model-specific: streaming = true
    overrides.overrides.insert(
        "my-provider/special-model".to_owned(),
        CapsOverrideEntry {
            supports_streaming: Some(true),
            ..Default::default()
        },
    );

    let caps = derive_caps("my-provider", &descriptor, &overrides);
    assert!(
        caps.supports_streaming,
        "model-specific override should take precedence over provider default"
    );
}

#[test]
fn caps_overrides_json_round_trip() {
    let mut overrides = CapsOverrides::default();
    overrides.provider_defaults.insert(
        "openai".to_owned(),
        CapsOverrideEntry {
            supports_streaming: Some(true),
            ..Default::default()
        },
    );
    overrides.overrides.insert(
        "openai/gpt-4o".to_owned(),
        CapsOverrideEntry {
            max_output_tokens: Some(32_768),
            ..Default::default()
        },
    );

    let json = serde_json::to_string_pretty(&overrides).expect("overrides should serialize");
    let parsed: CapsOverrides = serde_json::from_str(&json).expect("overrides should deserialize");
    assert_eq!(parsed, overrides);
}

#[test]
fn pinned_caps_overrides_parses() {
    // The pinned overrides file bundled via include_str! should parse.
    let catalog = ModelCatalog::from_pinned_snapshot().expect("pinned snapshot must parse");
    // Provider defaults should be loaded
    assert!(
        !catalog.caps_overrides.provider_defaults.is_empty(),
        "pinned overrides should have provider defaults"
    );
}

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

fn test_catalog_with_model(
    provider_id: &str,
    model_id: &str,
    model_name: &str,
    limit: ModelLimits,
) -> ModelCatalog {
    let mut models = BTreeMap::new();
    models.insert(
        model_id.to_owned(),
        ModelDescriptor {
            id: model_id.to_owned(),
            name: model_name.to_owned(),
            family: None,
            attachment: false,
            reasoning: false,
            tool_call: true,
            interleaved: None,
            structured_output: false,
            temperature: true,
            knowledge: None,
            release_date: None,
            last_updated: None,
            modalities: Default::default(),
            open_weights: false,
            cost: Default::default(),
            limit,
        },
    );

    let mut providers = BTreeMap::new();
    providers.insert(
        provider_id.to_owned(),
        CatalogProvider {
            id: provider_id.to_owned(),
            name: provider_id.to_owned(),
            env: vec![],
            api: None,
            doc: None,
            models,
        },
    );

    ModelCatalog::new(providers)
}
