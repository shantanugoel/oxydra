use std::collections::BTreeMap;

use types::{
    BootstrapEnvelopeError, ChannelsConfig, DEFAULT_RUNNER_CONFIG_VERSION, DEFAULT_RUNNER_TIMEZONE,
    LOG_TAIL_DEFAULT, LOG_TAIL_MAX, LogFormat, LogRole, LogSource, LogStream,
    RunnerBootstrapEnvelope, RunnerConfigError, RunnerControl, RunnerControlLogsRequest,
    RunnerGlobalConfig, RunnerLogEntry, RunnerResolvedMountPaths, RunnerResourceLimits,
    RunnerRuntimePolicy, RunnerUserConfig, RunnerUserRegistration,
    SUPPORTED_RUNNER_CONFIG_MAJOR_VERSION, SandboxTier, SenderBinding, SidecarEndpoint,
    SidecarTransport, StartupStatusReport, TelegramChannelConfig,
};

#[test]
fn runner_global_config_rejects_empty_user_config_path() {
    let mut users = BTreeMap::new();
    users.insert(
        "alice".to_owned(),
        RunnerUserRegistration {
            config_path: "   ".to_owned(),
        },
    );
    let config = RunnerGlobalConfig {
        workspace_root: "/var/lib/oxydra".to_owned(),
        users,
        default_tier: SandboxTier::Container,
        ..RunnerGlobalConfig::default()
    };

    let error = config
        .validate()
        .expect_err("empty user config path should fail validation");
    assert_eq!(
        error,
        RunnerConfigError::InvalidUserConfigPath {
            user_id: "alice".to_owned(),
        }
    );
}

#[test]
fn runner_global_config_rejects_unsupported_config_major() {
    let config = RunnerGlobalConfig {
        config_version: "2.0.0".to_owned(),
        ..RunnerGlobalConfig::default()
    };

    let error = config
        .validate()
        .expect_err("unsupported config version should fail validation");
    assert_eq!(
        error,
        RunnerConfigError::UnsupportedConfigVersion {
            version: "2.0.0".to_owned(),
            supported_major: SUPPORTED_RUNNER_CONFIG_MAJOR_VERSION,
        }
    );
}

#[test]
fn runner_user_config_rejects_invalid_config_version_format() {
    let config = RunnerUserConfig {
        config_version: "v1".to_owned(),
        ..RunnerUserConfig::default()
    };

    let error = config
        .validate()
        .expect_err("invalid config version should fail validation");
    assert_eq!(
        error,
        RunnerConfigError::InvalidConfigVersionFormat {
            version: "v1".to_owned(),
        }
    );
}

#[test]
fn runner_user_config_rejects_invalid_timezone() {
    let mut config = RunnerUserConfig::default();
    config.behavior.timezone = "Mars/Olympus".to_owned();

    let error = config
        .validate()
        .expect_err("invalid timezone should fail validation");
    assert_eq!(
        error,
        RunnerConfigError::InvalidTimezone {
            timezone: "Mars/Olympus".to_owned(),
        }
    );
}

#[test]
fn runner_config_defaults_to_current_version() {
    assert_eq!(
        RunnerGlobalConfig::default().config_version,
        DEFAULT_RUNNER_CONFIG_VERSION
    );
    assert_eq!(
        RunnerUserConfig::default().config_version,
        DEFAULT_RUNNER_CONFIG_VERSION
    );
    assert_eq!(
        RunnerUserConfig::default().behavior.timezone,
        DEFAULT_RUNNER_TIMEZONE
    );
}

#[test]
fn runner_user_config_rejects_invalid_resource_limits() {
    let config = RunnerUserConfig {
        resources: RunnerResourceLimits {
            max_vcpus: Some(0),
            max_memory_mib: Some(512),
            max_processes: None,
        },
        ..RunnerUserConfig::default()
    };

    let error = config
        .validate()
        .expect_err("zero resource limit should fail validation");
    assert_eq!(
        error,
        RunnerConfigError::InvalidResourceLimit {
            field: "max_vcpus",
            value: 0,
        }
    );
}

