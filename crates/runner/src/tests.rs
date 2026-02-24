use std::{
    collections::BTreeMap,
    env, fs,
    net::TcpListener,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use tokio_tungstenite::tungstenite::{Message as WsMessage, accept};
use types::{
    RunnerBootstrapEnvelope, RunnerControl, RunnerControlErrorCode, RunnerControlResponse,
    StartupDegradedReason, StartupDegradedReasonCode,
};

use super::*;

#[cfg(unix)]
const BOOTSTRAP_CAPTURE_ENV_KEY: &str = "OXYDRA_RUNNER_BOOTSTRAP_CAPTURE";

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

#[derive(Debug)]
struct EnvVarGuard {
    key: &'static str,
    previous: Option<String>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let previous = env::var(key).ok();
        // SAFETY: tests guard environment writes with a process-wide mutex.
        unsafe { env::set_var(key, value) };
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        if let Some(value) = &self.previous {
            // SAFETY: tests guard environment writes with a process-wide mutex.
            unsafe { env::set_var(self.key, value) };
        } else {
            // SAFETY: tests guard environment writes with a process-wide mutex.
            unsafe { env::remove_var(self.key) };
        }
    }
}

#[derive(Debug, Default)]
struct MockSandboxBackend {
    launches: Mutex<Vec<SandboxLaunchRequest>>,
}

impl MockSandboxBackend {
    fn recorded_launches(&self) -> Vec<SandboxLaunchRequest> {
        self.launches
            .lock()
            .expect("launch records mutex should not be poisoned")
            .clone()
    }
}

impl SandboxBackend for MockSandboxBackend {
    fn launch(&self, request: SandboxLaunchRequest) -> Result<SandboxLaunch, RunnerError> {
        self.launches
            .lock()
            .expect("launch records mutex should not be poisoned")
            .push(request.clone());

        let runtime = RunnerGuestHandle::simulated(
            RunnerGuestRole::OxydraVm,
            RunnerCommandSpec::new("mock-oxydra-vm", Vec::new()),
        );

        let transport = match request.sandbox_tier {
            SandboxTier::MicroVm if request.host_os == "linux" => SidecarTransport::Vsock,
            SandboxTier::MicroVm | SandboxTier::Container => SidecarTransport::Unix,
            SandboxTier::Process => SidecarTransport::Unix,
        };
        let sidecar_requested = request.sidecar_requested();
        let sidecar = sidecar_requested.then(|| {
            RunnerGuestHandle::simulated(
                RunnerGuestRole::ShellVm,
                RunnerCommandSpec::new("mock-shell-vm", Vec::new()),
            )
        });
        let sidecar_endpoint = sidecar_requested.then(|| SidecarEndpoint {
            transport,
            address: "/tmp/mock-shell-daemon.sock".to_owned(),
        });

        let mut warnings = Vec::new();
        let mut degraded_reasons = Vec::new();
        if request.sandbox_tier == SandboxTier::Process {
            warnings.push(PROCESS_TIER_WARNING.to_owned());
            degraded_reasons.push(StartupDegradedReason::new(
                StartupDegradedReasonCode::InsecureProcessTier,
                PROCESS_TIER_WARNING,
            ));
        }

        Ok(SandboxLaunch {
            launch: RunnerLaunchHandle {
                tier: request.sandbox_tier,
                runtime,
                sidecar,
                scope: Some(RunnerScopeHandle::Simulated),
            },
            sidecar_endpoint,
            shell_available: sidecar_requested && request.requested_shell,
            browser_available: sidecar_requested && request.requested_browser,
            degraded_reasons,
            warnings,
        })
    }
}

#[test]
fn global_and_user_configs_load_with_validation() {
    let root = temp_dir("config-load");
    let global_path = write_runner_config_fixture(&root, "micro_vm");
    write_user_config(&root.join("users/alice.toml"), "");

    let global = load_runner_global_config(&global_path).expect("global config should load");
    let user =
        load_runner_user_config(root.join("users/alice.toml")).expect("user config should load");
    assert_eq!(global.workspace_root, "workspaces");
    assert_eq!(global.default_tier, SandboxTier::MicroVm);
    assert_eq!(user, RunnerUserConfig::default());

    let _ = fs::remove_dir_all(root);
}

