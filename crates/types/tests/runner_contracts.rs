use std::collections::BTreeMap;

use types::{
    BootstrapEnvelopeError, ExecCommand, RunnerBootstrapEnvelope, RunnerConfigError, RunnerControl,
    RunnerControlError, RunnerControlErrorCode, RunnerControlHealthStatus, RunnerControlResponse,
    RunnerControlShutdownStatus, RunnerGlobalConfig, RunnerResolvedMountPaths,
    RunnerResourceLimits, RunnerRuntimePolicy, RunnerUserConfig, RunnerUserRegistration,
    SandboxTier, ShellDaemonRequest, SidecarEndpoint, SidecarTransport, StartupDegradedReason,
    StartupDegradedReasonCode, StartupStatusReport,
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