#[test]
fn bootstrap_envelope_supports_length_prefixed_round_trip() {
    let envelope = RunnerBootstrapEnvelope {
        user_id: "user-1".to_owned(),
        sandbox_tier: SandboxTier::MicroVm,
        workspace_root: "/workspace/user-1".to_owned(),
        sidecar_endpoint: Some(SidecarEndpoint {
            transport: SidecarTransport::Unix,
            address: "/tmp/shell-daemon.sock".to_owned(),
        }),
        runtime_policy: Some(RunnerRuntimePolicy {
            mounts: RunnerResolvedMountPaths {
                shared: "/workspace/user-1/shared".to_owned(),
                tmp: "/workspace/user-1/tmp".to_owned(),
                vault: "/workspace/user-1/vault".to_owned(),
            },
            resources: RunnerResourceLimits {
                max_vcpus: Some(2),
                max_memory_mib: Some(1024),
                max_processes: Some(32),
            },
            credential_refs: BTreeMap::from([
                ("github".to_owned(), "vault://github/token".to_owned()),
                ("slack".to_owned(), "vault://slack/token".to_owned()),
            ]),
        }),
        startup_status: Some(StartupStatusReport {
            sandbox_tier: SandboxTier::MicroVm,
            sidecar_available: true,
            shell_available: true,
            browser_available: true,
            degraded_reasons: Vec::new(),
        }),
        channels: None,
        browser_config: None,
    };

    let encoded = envelope
        .to_length_prefixed_json()
        .expect("framed envelope should encode");
    let decoded = RunnerBootstrapEnvelope::from_length_prefixed_json(&encoded)
        .expect("framed envelope should decode");
    assert_eq!(decoded, envelope);
}

#[test]
fn bootstrap_envelope_rejects_invalid_length_prefix() {
    let envelope = RunnerBootstrapEnvelope {
        user_id: "user-1".to_owned(),
        sandbox_tier: SandboxTier::Container,
        workspace_root: "/workspace/user-1".to_owned(),
        sidecar_endpoint: None,
        runtime_policy: None,
        startup_status: None,
        channels: None,
        browser_config: None,
    };

    let mut encoded = envelope
        .to_length_prefixed_json()
        .expect("framed envelope should encode");
    let prefixed_len = u32::from_be_bytes(
        encoded[..4]
            .try_into()
            .expect("prefix should be four bytes"),
    );
    encoded[..4].copy_from_slice(&(prefixed_len + 1).to_be_bytes());

    let error = RunnerBootstrapEnvelope::from_length_prefixed_json(&encoded)
        .expect_err("mismatched prefix should fail");
    assert!(matches!(
        error,
        BootstrapEnvelopeError::LengthPrefixMismatch { .. }
    ));
}

#[test]
fn bootstrap_envelope_accepts_process_tier_shell_without_sidecar() {
    let envelope = RunnerBootstrapEnvelope {
        user_id: "user-1".to_owned(),
        sandbox_tier: SandboxTier::Process,
        workspace_root: "/workspace/user-1".to_owned(),
        sidecar_endpoint: None,
        runtime_policy: None,
        startup_status: Some(StartupStatusReport {
            sandbox_tier: SandboxTier::Process,
            sidecar_available: false,
            shell_available: true,
            browser_available: false,
            degraded_reasons: Vec::new(),
        }),
        channels: None,
        browser_config: None,
    };

    let encoded = envelope
        .to_length_prefixed_json()
        .expect("process-tier shell availability without a sidecar should encode");
    let decoded = RunnerBootstrapEnvelope::from_length_prefixed_json(&encoded)
        .expect("process-tier shell availability without a sidecar should decode");
    assert_eq!(decoded, envelope);
}

#[test]
fn bootstrap_envelope_rejects_process_tier_browser_available() {
    let envelope = RunnerBootstrapEnvelope {
        user_id: "user-1".to_owned(),
        sandbox_tier: SandboxTier::Process,
        workspace_root: "/workspace/user-1".to_owned(),
        sidecar_endpoint: None,
        runtime_policy: None,
        startup_status: Some(StartupStatusReport {
            sandbox_tier: SandboxTier::Process,
            sidecar_available: false,
            shell_available: false,
            browser_available: true,
            degraded_reasons: Vec::new(),
        }),
        channels: None,
        browser_config: None,
    };

    let result = envelope.to_length_prefixed_json();
    assert!(matches!(
        result,
        Err(BootstrapEnvelopeError::InvalidField { field })
            if field == "startup_status.browser_available"
    ));
}

