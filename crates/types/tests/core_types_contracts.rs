use std::{
    env, fs,
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::json;
use types::{
    Context, Message, MessageRole, ModelCatalog, ModelDescriptor, ModelId, ProviderCaps,
    ProviderError, ProviderId, Response, RuntimeError, ToolCall, ToolError, init_tracing,
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
