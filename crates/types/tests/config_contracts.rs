use types::{
    AgentConfig, ConfigError, MemoryConfig, ModelId, ProviderId, SUPPORTED_CONFIG_MAJOR_VERSION,
    validate_config_version,
};

#[test]
fn default_agent_config_is_valid() {
    let config = AgentConfig::default();
    assert_eq!(config.selection.provider, ProviderId::from("openai"));
    assert_eq!(config.selection.model, ModelId::from("gpt-4o-mini"));
    assert_eq!(config.memory, MemoryConfig::default());
    assert!(config.validate().is_ok());
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
fn validation_rejects_enabled_local_memory_with_empty_database_path() {
    let mut config = AgentConfig::default();
    config.memory.enabled = true;
    config.memory.db_path = "  ".to_owned();
    let error = config
        .validate()
        .expect_err("empty local db path should fail validation");
    assert_eq!(error, ConfigError::InvalidMemoryDatabasePath);
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