#[test]
fn bootstrap_envelope_rejects_invalid_runtime_policy_mounts() {
    let envelope = RunnerBootstrapEnvelope {
        user_id: "user-1".to_owned(),
        sandbox_tier: SandboxTier::Container,
        workspace_root: "/workspace/user-1".to_owned(),
        sidecar_endpoint: None,
        runtime_policy: Some(RunnerRuntimePolicy {
            mounts: RunnerResolvedMountPaths {
                shared: String::new(),
                tmp: "/workspace/user-1/tmp".to_owned(),
                vault: "/workspace/user-1/vault".to_owned(),
            },
            resources: RunnerResourceLimits::default(),
            credential_refs: BTreeMap::new(),
        }),
        startup_status: None,
        channels: None,
        browser_config: None,
    };

    let error = envelope
        .to_length_prefixed_json()
        .expect_err("invalid runtime policy mounts should fail bootstrap encoding");
    assert!(matches!(
        error,
        BootstrapEnvelopeError::InvalidRuntimePolicy { .. }
    ));
}

#[test]
fn bootstrap_envelope_rejects_inconsistent_startup_status() {
    let envelope = RunnerBootstrapEnvelope {
        user_id: "user-1".to_owned(),
        sandbox_tier: SandboxTier::Container,
        workspace_root: "/workspace/user-1".to_owned(),
        sidecar_endpoint: None,
        runtime_policy: None,
        startup_status: Some(StartupStatusReport {
            sandbox_tier: SandboxTier::Container,
            sidecar_available: true,
            shell_available: true,
            browser_available: false,
            degraded_reasons: Vec::new(),
        }),
        channels: None,
        browser_config: None,
    };

    let error = envelope
        .to_length_prefixed_json()
        .expect_err("startup status claiming sidecar availability must fail without endpoint");
    assert!(matches!(
        error,
        BootstrapEnvelopeError::InvalidField {
            field: "startup_status.sidecar_available"
        }
    ));
}

// ── Channel Config Tests ────────────────────────────────────────────────────

#[test]
fn channels_config_is_optional_and_defaults_empty() {
    let config: RunnerUserConfig = toml::from_str("").expect("empty config should parse");
    assert!(config.channels.is_empty());
    assert!(config.channels.telegram.is_none());
}

#[test]
fn channels_config_with_telegram_round_trips_through_toml() {
    let toml_str = r#"
[channels.telegram]
enabled = true
bot_token_env = "ALICE_TELEGRAM_BOT_TOKEN"
polling_timeout_secs = 60
max_message_length = 2048

[[channels.telegram.senders]]
platform_ids = ["12345678"]
display_name = "Alice"

[[channels.telegram.senders]]
platform_ids = ["87654321", "11223344"]
display_name = "Bob"
"#;
    let config: RunnerUserConfig = toml::from_str(toml_str).expect("telegram config should parse");
    let telegram = config
        .channels
        .telegram
        .as_ref()
        .expect("telegram config should be present");
    assert!(telegram.enabled);
    assert_eq!(
        telegram.bot_token_env.as_deref(),
        Some("ALICE_TELEGRAM_BOT_TOKEN")
    );
    assert_eq!(telegram.polling_timeout_secs, 60);
    assert_eq!(telegram.max_message_length, 2048);
    assert_eq!(telegram.senders.len(), 2);
    assert_eq!(telegram.senders[0].platform_ids, vec!["12345678"]);
    assert_eq!(telegram.senders[0].display_name.as_deref(), Some("Alice"));
    assert_eq!(
        telegram.senders[1].platform_ids,
        vec!["87654321", "11223344"]
    );
}

#[test]
fn channels_config_defaults_for_telegram_fields() {
    let toml_str = r#"
[channels.telegram]
enabled = false
"#;
    let config: RunnerUserConfig = toml::from_str(toml_str).expect("minimal telegram should parse");
    let telegram = config.channels.telegram.unwrap();
    assert!(!telegram.enabled);
    assert!(telegram.bot_token_env.is_none());
    assert_eq!(telegram.polling_timeout_secs, 30); // default
    assert_eq!(telegram.max_message_length, 4096); // default
    assert!(telegram.senders.is_empty());
}