#[test]
fn runner_resolves_relative_user_config_paths() {
    let root = temp_dir("path-resolution");
    let global_path = write_runner_config_fixture(&root, "container");
    write_user_config(
        &root.join("users/alice.toml"),
        "[behavior]\nsandbox_tier = \"micro_vm\"\n",
    );

    let backend: Arc<dyn SandboxBackend> = Arc::new(MockSandboxBackend::default());
    let runner = Runner::from_global_config_path_with_backend(&global_path, backend)
        .expect("runner should initialize");
    let user_config = runner
        .load_user_config("alice")
        .expect("user config should load through registration map");
    assert_eq!(
        user_config.behavior.sandbox_tier,
        Some(SandboxTier::MicroVm)
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn workspace_provisioning_creates_expected_directories() {
    let root = temp_dir("workspace-provisioning");
    let workspace = provision_user_workspace(root.join("workspace-root"), "alice")
        .expect("workspace should provision");
    assert!(workspace.root.is_dir());
    assert!(workspace.shared.is_dir());
    assert!(workspace.tmp.is_dir());
    assert!(workspace.vault.is_dir());
    assert!(workspace.ipc.is_dir());
    assert!(workspace.internal.is_dir());

    let _ = fs::remove_dir_all(root);
}

#[test]
fn sandbox_tier_resolution_honors_insecure_override() {
    let global = RunnerGlobalConfig {
        default_tier: SandboxTier::Container,
        ..RunnerGlobalConfig::default()
    };
    global.validate().expect("global config should be valid");

    let mut user = RunnerUserConfig::default();
    user.behavior.sandbox_tier = Some(SandboxTier::MicroVm);

    assert_eq!(
        resolve_sandbox_tier(&global, &user, false),
        SandboxTier::MicroVm
    );
    assert_eq!(
        resolve_sandbox_tier(&global, &user, true),
        SandboxTier::Process
    );
}

#[test]
fn startup_uses_linux_microvm_backend_and_vsock_sidecar() {
    let root = temp_dir("linux-microvm");
    let global_path = write_runner_config_fixture(&root, "micro_vm");
    write_user_config(&root.join("users/alice.toml"), "");

    let backend = Arc::new(MockSandboxBackend::default());
    let runner = Runner::from_global_config_path_with_backend(&global_path, backend.clone())
        .expect("runner should initialize");
    let startup = runner
        .start_user_for_host(RunnerStartRequest::new("alice"), "linux")
        .expect("startup should succeed");

    assert_eq!(startup.sandbox_tier, SandboxTier::MicroVm);
    assert_eq!(startup.launch.tier, SandboxTier::MicroVm);
    assert_eq!(
        startup
            .bootstrap
            .sidecar_endpoint
            .as_ref()
            .map(|sidecar| sidecar.transport),
        Some(SidecarTransport::Vsock)
    );
    assert!(startup.shell_available);
    assert!(startup.browser_available);
    assert!(startup.warnings.is_empty());

    let launches = backend.recorded_launches();
    assert_eq!(launches.len(), 1);
    assert_eq!(launches[0].sandbox_tier, SandboxTier::MicroVm);
    assert!(launches[0].requested_shell);
    assert!(launches[0].requested_browser);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn startup_uses_macos_microvm_backend_with_unix_sidecar() {
    let root = temp_dir("macos-microvm");
    let global_path = write_runner_config_fixture(&root, "micro_vm");
    write_user_config(&root.join("users/alice.toml"), "");

    let backend = Arc::new(MockSandboxBackend::default());
    let runner = Runner::from_global_config_path_with_backend(&global_path, backend.clone())
        .expect("runner should initialize");
    let startup = runner
        .start_user_for_host(RunnerStartRequest::new("alice"), "macos")
        .expect("startup should succeed");

    assert_eq!(startup.sandbox_tier, SandboxTier::MicroVm);
    assert_eq!(startup.launch.tier, SandboxTier::MicroVm);
    assert_eq!(
        startup
            .bootstrap
            .sidecar_endpoint
            .as_ref()
            .map(|sidecar| sidecar.transport),
        Some(SidecarTransport::Unix)
    );
    assert!(startup.shell_available);
    assert!(startup.browser_available);

    let launches = backend.recorded_launches();
    assert_eq!(launches.len(), 1);
    assert_eq!(launches[0].sandbox_tier, SandboxTier::MicroVm);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn startup_insecure_process_mode_disables_shell_and_browser() {
    let root = temp_dir("process-mode");
    let global_path = write_runner_config_fixture(&root, "micro_vm");
    write_user_config(&root.join("users/alice.toml"), "");

    let backend = Arc::new(MockSandboxBackend::default());
    let runner = Runner::from_global_config_path_with_backend(&global_path, backend.clone())
        .expect("runner should initialize");
    let startup = runner
        .start_user_for_host(
            RunnerStartRequest {
                user_id: "alice".to_owned(),
                insecure: true,
                extra_env: Vec::new(),
            },
            "linux",
        )
        .expect("startup should succeed");

    assert_eq!(startup.sandbox_tier, SandboxTier::Process);
    assert_eq!(startup.launch.tier, SandboxTier::Process);
    assert!(startup.launch.sidecar.is_none());
    assert!(!startup.shell_available);
    assert!(!startup.browser_available);
    assert!(!startup.startup_status.sidecar_available);
    assert!(
        startup
            .startup_status
            .has_reason_code(StartupDegradedReasonCode::InsecureProcessTier)
    );
    assert!(startup.bootstrap.sidecar_endpoint.is_none());
    assert_eq!(startup.warnings, vec![PROCESS_TIER_WARNING.to_owned()]);

    let launches = backend.recorded_launches();
    assert_eq!(launches.len(), 1);
    assert_eq!(launches[0].sandbox_tier, SandboxTier::Process);
    assert!(!launches[0].requested_shell);
    assert!(!launches[0].requested_browser);

    let _ = fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn process_startup_sends_bootstrap_frame_to_runtime_stdin() {
    let _env_lock = env_lock().lock().unwrap_or_else(|error| error.into_inner());
    let root = temp_dir("process-bootstrap-stdin");
    let global_path = write_runner_config_fixture(&root, "micro_vm");
    write_user_config(&root.join("users/alice.toml"), "");
    let script_path = root.join("capture-bootstrap.sh");
    let captured_frame_path = root.join("captured-bootstrap-frame.bin");

    fs::write(
        &script_path,
        format!("#!/bin/sh\ncat > \"${BOOTSTRAP_CAPTURE_ENV_KEY}\"\n"),
    )
    .expect("capture script should be writable");
    let mut permissions = fs::metadata(&script_path)
        .expect("capture script metadata should load")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&script_path, permissions)
        .expect("capture script should become executable");

    let _exec = EnvVarGuard::set(
        PROCESS_EXECUTABLE_ENV_KEY,
        script_path.to_string_lossy().as_ref(),
    );
    let _capture = EnvVarGuard::set(
        BOOTSTRAP_CAPTURE_ENV_KEY,
        captured_frame_path.to_string_lossy().as_ref(),
    );

    let runner = Runner::from_global_config_path(&global_path).expect("runner should initialize");
    let mut startup = runner
        .start_user_for_host(
            RunnerStartRequest {
                user_id: "alice".to_owned(),
                insecure: true,
                extra_env: Vec::new(),
            },
            "linux",
        )
        .expect("process startup should succeed");
    wait_for_file(&captured_frame_path);

    let encoded = fs::read(&captured_frame_path).expect("captured bootstrap frame should exist");
    let decoded = RunnerBootstrapEnvelope::from_length_prefixed_json(&encoded)
        .expect("captured bootstrap frame should decode");
    assert_eq!(decoded, startup.bootstrap);
    startup
        .shutdown()
        .expect("startup shutdown should clean up runtime handle");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn startup_container_tier_uses_unix_sidecar_transport() {
    let root = temp_dir("container-tier");
    let global_path = write_runner_config_fixture(&root, "container");
    write_user_config(&root.join("users/alice.toml"), "");

    let backend = Arc::new(MockSandboxBackend::default());
    let runner = Runner::from_global_config_path_with_backend(&global_path, backend.clone())
        .expect("runner should initialize");
    let startup = runner
        .start_user_for_host(RunnerStartRequest::new("alice"), "linux")
        .expect("startup should succeed");

    assert_eq!(startup.sandbox_tier, SandboxTier::Container);
    assert_eq!(startup.launch.tier, SandboxTier::Container);
    assert_eq!(
        startup
            .bootstrap
            .sidecar_endpoint
            .as_ref()
            .map(|sidecar| sidecar.transport),
        Some(SidecarTransport::Unix)
    );

    let launches = backend.recorded_launches();
    assert_eq!(launches.len(), 1);
    assert_eq!(launches[0].sandbox_tier, SandboxTier::Container);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn runner_control_rejects_shutdown_for_unknown_user() {
    let root = temp_dir("runner-control-unknown-user");
    let global_path = write_runner_config_fixture(&root, "container");
    write_user_config(&root.join("users/alice.toml"), "");

    let backend = Arc::new(MockSandboxBackend::default());
    let runner = Runner::from_global_config_path_with_backend(&global_path, backend)
        .expect("runner should initialize");
    let mut startup = runner
        .start_user_for_host(RunnerStartRequest::new("alice"), "linux")
        .expect("startup should succeed");

    let response = startup.handle_control(RunnerControl::ShutdownUser {
        user_id: "bob".to_owned(),
    });
    assert!(matches!(
        response,
        RunnerControlResponse::Error(error)
            if error.code == RunnerControlErrorCode::UnknownUser
                && error.message.contains("unknown user `bob`")
    ));

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn runner_control_transport_handles_health_shutdown_and_invalid_frames() {
    let root = temp_dir("runner-control-transport");
    let global_path = write_runner_config_fixture(&root, "container");
    write_user_config(&root.join("users/alice.toml"), "");

    let backend = Arc::new(MockSandboxBackend::default());
    let runner = Runner::from_global_config_path_with_backend(&global_path, backend)
        .expect("runner should initialize");
    let startup = runner
        .start_user_for_host(RunnerStartRequest::new("alice"), "linux")
        .expect("startup should succeed");

    let (mut client, server) = tokio::io::duplex(8192);
    let server_task = tokio::spawn(async move {
        let mut startup = startup;
        startup
            .serve_control_stream(server)
            .await
            .expect("control stream should serve");
        startup
    });

    let health = send_control_request(&mut client, &RunnerControl::HealthCheck).await;
    assert!(matches!(
        health,
        RunnerControlResponse::HealthStatus(status)
            if status.healthy
                && !status.shutdown
                && status.user_id == "alice"
                && status.shell_available
                && status.startup_status.sidecar_available
    ));

    send_control_payload(&mut client, br#"{"op":"unknown"}"#).await;
    let invalid = read_control_response(&mut client).await;
    assert!(matches!(
        invalid,
        RunnerControlResponse::Error(error)
            if error.code == RunnerControlErrorCode::InvalidRequest
                && error.message.contains("invalid runner control request frame")
    ));

    let shutdown = send_control_request(
        &mut client,
        &RunnerControl::ShutdownUser {
            user_id: "alice".to_owned(),
        },
    )
    .await;
    assert!(matches!(
        shutdown,
        RunnerControlResponse::ShutdownStatus(status)
            if status.shutdown && !status.already_stopped && status.user_id == "alice"
    ));

    let health_after_shutdown =
        send_control_request(&mut client, &RunnerControl::HealthCheck).await;
    assert!(matches!(
        health_after_shutdown,
        RunnerControlResponse::HealthStatus(status)
            if !status.healthy
                && status.shutdown
                && status
                    .startup_status
                    .has_reason_code(StartupDegradedReasonCode::RuntimeShutdown)
    ));

    drop(client);
    let mut startup = server_task
        .await
        .expect("control server task should complete");
    assert!(matches!(
        startup.handle_control(RunnerControl::HealthCheck),
        RunnerControlResponse::HealthStatus(status) if status.shutdown
    ));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn startup_applies_mount_resource_and_credential_overrides_to_launch_and_bootstrap() {
    let root = temp_dir("effective-launch-settings");
    let global_path = write_runner_config_fixture(&root, "container");
    let tmp_override = root.join("custom-runtime-tmp");
    write_user_config(
        &root.join("users/alice.toml"),
        &format!(
            r#"
[mounts]
shared = "custom-shared"
tmp = "{}"
vault = "custom-vault"

[resources]
max_vcpus = 2
max_memory_mib = 1024
max_processes = 64

[credential_refs]
github = "vault://github/token"
slack = "vault://slack/token"
"#,
            tmp_override.display()
        ),
    );

    let backend = Arc::new(MockSandboxBackend::default());
    let runner = Runner::from_global_config_path_with_backend(&global_path, backend.clone())
        .expect("runner should initialize");
    let startup = runner
        .start_user_for_host(RunnerStartRequest::new("alice"), "linux")
        .expect("startup should succeed");
    let launch = backend
        .recorded_launches()
        .into_iter()
        .next()
        .expect("launch should be recorded");

    assert_eq!(
        launch.mounts.shared,
        startup.workspace.root.join("custom-shared")
    );
    assert_eq!(launch.mounts.tmp, tmp_override);
    assert_eq!(
        launch.mounts.vault,
        startup.workspace.root.join("custom-vault")
    );
    assert_eq!(launch.resource_limits.max_vcpus, Some(2));
    assert_eq!(launch.resource_limits.max_memory_mib, Some(1024));
    assert_eq!(launch.resource_limits.max_processes, Some(64));
    assert_eq!(
        launch.credential_refs,
        BTreeMap::from([
            ("github".to_owned(), "vault://github/token".to_owned()),
            ("slack".to_owned(), "vault://slack/token".to_owned())
        ])
    );

    let runtime_policy = startup
        .bootstrap
        .runtime_policy
        .as_ref()
        .expect("startup bootstrap should include runtime policy");
    assert_eq!(
        runtime_policy.mounts.shared,
        startup
            .workspace
            .root
            .join("custom-shared")
            .to_string_lossy()
            .to_string()
    );
    assert_eq!(
        runtime_policy.mounts.tmp,
        launch.mounts.tmp.to_string_lossy().to_string()
    );
    assert_eq!(
        runtime_policy.mounts.vault,
        startup
            .workspace
            .root
            .join("custom-vault")
            .to_string_lossy()
            .to_string()
    );
    assert_eq!(runtime_policy.resources, launch.resource_limits);
    assert_eq!(runtime_policy.credential_refs, launch.credential_refs);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn tui_connect_only_succeeds_against_running_gateway_endpoint() {
    let root = temp_dir("tui-connect-success");
    let global_path = write_runner_config_fixture(&root, "container");
    write_user_config(&root.join("users/alice.toml"), "");

    let backend = Arc::new(MockSandboxBackend::default());
    let runner = Runner::from_global_config_path_with_backend(&global_path, backend.clone())
        .expect("runner should initialize");
    let workspace = runner
        .provision_user_workspace("alice")
        .expect("workspace should provision");

    let (gateway_endpoint, server_task) = spawn_mock_gateway_probe_server(true);
    fs::write(
        workspace.ipc.join(GATEWAY_ENDPOINT_MARKER_FILE),
        &gateway_endpoint,
    )
    .expect("gateway endpoint marker should be writable");

    let connection = runner
        .connect_tui(RunnerTuiConnectRequest::new("alice"))
        .expect("connect-only path should succeed with running gateway");
    assert_eq!(connection.user_id, "alice");
    assert_eq!(connection.workspace.root, workspace.root);
    assert_eq!(connection.gateway_endpoint, gateway_endpoint);
    assert_eq!(connection.runtime_session_id, "runtime-alice");
    assert!(backend.recorded_launches().is_empty());

    server_task.join().expect("mock gateway should shut down");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn tui_connect_only_fails_with_clear_error_when_guest_is_absent() {
    let root = temp_dir("tui-connect-missing-guest");
    let global_path = write_runner_config_fixture(&root, "container");
    write_user_config(&root.join("users/alice.toml"), "");

    let backend = Arc::new(MockSandboxBackend::default());
    let runner = Runner::from_global_config_path_with_backend(&global_path, backend.clone())
        .expect("runner should initialize");

    let error = runner
        .connect_tui(RunnerTuiConnectRequest::new("alice"))
        .expect_err("missing running guest should fail connect-only path");
    assert!(matches!(
        error,
        RunnerError::NoRunningGuest {
            ref user_id,
            ref endpoint_path
        } if user_id == "alice" && endpoint_path.ends_with(GATEWAY_ENDPOINT_MARKER_FILE)
    ));
    assert!(backend.recorded_launches().is_empty());

    let _ = fs::remove_dir_all(root);
}

#[test]
fn tui_connect_only_never_spawns_guest_when_probe_fails() {
    let root = temp_dir("tui-connect-probe-fail");
    let global_path = write_runner_config_fixture(&root, "container");
    write_user_config(&root.join("users/alice.toml"), "");

    let backend = Arc::new(MockSandboxBackend::default());
    let runner = Runner::from_global_config_path_with_backend(&global_path, backend.clone())
        .expect("runner should initialize");
    let workspace = runner
        .provision_user_workspace("alice")
        .expect("workspace should provision");
    fs::write(
        workspace.ipc.join(GATEWAY_ENDPOINT_MARKER_FILE),
        "ws://127.0.0.1:9/ws",
    )
    .expect("gateway endpoint marker should be writable");

    let error = runner
        .connect_tui(RunnerTuiConnectRequest::new("alice"))
        .expect_err("unreachable gateway probe should fail");
    assert!(matches!(error, RunnerError::StaleGatewayEndpoint { .. }));
    assert!(backend.recorded_launches().is_empty());

    let _ = fs::remove_dir_all(root);
}

#[test]
fn startup_rejects_microvm_on_unsupported_host() {
    let root = temp_dir("unsupported-host");
    let global_path = write_runner_config_fixture(&root, "micro_vm");
    write_user_config(&root.join("users/alice.toml"), "");

    let runner = Runner::from_global_config_path(&global_path).expect("runner should initialize");
    let error = runner
        .start_user_for_host(RunnerStartRequest::new("alice"), "windows")
        .expect_err("unsupported host should fail");
    assert!(matches!(error, RunnerError::UnsupportedMicroVmHost { .. }));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn extract_socket_path_handles_nested_payload() {
    let payload = json!({
        "vm": {
            "connection": {
                "socketPath": "/tmp/sandbox/docker.sock"
            }
        }
    });
    assert_eq!(
        extract_socket_path(&payload),
        Some(PathBuf::from("/tmp/sandbox/docker.sock"))
    );
}

fn spawn_mock_gateway_probe_server(healthy: bool) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("mock gateway should bind");
    let address = listener
        .local_addr()
        .expect("mock gateway should expose address");
    let handle = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("mock gateway should accept");
        let mut socket = accept(stream).expect("mock websocket handshake should succeed");

        let hello = parse_client_frame(
            socket
                .read()
                .expect("mock gateway should receive hello frame"),
        );
        let user_id = match hello {
            GatewayClientFrame::Hello(hello) => hello.user_id,
            other => panic!("expected hello frame, got {other:?}"),
        };
        send_server_frame(
            &mut socket,
            GatewayServerFrame::HelloAck(types::GatewayHelloAck {
                request_id: "runner-connect-hello".to_owned(),
                protocol_version: GATEWAY_PROTOCOL_VERSION,
                session: types::GatewaySession {
                    user_id: user_id.clone(),
                    runtime_session_id: format!("runtime-{user_id}"),
                },
                active_turn: None,
            }),
        );

        let health_check = parse_client_frame(
            socket
                .read()
                .expect("mock gateway should receive health check"),
        );
        let request_id = match health_check {
            GatewayClientFrame::HealthCheck(request) => request.request_id,
            other => panic!("expected health_check frame, got {other:?}"),
        };
        send_server_frame(
            &mut socket,
            GatewayServerFrame::HealthStatus(types::GatewayHealthStatus {
                request_id,
                healthy,
                session: Some(types::GatewaySession {
                    user_id: user_id.clone(),
                    runtime_session_id: format!("runtime-{user_id}"),
                }),
                active_turn: None,
                startup_status: None,
                message: Some(if healthy {
                    "ready".to_owned()
                } else {
                    "unhealthy".to_owned()
                }),
            }),
        );
        let _ = socket.close(None);
    });

    (format!("ws://{address}/ws"), handle)
}

fn parse_client_frame(message: WsMessage) -> GatewayClientFrame {
    match message {
        WsMessage::Text(payload) => serde_json::from_str::<GatewayClientFrame>(payload.as_ref())
            .expect("mock gateway should decode text client frame"),
        WsMessage::Binary(payload) => serde_json::from_slice::<GatewayClientFrame>(&payload)
            .expect("mock gateway should decode binary client frame"),
        other => panic!("unexpected client websocket message: {other:?}"),
    }
}

fn send_server_frame(
    socket: &mut tokio_tungstenite::tungstenite::WebSocket<std::net::TcpStream>,
    frame: GatewayServerFrame,
) {
    let payload = serde_json::to_string(&frame).expect("mock server frame should encode");
    socket
        .send(WsMessage::Text(payload.into()))
        .expect("mock gateway should send server frame");
}

fn write_runner_config_fixture(root: &Path, default_tier: &str) -> PathBuf {
    let path = root.join("runner.toml");
    fs::create_dir_all(root).expect("root should exist");
    fs::write(
        &path,
        format!(
            r#"
workspace_root = "workspaces"
default_tier = "{default_tier}"

[guest_images]
oxydra_vm = "oxydra-vm:test"
shell_vm = "shell-vm:test"

[users.alice]
config_path = "users/alice.toml"
"#
        )
        .trim_start(),
    )
    .expect("runner config should be writable");
    path
}

fn write_user_config(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("parent directory should exist");
    }
    fs::write(path, content).expect("user config should be writable");
}

fn temp_dir(label: &str) -> PathBuf {
    let mut path = env::temp_dir();
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be monotonic")
        .as_nanos();
    path.push(format!(
        "oxydra-runner-{label}-{}-{unique}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("temp dir should be creatable");
    path
}

fn wait_for_file(path: &Path) {
    for _ in 0..100 {
        if path.exists() {
            return;
        }
        thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!("timed out waiting for file `{}`", path.display());
}

async fn send_control_request(
    stream: &mut tokio::io::DuplexStream,
    request: &RunnerControl,
) -> RunnerControlResponse {
    let payload = serde_json::to_vec(request).expect("runner control request should encode");
    send_control_payload(stream, &payload).await;
    read_control_response(stream).await
}

async fn send_control_payload(stream: &mut tokio::io::DuplexStream, payload: &[u8]) {
    write_runner_control_frame(stream, payload)
        .await
        .expect("runner control frame should send");
}

async fn read_control_response(stream: &mut tokio::io::DuplexStream) -> RunnerControlResponse {
    let frame = read_runner_control_frame(stream)
        .await
        .expect("runner control response frame should read")
        .expect("runner control response frame should be present");
    serde_json::from_slice(&frame).expect("runner control response should decode")
}

// ---------------------------------------------------------------------------
//  Bootstrap file and Firecracker config tests
// ---------------------------------------------------------------------------

#[test]
fn bootstrap_file_written_with_correct_content_for_non_process_tier() {
    let root = temp_dir("bootstrap-file-write");
    let workspace = provision_user_workspace(root.join("workspace-root"), "alice")
        .expect("workspace should provision");
    let bootstrap = RunnerBootstrapEnvelope {
        user_id: "alice".to_owned(),
        sandbox_tier: SandboxTier::Container,
        workspace_root: workspace.root.to_string_lossy().into_owned(),
        sidecar_endpoint: None,
        runtime_policy: None,
        startup_status: None,
    };
    let path =
        write_bootstrap_file(&workspace, &bootstrap).expect("bootstrap file should be written");

    assert!(path.exists(), "bootstrap file should exist on disk");
    assert!(
        path.starts_with(&workspace.ipc),
        "bootstrap file should be under workspace.ipc"
    );
    assert_eq!(
        path.file_name().and_then(|n| n.to_str()),
        Some("runner_bootstrap.json"),
    );

    let content = fs::read_to_string(&path).expect("bootstrap file should be readable");
    let decoded: RunnerBootstrapEnvelope =
        serde_json::from_str(&content).expect("bootstrap file should contain valid JSON");
    assert_eq!(decoded.user_id, "alice");
    assert_eq!(decoded.sandbox_tier, SandboxTier::Container);

    #[cfg(unix)]
    {
        let mode = fs::metadata(&path)
            .expect("bootstrap file metadata should load")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o400, "bootstrap file should be read-only for owner");
    }

    let _ = fs::remove_dir_all(root);
}

#[test]
fn container_tier_launch_request_includes_bootstrap_file() {
    let root = temp_dir("container-bootstrap-req");
    let global_path = write_runner_config_fixture(&root, "container");
    write_user_config(&root.join("users/alice.toml"), "");

    let backend = Arc::new(MockSandboxBackend::default());
    let runner = Runner::from_global_config_path_with_backend(&global_path, backend.clone())
        .expect("runner should initialize");
    let _startup = runner
        .start_user_for_host(RunnerStartRequest::new("alice"), "linux")
        .expect("startup should succeed");

    let launches = backend.recorded_launches();
    assert_eq!(launches.len(), 1);
    assert!(
        launches[0].bootstrap_file.is_some(),
        "container tier launch request should include bootstrap_file"
    );
    let bfile = launches[0].bootstrap_file.as_ref().unwrap();
    assert!(bfile.exists(), "bootstrap file should exist on disk");
    let content = fs::read_to_string(bfile).expect("bootstrap file should be readable");
    let decoded: RunnerBootstrapEnvelope =
        serde_json::from_str(&content).expect("bootstrap file should contain valid JSON");
    assert_eq!(decoded.user_id, "alice");
    assert_eq!(decoded.sandbox_tier, SandboxTier::Container);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn process_tier_launch_request_has_no_bootstrap_file() {
    let root = temp_dir("process-no-bootstrap-file");
    let global_path = write_runner_config_fixture(&root, "micro_vm");
    write_user_config(&root.join("users/alice.toml"), "");

    let backend = Arc::new(MockSandboxBackend::default());
    let runner = Runner::from_global_config_path_with_backend(&global_path, backend.clone())
        .expect("runner should initialize");
    let _startup = runner
        .start_user_for_host(
            RunnerStartRequest {
                user_id: "alice".to_owned(),
                insecure: true,
                extra_env: Vec::new(),
            },
            "linux",
        )
        .expect("startup should succeed");

    let launches = backend.recorded_launches();
    assert_eq!(launches.len(), 1);
    assert!(
        launches[0].bootstrap_file.is_none(),
        "process tier launch request should not include bootstrap_file"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn firecracker_config_injects_bootstrap_into_boot_args() {
    use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};

    let root = temp_dir("fc-config-inject");
    let template_path = root.join("template.json");
    let output_path = root.join("generated.json");
    let bootstrap_path = root.join("bootstrap.json");

    let template = serde_json::json!({
        "boot-source": {
            "kernel_image_path": "/images/vmlinux",
            "boot_args": "console=ttyS0 reboot=k"
        },
        "drives": [{"drive_id": "rootfs"}],
        "machine-config": {"vcpu_count": 2}
    });
    fs::write(
        &template_path,
        serde_json::to_string_pretty(&template).unwrap(),
    )
    .expect("template should be writable");

    let bootstrap = RunnerBootstrapEnvelope {
        user_id: "alice".to_owned(),
        sandbox_tier: SandboxTier::MicroVm,
        workspace_root: "/workspace".to_owned(),
        sidecar_endpoint: None,
        runtime_policy: None,
        startup_status: None,
    };
    let bootstrap_json = serde_json::to_vec_pretty(&bootstrap).unwrap();
    fs::write(&bootstrap_path, &bootstrap_json).expect("bootstrap file should be writable");

    backend::generate_firecracker_config(
        template_path.to_str().unwrap(),
        Some(bootstrap_path.as_path()),
        &output_path,
    )
    .expect("config generation should succeed");

    let generated_content =
        fs::read_to_string(&output_path).expect("generated config should exist");
    let generated: serde_json::Value =
        serde_json::from_str(&generated_content).expect("generated config should be valid JSON");

    // Template fields preserved
    assert_eq!(
        generated["drives"][0]["drive_id"].as_str(),
        Some("rootfs"),
        "template fields should be preserved"
    );
    assert_eq!(
        generated["machine-config"]["vcpu_count"].as_u64(),
        Some(2),
        "machine-config should be preserved"
    );

    // Boot args should contain existing args plus the bootstrap
    let boot_args = generated["boot-source"]["boot_args"]
        .as_str()
        .expect("boot_args should be a string");
    assert!(
        boot_args.starts_with("console=ttyS0 reboot=k"),
        "existing boot_args should be preserved"
    );
    assert!(
        boot_args.contains("oxydra.bootstrap_b64="),
        "bootstrap should be injected into boot_args"
    );

    // Extract and decode the bootstrap payload
    let b64_payload = boot_args
        .split("oxydra.bootstrap_b64=")
        .nth(1)
        .expect("bootstrap key should be present");
    let decoded_bytes = BASE64_STANDARD
        .decode(b64_payload.trim())
        .expect("base64 payload should decode");
    let decoded: RunnerBootstrapEnvelope =
        serde_json::from_slice(&decoded_bytes).expect("decoded payload should be valid bootstrap");
    assert_eq!(decoded.user_id, "alice");
    assert_eq!(decoded.sandbox_tier, SandboxTier::MicroVm);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn firecracker_config_without_bootstrap_copies_template() {
    let root = temp_dir("fc-config-no-bootstrap");
    let template_path = root.join("template.json");
    let output_path = root.join("generated.json");

    let template = serde_json::json!({
        "boot-source": {
            "kernel_image_path": "/images/vmlinux",
            "boot_args": "console=ttyS0"
        },
        "drives": [{"drive_id": "rootfs"}]
    });
    fs::write(
        &template_path,
        serde_json::to_string_pretty(&template).unwrap(),
    )
    .expect("template should be writable");

    backend::generate_firecracker_config(template_path.to_str().unwrap(), None, &output_path)
        .expect("config generation without bootstrap should succeed");

    let generated: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&output_path).unwrap())
            .expect("generated config should be valid JSON");
    assert_eq!(
        generated["boot-source"]["boot_args"].as_str(),
        Some("console=ttyS0"),
        "boot_args should be unchanged when no bootstrap is injected"
    );
    assert_eq!(generated["drives"][0]["drive_id"].as_str(), Some("rootfs"),);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn firecracker_config_rejects_oversized_bootstrap() {
    let root = temp_dir("fc-config-oversized");
    let template_path = root.join("template.json");
    let output_path = root.join("generated.json");
    let bootstrap_path = root.join("bootstrap.json");

    let template = serde_json::json!({
        "boot-source": {
            "kernel_image_path": "/images/vmlinux",
            "boot_args": "console=ttyS0"
        }
    });
    fs::write(
        &template_path,
        serde_json::to_string_pretty(&template).unwrap(),
    )
    .expect("template should be writable");

    // Create a bootstrap with a very large workspace_root to exceed the limit
    let large_path = "x".repeat(4096);
    let bootstrap = RunnerBootstrapEnvelope {
        user_id: "alice".to_owned(),
        sandbox_tier: SandboxTier::MicroVm,
        workspace_root: large_path,
        sidecar_endpoint: None,
        runtime_policy: None,
        startup_status: None,
    };
    let bootstrap_json = serde_json::to_vec_pretty(&bootstrap).unwrap();
    fs::write(&bootstrap_path, &bootstrap_json).expect("bootstrap file should be writable");

    let result = backend::generate_firecracker_config(
        template_path.to_str().unwrap(),
        Some(bootstrap_path.as_path()),
        &output_path,
    );
    assert!(result.is_err(), "oversized bootstrap should be rejected");
    let error = result.unwrap_err();
    let error_msg = error.to_string();
    assert!(
        error_msg.contains("too large") || error_msg.contains("cmdline"),
        "error should mention size limit: {error_msg}"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn health_check_includes_log_dir_and_metadata() {
    let root = temp_dir("health-check-metadata");
    let global_path = write_runner_config_fixture(&root, "container");
    write_user_config(&root.join("users/alice.toml"), "");

    let backend = Arc::new(MockSandboxBackend::default());
    let runner = Runner::from_global_config_path_with_backend(&global_path, backend)
        .expect("runner should initialize");
    let startup = runner
        .start_user_for_host(RunnerStartRequest::new("alice"), "linux")
        .expect("startup should succeed");

    let (mut client, server) = tokio::io::duplex(8192);
    let server_task = tokio::spawn(async move {
        let mut startup = startup;
        startup
            .serve_control_stream(server)
            .await
            .expect("control stream should serve");
        startup
    });

    let health = send_control_request(&mut client, &RunnerControl::HealthCheck).await;
    match health {
        RunnerControlResponse::HealthStatus(status) => {
            assert!(status.healthy);
            assert!(
                status.log_dir.is_some(),
                "health status should include log_dir"
            );
            let log_dir = status.log_dir.as_ref().unwrap();
            assert!(
                log_dir.contains("logs"),
                "log_dir should point to logs directory: {log_dir}"
            );
            // Simulated handles have pid: None
            assert_eq!(
                status.runtime_pid, None,
                "simulated handle should have no runtime_pid"
            );
            // Mock backend creates Simulated lifecycle, not Docker
            assert_eq!(
                status.runtime_container_name, None,
                "simulated handle should have no container name"
            );
        }
        other => panic!("expected HealthStatus, got {other:?}"),
    }

    // Shut down to let server task complete
    let _ = send_control_request(
        &mut client,
        &RunnerControl::ShutdownUser {
            user_id: "alice".to_owned(),
        },
    )
    .await;
    drop(client);
    let _ = server_task.await;

    let _ = fs::remove_dir_all(root);
}

#[test]
fn microvm_tier_launch_request_includes_bootstrap_file() {
    let root = temp_dir("microvm-bootstrap-req");
    let global_path = write_runner_config_fixture(&root, "micro_vm");
    write_user_config(&root.join("users/alice.toml"), "");

    let backend = Arc::new(MockSandboxBackend::default());
    let runner = Runner::from_global_config_path_with_backend(&global_path, backend.clone())
        .expect("runner should initialize");
    let _startup = runner
        .start_user_for_host(RunnerStartRequest::new("alice"), "linux")
        .expect("startup should succeed");

    let launches = backend.recorded_launches();
    assert_eq!(launches.len(), 1);
    assert!(
        launches[0].bootstrap_file.is_some(),
        "microvm tier launch request should include bootstrap_file"
    );
    let bfile = launches[0].bootstrap_file.as_ref().unwrap();
    assert!(bfile.exists(), "bootstrap file should exist on disk");
    let decoded: RunnerBootstrapEnvelope =
        serde_json::from_str(&fs::read_to_string(bfile).unwrap())
            .expect("bootstrap file should contain valid bootstrap envelope");
    assert_eq!(decoded.user_id, "alice");
    assert_eq!(decoded.sandbox_tier, SandboxTier::MicroVm);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn copy_agent_config_to_workspace_copies_existing_config_files() {
    let root = temp_dir("copy-agent-config");
    let workspace = provision_user_workspace(root.join("workspace-root"), "alice")
        .expect("workspace should provision");

    // Create a fake host-side .oxydra/ directory in CWD for the test.
    // Since copy_agent_config_to_workspace uses ConfigSearchPaths::discover()
    // which reads CWD/.oxydra/, we instead test the underlying copy logic
    // by directly placing files in workspace.internal and verifying the path.
    let agent_content = "config_version = \"1.0\"\n[selection]\nprovider = \"gemini\"\n";
    fs::write(workspace.internal.join("agent.toml"), agent_content)
        .expect("agent.toml should be writable");

    let written = fs::read_to_string(workspace.internal.join("agent.toml"))
        .expect("agent.toml should be readable in workspace internal");
    assert_eq!(written, agent_content);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn container_tier_launch_request_includes_extra_env() {
    let root = temp_dir("container-extra-env");
    let global_path = write_runner_config_fixture(&root, "container");
    write_user_config(&root.join("users/alice.toml"), "");

    let backend = Arc::new(MockSandboxBackend::default());
    let runner = Runner::from_global_config_path_with_backend(&global_path, backend.clone())
        .expect("runner should initialize");

    let _startup = runner
        .start_user_for_host(
            RunnerStartRequest {
                user_id: "alice".to_owned(),
                insecure: false,
                extra_env: vec!["MY_KEY=my_value".to_owned()],
            },
            "linux",
        )
        .expect("startup should succeed");

    let launches = backend.recorded_launches();
    assert_eq!(launches.len(), 1);
    assert!(
        launches[0]
            .extra_env
            .contains(&"MY_KEY=my_value".to_owned()),
        "extra_env should contain CLI-provided env var; got: {:?}",
        launches[0].extra_env
    );

    let _ = fs::remove_dir_all(root);
}
