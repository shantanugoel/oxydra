use std::collections::BTreeMap;

use types::{
    BootstrapEnvelopeError, ChannelsConfig, ExecCommand, RunnerBootstrapEnvelope,
    RunnerConfigError, RunnerControl, RunnerControlError, RunnerControlErrorCode,
    RunnerControlHealthStatus, RunnerControlResponse, RunnerControlShutdownStatus,
    RunnerGlobalConfig, RunnerResolvedMountPaths, RunnerResourceLimits, RunnerRuntimePolicy,
    RunnerUserConfig, RunnerUserRegistration, SandboxTier, SenderBinding, ShellDaemonRequest,
    SidecarEndpoint, SidecarTransport, StartupDegradedReason, StartupDegradedReasonCode,
    StartupStatusReport, TelegramChannelConfig,
};

#[test]
fn sandbox_tier_uses_snake_case_serde_labels() {
    let encoded = serde_json::to_string(&SandboxTier::MicroVm).expect("tier should serialize");
    assert_eq!(encoded, "\"micro_vm\"");
}

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
fn runner_global_config_round_trips_through_serde() {
    let mut users = BTreeMap::new();
    users.insert(
        "alice".to_owned(),
        RunnerUserRegistration {
            config_path: "/etc/oxydra/users/alice.toml".to_owned(),
        },
    );
    let config = RunnerGlobalConfig {
        workspace_root: "/var/lib/oxydra".to_owned(),
        users,
        default_tier: SandboxTier::Container,
        ..RunnerGlobalConfig::default()
    };

    let encoded = serde_json::to_string(&config).expect("runner global config should serialize");
    let decoded: RunnerGlobalConfig =
        serde_json::from_str(&encoded).expect("runner global config should deserialize");
    assert_eq!(decoded, config);
    assert!(decoded.validate().is_ok());
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
fn shell_daemon_exec_command_request_round_trips() {
    let request = ShellDaemonRequest::ExecCommand(ExecCommand {
        request_id: "req-1".to_owned(),
        session_id: "session-42".to_owned(),
        command: "echo hi".to_owned(),
        timeout_secs: Some(30),
    });

    let encoded = serde_json::to_string(&request).expect("request should serialize");
    let decoded: ShellDaemonRequest =
        serde_json::from_str(&encoded).expect("request should deserialize");
    assert_eq!(decoded, request);
}

#[test]
fn runner_control_request_round_trips() {
    let request = RunnerControl::ShutdownUser {
        user_id: "alice".to_owned(),
    };
    let encoded = serde_json::to_string(&request).expect("runner control should serialize");
    let decoded: RunnerControl =
        serde_json::from_str(&encoded).expect("runner control should deserialize");
    assert_eq!(decoded, request);
}

#[test]
fn runner_control_response_round_trips() {
    let response = RunnerControlResponse::HealthStatus(RunnerControlHealthStatus {
        user_id: "alice".to_owned(),
        healthy: true,
        sandbox_tier: SandboxTier::Container,
        startup_status: StartupStatusReport {
            sandbox_tier: SandboxTier::Container,
            sidecar_available: true,
            shell_available: true,
            browser_available: false,
            degraded_reasons: vec![StartupDegradedReason::new(
                StartupDegradedReasonCode::SidecarProtocolError,
                "sidecar protocol handshake failed",
            )],
        },
        shell_available: true,
        browser_available: false,
        shutdown: false,
        message: Some("ready".to_owned()),
        log_dir: Some("/tmp/workspaces/alice/logs".to_owned()),
        runtime_pid: Some(12345),
        runtime_container_name: None,
    });
    let encoded = serde_json::to_string(&response).expect("runner control response should encode");
    let decoded: RunnerControlResponse =
        serde_json::from_str(&encoded).expect("runner control response should decode");
    assert_eq!(decoded, response);
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

#[test]
fn runner_control_error_response_round_trips() {
    let response = RunnerControlResponse::Error(RunnerControlError {
        code: RunnerControlErrorCode::UnknownUser,
        message: "unknown user `bob`".to_owned(),
    });
    let encoded = serde_json::to_string(&response).expect("runner control error should encode");
    let decoded: RunnerControlResponse =
        serde_json::from_str(&encoded).expect("runner control error should decode");
    assert_eq!(decoded, response);
}

#[test]
fn runner_control_shutdown_response_round_trips() {
    let response = RunnerControlResponse::ShutdownStatus(RunnerControlShutdownStatus {
        user_id: "alice".to_owned(),
        shutdown: true,
        already_stopped: false,
        message: Some("shutdown completed".to_owned()),
    });
    let encoded = serde_json::to_string(&response).expect("shutdown status should encode");
    let decoded: RunnerControlResponse =
        serde_json::from_str(&encoded).expect("shutdown status should decode");
    assert_eq!(decoded, response);
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
fn bootstrap_envelope_with_channels_round_trips() {
    let envelope = RunnerBootstrapEnvelope {
        user_id: "alice".to_owned(),
        sandbox_tier: SandboxTier::Container,
        workspace_root: "/workspace/alice".to_owned(),
        sidecar_endpoint: None,
        runtime_policy: None,
        startup_status: None,
        channels: Some(ChannelsConfig {
            telegram: Some(TelegramChannelConfig {
                enabled: true,
                bot_token_env: Some("ALICE_BOT_TOKEN".to_owned()),
                polling_timeout_secs: 30,
                senders: vec![
                    SenderBinding {
                        platform_ids: vec!["12345678".to_owned()],
                        display_name: Some("Alice".to_owned()),
                    },
                    SenderBinding {
                        platform_ids: vec!["87654321".to_owned()],
                        display_name: None,
                    },
                ],
                max_message_length: 4096,
            }),
        }),
    };

    let encoded = envelope
        .to_length_prefixed_json()
        .expect("envelope with channels should encode");
    let decoded = RunnerBootstrapEnvelope::from_length_prefixed_json(&encoded)
        .expect("envelope with channels should decode");
    assert_eq!(decoded, envelope);
    let channels = decoded.channels.expect("channels should be present");
    let telegram = channels.telegram.expect("telegram should be present");
    assert!(telegram.enabled);
    assert_eq!(telegram.senders.len(), 2);
}

#[test]
fn bootstrap_envelope_without_channels_round_trips() {
    let envelope = RunnerBootstrapEnvelope {
        user_id: "bob".to_owned(),
        sandbox_tier: SandboxTier::Process,
        workspace_root: "/workspace/bob".to_owned(),
        sidecar_endpoint: None,
        runtime_policy: None,
        startup_status: None,
        channels: None,
    };

    let encoded = envelope
        .to_length_prefixed_json()
        .expect("envelope without channels should encode");
    let decoded = RunnerBootstrapEnvelope::from_length_prefixed_json(&encoded)
        .expect("envelope without channels should decode");
    assert_eq!(decoded, envelope);
    assert!(decoded.channels.is_none());
}

#[test]
fn sender_binding_serde_round_trip() {
    let binding = SenderBinding {
        platform_ids: vec!["aaa".to_owned(), "bbb".to_owned()],
        display_name: Some("Test User".to_owned()),
    };
    let json = serde_json::to_string(&binding).expect("binding should serialize");
    let decoded: SenderBinding =
        serde_json::from_str(&json).expect("binding should deserialize");
    assert_eq!(decoded, binding);
}

#[test]
fn sender_binding_without_display_name() {
    let json = r#"{"platform_ids":["12345678"]}"#;
    let binding: SenderBinding =
        serde_json::from_str(json).expect("binding without display_name should parse");
    assert_eq!(binding.platform_ids, vec!["12345678"]);
    assert!(binding.display_name.is_none());
}