#[test]
fn empty_senders_list_means_nobody_can_interact() {
    let config = TelegramChannelConfig {
        enabled: true,
        bot_token_env: Some("TOKEN".to_owned()),
        polling_timeout_secs: 30,
        senders: vec![],
        max_message_length: 4096,
    };
    let channels = ChannelsConfig {
        telegram: Some(config),
    };
    // bot_token_env is still collected because the channel is enabled
    assert_eq!(channels.bot_token_env_refs(), vec!["TOKEN"]);
    // But the senders list is empty — no one can interact
}

#[test]
fn bot_token_env_refs_only_from_enabled_channels() {
    let disabled = ChannelsConfig {
        telegram: Some(TelegramChannelConfig {
            enabled: false,
            bot_token_env: Some("DISABLED_TOKEN".to_owned()),
            polling_timeout_secs: 30,
            senders: vec![],
            max_message_length: 4096,
        }),
    };
    assert!(
        disabled.bot_token_env_refs().is_empty(),
        "disabled channel should not contribute env refs"
    );

    let enabled = ChannelsConfig {
        telegram: Some(TelegramChannelConfig {
            enabled: true,
            bot_token_env: Some("ENABLED_TOKEN".to_owned()),
            polling_timeout_secs: 30,
            senders: vec![],
            max_message_length: 4096,
        }),
    };
    assert_eq!(enabled.bot_token_env_refs(), vec!["ENABLED_TOKEN"]);
}

#[test]
fn sender_binding_without_display_name() {
    let json = r#"{"platform_ids":["12345678"]}"#;
    let binding: SenderBinding =
        serde_json::from_str(json).expect("binding without display_name should parse");
    assert_eq!(binding.platform_ids, vec!["12345678"]);
    assert!(binding.display_name.is_none());
}

// ── Log types tests ─────────────────────────────────────────────────────────

#[test]
fn runner_control_logs_request_defaults() {
    let json = r#"{"op":"logs","payload":{}}"#;
    let request: RunnerControl =
        serde_json::from_str(json).expect("logs request with defaults should parse");
    if let RunnerControl::Logs(logs) = request {
        assert_eq!(logs.role, LogRole::Runtime);
        assert_eq!(logs.stream, LogStream::Both);
        assert_eq!(logs.tail, None);
        assert_eq!(logs.since, None);
        assert_eq!(logs.format, LogFormat::Text);
    } else {
        panic!("expected Logs variant");
    }
}

#[test]
fn log_entry_text_format() {
    let entry = RunnerLogEntry {
        timestamp: Some("2026-03-01T10:55:12Z".to_owned()),
        source: LogSource::DockerApi,
        role: "oxydra-vm".to_owned(),
        stream: "stderr".to_owned(),
        message: "gateway bind failed: port in use".to_owned(),
    };
    let text = entry.to_text_line();
    assert!(text.contains("2026-03-01T10:55:12Z"));
    assert!(text.contains("[docker_api]"));
    assert!(text.contains("[oxydra-vm]"));
    assert!(text.contains("[stderr]"));
    assert!(text.contains("gateway bind failed: port in use"));
}

#[test]
fn log_entry_text_format_without_timestamp() {
    let entry = RunnerLogEntry {
        timestamp: None,
        source: LogSource::ProcessFile,
        role: "shell-vm".to_owned(),
        stream: "stdout".to_owned(),
        message: "session started".to_owned(),
    };
    let text = entry.to_text_line();
    assert!(text.starts_with("- [process_file]"));
}

#[test]
fn logs_request_effective_tail_clamps_to_max() {
    let request = RunnerControlLogsRequest {
        role: LogRole::Runtime,
        stream: LogStream::Both,
        tail: Some(5000),
        since: None,
        format: LogFormat::Text,
    };
    assert_eq!(request.effective_tail(), LOG_TAIL_MAX);
}

#[test]
fn logs_request_effective_tail_uses_default() {
    let request = RunnerControlLogsRequest {
        role: LogRole::Runtime,
        stream: LogStream::Both,
        tail: None,
        since: None,
        format: LogFormat::Text,
    };
    assert_eq!(request.effective_tail(), LOG_TAIL_DEFAULT);
}

#[test]
fn logs_request_effective_tail_preserves_small_value() {
    let request = RunnerControlLogsRequest {
        role: LogRole::Runtime,
        stream: LogStream::Both,
        tail: Some(50),
        since: None,
        format: LogFormat::Text,
    };
    assert_eq!(request.effective_tail(), 50);
}
