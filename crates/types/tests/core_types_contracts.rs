use std::{
    env, fs,
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::json;
use types::{
    Context, MemoryChunkDocument, MemoryChunkUpsertRequest, MemoryChunkUpsertResponse, MemoryError,
    MemoryForgetRequest, MemoryHybridQueryRequest, MemoryHybridQueryResult, MemoryRecallRequest,
    MemoryRecord, MemoryStoreRequest, MemorySummaryReadRequest, MemorySummaryState,
    MemorySummaryWriteRequest, MemorySummaryWriteResult, Message, MessageRole, ModelCatalog,
    ModelDescriptor, ModelId, ProviderCaps, ProviderError, ProviderId, Response, RuntimeError,
    ToolCall, ToolError, init_tracing,
};

#[test]
fn serde_round_trip_for_core_types() {
    let tool_call = ToolCall {
        id: "call_1".to_owned(),
        name: "file_read".to_owned(),
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
    let catalog = ModelCatalog::new(vec![ModelDescriptor {
        provider: provider.clone(),
        model: known_model.clone(),
        display_name: Some("GPT-4o mini".to_owned()),
        caps: ProviderCaps {
            supports_streaming: false,
            supports_tools: true,
            supports_json_mode: true,
            supports_reasoning_traces: false,
            max_input_tokens: Some(128_000),
            max_output_tokens: Some(16_384),
            max_context_tokens: Some(128_000),
        },
        deprecated: false,
    }]);

    let descriptor = catalog
        .validate(&provider, &known_model)
        .expect("known model must validate");
    assert_eq!(descriptor.model, known_model);

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
        !catalog.models.is_empty(),
        "pinned catalog must not be empty"
    );
    assert!(catalog.models.iter().all(|descriptor| {
        !descriptor.provider.0.trim().is_empty() && !descriptor.model.0.trim().is_empty()
    }));

    let openai_provider = ProviderId::from("openai");
    let known_model = catalog
        .models
        .iter()
        .find(|descriptor| descriptor.provider == openai_provider)
        .map(|descriptor| descriptor.model.clone())
        .expect("pinned snapshot should include at least one openai model");
    let descriptor = catalog
        .validate(&openai_provider, &known_model)
        .expect("known pinned model should validate");
    assert_eq!(descriptor.model, known_model);

    let anthropic_provider = ProviderId::from("anthropic");
    let anthropic_model = catalog
        .models
        .iter()
        .find(|descriptor| descriptor.provider == anthropic_provider)
        .map(|descriptor| descriptor.model.clone())
        .expect("pinned snapshot should include at least one anthropic model");
    let anthropic_descriptor = catalog
        .validate(&anthropic_provider, &anthropic_model)
        .expect("known anthropic pinned model should validate");
    assert_eq!(anthropic_descriptor.model, anthropic_model);
}

#[test]
fn pinned_catalog_snapshot_rejects_missing_required_fields() {
    let invalid_snapshot = r#"
    {
      "models": [
        {
          "provider": "openai",
          "display_name": "Missing model field"
        }
      ]
    }
    "#;

    let parse_result = ModelCatalog::from_snapshot_str(invalid_snapshot);
    assert!(matches!(parse_result, Err(ProviderError::Serialization(_))));
}

#[test]
fn model_catalog_regenerator_writes_canonical_sorted_snapshot() {
    let unsorted_snapshot = r#"
    {
      "models": [
        {
          "provider": "openai",
          "model": "z-model",
          "caps": {}
        },
        {
          "provider": "openai",
          "model": "a-model",
          "caps": {}
        }
      ]
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
    assert_eq!(catalog.models[0].model, ModelId::from("a-model"));
    assert_eq!(catalog.models[1].model, ModelId::from("z-model"));
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
