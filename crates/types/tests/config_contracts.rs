use types::{
    AgentConfig, ConfigError, MemoryConfig, MemoryEmbeddingBackend, Model2vecModel, ModelId,
    ProviderId, SUPPORTED_CONFIG_MAJOR_VERSION, validate_config_version,
};

#[test]
fn default_agent_config_is_valid() {
    let config = AgentConfig::default();
    assert_eq!(config.selection.provider, ProviderId::from("openai"));
    assert_eq!(config.selection.model, ModelId::from("gpt-4o-mini"));
    assert_eq!(config.memory, MemoryConfig::default());
    assert_eq!(
        config.memory.embedding_backend,
        MemoryEmbeddingBackend::Model2vec
    );
    assert_eq!(config.memory.model2vec_model, Model2vecModel::Potion32m);
    assert!(config.validate().is_ok());
}

#[test]
fn default_memory_embedding_config_uses_model2vec_potion_32m() {
    let config = MemoryConfig::default();
    assert_eq!(config.embedding_backend, MemoryEmbeddingBackend::Model2vec);
    assert_eq!(config.model2vec_model, Model2vecModel::Potion32m);
}

#[test]
fn memory_embedding_enums_serialize_with_expected_wire_values() {
    assert_eq!(
        serde_json::to_string(&MemoryEmbeddingBackend::Model2vec).expect("serialize backend"),
        "\"model2vec\""
    );
    assert_eq!(
        serde_json::to_string(&MemoryEmbeddingBackend::Deterministic).expect("serialize backend"),
        "\"deterministic\""
    );
    assert_eq!(
        serde_json::to_string(&Model2vecModel::Potion8m).expect("serialize model"),
        "\"potion_8m\""
    );
    assert_eq!(
        serde_json::to_string(&Model2vecModel::Potion32m).expect("serialize model"),
        "\"potion_32m\""
    );
}

#[test]
fn config_version_accepts_supported_major() {
    for version in ["1", "1.0", "1.0.0", "1.2.3"] {
        assert!(
            validate_config_version(version).is_ok(),
            "version `{version}` should be accepted for major {SUPPORTED_CONFIG_MAJOR_VERSION}"
        );
    }
}

#[test]
fn config_version_rejects_unsupported_major() {
    let error = validate_config_version("2.0.0").expect_err("version 2.x should be rejected");
    assert_eq!(
        error,
        ConfigError::UnsupportedConfigVersion {
            version: "2.0.0".to_owned(),
            supported_major: SUPPORTED_CONFIG_MAJOR_VERSION,
        }
    );
}

#[test]
fn config_version_rejects_invalid_format() {
    let error = validate_config_version("v1").expect_err("non-numeric version should be rejected");
    assert_eq!(
        error,
        ConfigError::InvalidConfigVersionFormat {
            version: "v1".to_owned(),
        }
    );
}

#[test]
fn validation_rejects_unknown_provider() {
    let mut config = AgentConfig::default();
    config.selection.provider = ProviderId::from("unsupported");
    let error = config
        .validate()
        .expect_err("unknown provider should fail validation");
    assert_eq!(
        error,
        ConfigError::UnsupportedProvider {
            provider: "unsupported".to_owned(),
        }
    );
}

#[test]
fn validation_rejects_empty_model_id() {
    let mut config = AgentConfig::default();
    config.selection.model = ModelId::from("  ");
    let error = config
        .validate()
        .expect_err("empty model id should fail validation");
    assert_eq!(
        error,
        ConfigError::EmptyModelForProvider {
            provider: "openai".to_owned(),
        }
    );
}

#[test]
fn validation_rejects_invalid_reliability_bounds() {
    let mut config = AgentConfig::default();
    config.reliability.backoff_base_ms = 10_000;
    config.reliability.backoff_max_ms = 100;
    let error = config
        .validate()
        .expect_err("invalid reliability bounds should fail validation");
    assert_eq!(
        error,
        ConfigError::InvalidReliabilityBackoff {
            base_ms: 10_000,
            max_ms: 100,
        }
    );
}

#[test]
fn validation_rejects_enabled_remote_memory_without_auth_token() {
    let mut config = AgentConfig::default();
    config.memory.enabled = true;
    config.memory.remote_url = Some("libsql://example-org.turso.io".to_owned());
    config.memory.auth_token = Some("   ".to_owned());
    let error = config
        .validate()
        .expect_err("remote mode without auth token should fail validation");
    assert_eq!(
        error,
        ConfigError::MissingMemoryAuthToken {
            remote_url: "libsql://example-org.turso.io".to_owned()
        }
    );
}

#[test]
fn validation_rejects_invalid_context_budget_ratio() {
    let mut config = AgentConfig::default();
    config.runtime.context_budget.trigger_ratio = 1.2;
    let error = config
        .validate()
        .expect_err("invalid trigger ratio should fail validation");
    assert_eq!(error, ConfigError::InvalidContextBudgetRatio { value: 1.2 });
}

#[test]
fn validation_rejects_invalid_context_fallback_max_context_tokens() {
    let mut config = AgentConfig::default();
    config.runtime.context_budget.fallback_max_context_tokens = 0;
    let error = config
        .validate()
        .expect_err("invalid fallback_max_context_tokens should fail validation");
    assert_eq!(
        error,
        ConfigError::InvalidContextFallbackMaxContextTokens { value: 0 }
    );
}

#[test]
fn validation_rejects_invalid_summarization_min_turns() {
    let mut config = AgentConfig::default();
    config.runtime.summarization.min_turns = 0;
    let error = config
        .validate()
        .expect_err("invalid min_turns should fail validation");
    assert_eq!(
        error,
        ConfigError::InvalidSummarizationMinTurns { value: 0 }
    );
}

#[test]
fn validation_rejects_invalid_retrieval_weight_sum() {
    let mut config = AgentConfig::default();
    config.memory.retrieval.vector_weight = 0.5;
    config.memory.retrieval.fts_weight = 0.2;
    let error = config
        .validate()
        .expect_err("retrieval weights must sum to 1.0");
    assert_eq!(
        error,
        ConfigError::InvalidRetrievalWeightSum {
            vector_weight: 0.5,
            fts_weight: 0.2
        }
    );
}
