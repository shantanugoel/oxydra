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
    DEFAULT_RUNNER_CONFIG_VERSION, LogFormat, LogRole, LogStream, RunnerBootstrapEnvelope,
    RunnerControl, RunnerControlErrorCode, RunnerControlLogsRequest, RunnerControlResponse,
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

    fn remove(key: &'static str) -> Self {
        let previous = env::var(key).ok();
        // SAFETY: tests guard environment writes with a process-wide mutex.
        unsafe { env::remove_var(key) };
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
fn loading_legacy_runner_global_config_auto_migrates_and_creates_backup() {
    let root = temp_dir("global-config-migration");
    let global_path = write_legacy_runner_config_fixture(&root, "container");

    let config = load_runner_global_config(&global_path).expect("global config should load");
    assert_eq!(config.config_version, DEFAULT_RUNNER_CONFIG_VERSION);

    let migrated = fs::read_to_string(&global_path).expect("migrated config should be readable");
    assert!(
        migrated.contains(&format!(
            "config_version = \"{DEFAULT_RUNNER_CONFIG_VERSION}\""
        )),
        "migrated config should include updated version"
    );

    let backup_count = fs::read_dir(&root)
        .expect("root should be readable")
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("runner.toml.bak.")
        })
        .count();
    assert_eq!(backup_count, 1, "expected one backup file");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn loading_legacy_runner_user_config_auto_migrates_and_creates_backup() {
    let root = temp_dir("user-config-migration");
    let user_path = root.join("users/alice.toml");
    write_user_config(&user_path, "");

    let config = load_runner_user_config(&user_path).expect("user config should load");
    assert_eq!(config.config_version, DEFAULT_RUNNER_CONFIG_VERSION);

    let migrated = fs::read_to_string(&user_path).expect("migrated config should be readable");
    assert!(
        migrated.contains(&format!(
            "config_version = \"{DEFAULT_RUNNER_CONFIG_VERSION}\""
        )),
        "migrated config should include updated version"
    );

    let backup_count = fs::read_dir(root.join("users"))
        .expect("users directory should be readable")
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("alice.toml.bak.")
        })
        .count();
    assert_eq!(backup_count, 1, "expected one backup file");

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
    let decoded = wait_for_bootstrap_frame(&captured_frame_path);
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
    assert_eq!(connection.session_id, "runtime-alice");
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
                    session_id: format!("runtime-{user_id}"),
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
                    session_id: format!("runtime-{user_id}"),
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
config_version = "{DEFAULT_RUNNER_CONFIG_VERSION}"
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

/// Writes a runner config fixture *without* `config_version` to simulate legacy files
/// that need migration.
fn write_legacy_runner_config_fixture(root: &Path, default_tier: &str) -> PathBuf {
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
    .expect("legacy runner config should be writable");
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

/// Creates a short temp directory under `/tmp` for Unix socket tests.
/// macOS limits Unix socket paths to 104 bytes (SUN_LEN), so the standard
/// `env::temp_dir()` path (which can be ~50 chars on its own) is too long
/// when combined with workspace subdirectories.
fn short_temp_dir(label: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be monotonic")
        .as_nanos();
    let path = PathBuf::from(format!("/tmp/ox-{label}-{unique}"));
    fs::create_dir_all(&path).expect("short temp dir should be creatable");
    path
}

fn wait_for_bootstrap_frame(path: &Path) -> RunnerBootstrapEnvelope {
    for _ in 0..100 {
        if let Ok(encoded) = fs::read(path)
            && let Ok(decoded) = RunnerBootstrapEnvelope::from_length_prefixed_json(&encoded)
        {
            return decoded;
        }
        thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!(
        "timed out waiting for decodable bootstrap frame in `{}`",
        path.display()
    );
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
        channels: None,
        browser_config: None,
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
        channels: None,
        browser_config: None,
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
        channels: None,
        browser_config: None,
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
fn copy_agent_config_to_workspace_materializes_merged_config() {
    let _env_lock = env_lock().lock().unwrap_or_else(|error| error.into_inner());
    let _selection_provider = EnvVarGuard::remove("OXYDRA__SELECTION__PROVIDER");
    let _selection_model = EnvVarGuard::remove("OXYDRA__SELECTION__MODEL");

    let root = temp_dir("copy-agent-config");
    let host_paths = bootstrap::ConfigSearchPaths {
        system_dir: root.join("system"),
        user_dir: Some(root.join("user")),
        workspace_dir: root.join("workspace"),
    };
    fs::create_dir_all(&host_paths.system_dir).expect("system config dir should be creatable");
    fs::create_dir_all(
        host_paths
            .user_dir
            .as_ref()
            .expect("user config dir should exist in test paths"),
    )
    .expect("user config dir should be creatable");
    fs::create_dir_all(&host_paths.workspace_dir)
        .expect("workspace config dir should be creatable");

    fs::write(
        host_paths
            .user_dir
            .as_ref()
            .expect("user config dir should exist")
            .join(bootstrap::AGENT_CONFIG_FILE_NAME),
        r#"
config_version = "1.0.0"
[selection]
provider = "gemini"
model = "gemini-3.1-flash-image-preview"
[catalog]
skip_catalog_validation = true
"#
        .trim_start(),
    )
    .expect("user agent config should be writable");
    fs::write(
        host_paths
            .workspace_dir
            .join(bootstrap::PROVIDERS_CONFIG_FILE_NAME),
        r#"
[providers.registry.gemini]
provider_type = "gemini"
api_key = "test-gemini-key"
"#
        .trim_start(),
    )
    .expect("workspace providers config should be writable");

    let workspace = provision_user_workspace(root.join("workspace-root"), "alice")
        .expect("workspace should provision");
    copy_agent_config_to_workspace_with_paths(&workspace, &host_paths)
        .expect("merged config should be materialized in workspace internal");

    let persisted = fs::read_to_string(workspace.internal.join(bootstrap::AGENT_CONFIG_FILE_NAME))
        .expect("materialized agent config should be readable");
    let config: types::AgentConfig =
        toml::from_str(&persisted).expect("materialized agent config should parse");
    assert_eq!(config.selection.provider, types::ProviderId::from("gemini"));
    assert_eq!(
        config.selection.model,
        types::ModelId::from("gemini-3.1-flash-image-preview")
    );
    assert!(config.catalog.skip_catalog_validation);
    assert!(
        !workspace
            .internal
            .join(bootstrap::PROVIDERS_CONFIG_FILE_NAME)
            .exists(),
        "providers.toml should be removed to avoid stale overrides"
    );

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
    // Non-SHELL_-prefixed vars should NOT be in shell_env.
    assert!(
        !launches[0]
            .shell_env
            .contains(&"MY_KEY=my_value".to_owned()),
        "shell_env should not contain non-SHELL_ env var; got: {:?}",
        launches[0].shell_env
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn shell_prefixed_env_vars_forwarded_to_shell_env_with_prefix_stripped() {
    let root = temp_dir("shell-prefix-env");
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
                extra_env: vec![
                    "SHELL_NPM_TOKEN=tok123".to_owned(),
                    "REGULAR_KEY=val456".to_owned(),
                ],
            },
            "linux",
        )
        .expect("startup should succeed");

    let launches = backend.recorded_launches();
    assert_eq!(launches.len(), 1);

    // SHELL_NPM_TOKEN should appear in shell_env as NPM_TOKEN (prefix stripped).
    assert!(
        launches[0]
            .shell_env
            .contains(&"NPM_TOKEN=tok123".to_owned()),
        "shell_env should contain stripped SHELL_ env var; got: {:?}",
        launches[0].shell_env
    );

    // SHELL_NPM_TOKEN should NOT appear in extra_env (oxydra-vm).
    assert!(
        !launches[0]
            .extra_env
            .iter()
            .any(|e| e.contains("NPM_TOKEN")),
        "extra_env should not contain SHELL_-prefixed env var; got: {:?}",
        launches[0].extra_env
    );

    // REGULAR_KEY should be in extra_env (oxydra-vm).
    assert!(
        launches[0]
            .extra_env
            .contains(&"REGULAR_KEY=val456".to_owned()),
        "extra_env should contain regular env var; got: {:?}",
        launches[0].extra_env
    );

    // REGULAR_KEY should NOT be in shell_env.
    assert!(
        !launches[0]
            .shell_env
            .contains(&"REGULAR_KEY=val456".to_owned()),
        "shell_env should not contain regular env var; got: {:?}",
        launches[0].shell_env
    );

    let _ = fs::remove_dir_all(root);
}

// ---------------------------------------------------------------------------
//  Daemon control socket and exit-after-shutdown tests
// ---------------------------------------------------------------------------

#[test]
fn workspace_control_socket_path_uses_runner_constant() {
    let root = temp_dir("workspace-socket-path");
    let workspace = provision_user_workspace(root.join("workspace-root"), "alice")
        .expect("workspace should provision");
    let socket_path = workspace.control_socket_path();
    assert!(
        socket_path.ends_with(RUNNER_CONTROL_SOCKET_NAME),
        "control socket path should use the constant: {}",
        socket_path.display()
    );
    assert!(
        socket_path.starts_with(&workspace.ipc),
        "control socket should be under workspace.ipc: {}",
        socket_path.display()
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn serve_control_unix_listener_exits_after_shutdown() {
    let root = temp_dir("ctl-exit");
    let global_path = write_runner_config_fixture(&root, "container");
    write_user_config(&root.join("users/alice.toml"), "");

    let backend = Arc::new(MockSandboxBackend::default());
    let runner = Runner::from_global_config_path_with_backend(&global_path, backend)
        .expect("runner should initialize");
    let startup = runner
        .start_user_for_host(RunnerStartRequest::new("alice"), "linux")
        .expect("startup should succeed");

    // Use a short path under /tmp to stay within macOS SUN_LEN (104 bytes).
    let socket_dir = short_temp_dir("ce");
    let socket_path = socket_dir.join("s.sock");
    let _ = fs::remove_file(&socket_path);
    let listener =
        tokio::net::UnixListener::bind(&socket_path).expect("test control socket should bind");

    let server_task = tokio::spawn(async move {
        let mut startup = startup;
        let result = startup.serve_control_unix_listener(listener).await;
        (startup, result)
    });

    // Connect as a client, send shutdown, verify the listener task completes.
    let mut client = tokio::net::UnixStream::connect(&socket_path)
        .await
        .expect("client should connect to control socket");

    let shutdown_request = RunnerControl::ShutdownUser {
        user_id: "alice".to_owned(),
    };
    let payload = serde_json::to_vec(&shutdown_request).expect("shutdown request should serialize");
    write_runner_control_frame(&mut client, &payload)
        .await
        .expect("should send shutdown frame");

    let response_frame = read_runner_control_frame(&mut client)
        .await
        .expect("should read response frame")
        .expect("response frame should be present");
    let response: RunnerControlResponse =
        serde_json::from_slice(&response_frame).expect("response should decode");
    assert!(
        matches!(
            response,
            RunnerControlResponse::ShutdownStatus(ref status) if status.shutdown
        ),
        "shutdown response should confirm shutdown: {response:?}"
    );

    // Drop the client connection so the listener can process the disconnect.
    drop(client);

    // The serve_control_unix_listener should return because shutdown_complete is true.
    let (startup, result) = tokio::time::timeout(std::time::Duration::from_secs(5), server_task)
        .await
        .expect("server task should complete within timeout")
        .expect("server task should not panic");

    result.expect("serve_control_unix_listener should return Ok after shutdown");
    assert!(
        startup
            .startup_status
            .has_reason_code(StartupDegradedReasonCode::RuntimeShutdown),
        "startup should be marked as shut down"
    );

    let _ = fs::remove_file(&socket_path);
    let _ = fs::remove_dir_all(socket_dir);
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn send_control_to_daemon_receives_health_response() {
    let root = temp_dir("ctl-hlth");
    let global_path = write_runner_config_fixture(&root, "container");
    write_user_config(&root.join("users/alice.toml"), "");

    let backend = Arc::new(MockSandboxBackend::default());
    let runner = Runner::from_global_config_path_with_backend(&global_path, backend)
        .expect("runner should initialize");
    let startup = runner
        .start_user_for_host(RunnerStartRequest::new("alice"), "linux")
        .expect("startup should succeed");

    // Use a short path under /tmp to stay within macOS SUN_LEN (104 bytes).
    let socket_dir = short_temp_dir("ch");
    let socket_path = socket_dir.join("s.sock");
    let _ = fs::remove_file(&socket_path);
    let listener =
        tokio::net::UnixListener::bind(&socket_path).expect("test control socket should bind");

    let socket_path_clone = socket_path.clone();
    let server_task = tokio::spawn(async move {
        let mut startup = startup;
        let _ = startup.serve_control_unix_listener(listener).await;
        startup
    });

    // Use send_control_to_daemon from a blocking context (like the CLI does).
    let response = tokio::task::spawn_blocking(move || {
        send_control_to_daemon(&socket_path_clone, &RunnerControl::HealthCheck)
    })
    .await
    .expect("blocking task should not panic")
    .expect("send_control_to_daemon should succeed");

    assert!(
        matches!(
            response,
            RunnerControlResponse::HealthStatus(ref status) if status.healthy
        ),
        "health check response should be healthy: {response:?}"
    );

    // Shut down via a second connection to let the server task exit.
    let socket_path_clone2 = socket_path.clone();
    let _ = tokio::task::spawn_blocking(move || {
        send_control_to_daemon(
            &socket_path_clone2,
            &RunnerControl::ShutdownUser {
                user_id: "alice".to_owned(),
            },
        )
    })
    .await;

    let _ = server_task.await;
    let _ = fs::remove_file(&socket_path);
    let _ = fs::remove_dir_all(socket_dir);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn send_control_to_daemon_reports_connection_refused_for_missing_socket() {
    let root = temp_dir("ctl-miss");
    let nonexistent_socket = root.join("x.sock");

    let result = send_control_to_daemon(&nonexistent_socket, &RunnerControl::HealthCheck);
    assert!(result.is_err(), "missing socket should fail");
    let err = result.unwrap_err();
    assert!(
        err.is_connection_refused() || matches!(err, RunnerControlTransportError::Transport(_)),
        "error should be a transport error: {err}"
    );

    let _ = fs::remove_dir_all(root);
}

// ---------------------------------------------------------------------------
//  Log retrieval tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn control_logs_returns_entries_from_process_log_files() {
    let root = temp_dir("logs-process");
    let global_path = write_runner_config_fixture(&root, "process");
    write_user_config(&root.join("users/alice.toml"), "");

    let backend = Arc::new(MockSandboxBackend::default());
    let runner = Runner::from_global_config_path_with_backend(&global_path, backend)
        .expect("runner should initialize");
    let startup = runner
        .start_user_for_host(RunnerStartRequest::new("alice"), "linux")
        .expect("startup should succeed");

    // Write some fake log content.
    let log_dir = &startup.workspace.logs;
    fs::write(
        log_dir.join("oxydra-vm.stdout.log"),
        "2026-03-01T10:00:00Z line one\n2026-03-01T10:00:01Z line two\n",
    )
    .expect("stdout log should be writable");
    fs::write(
        log_dir.join("oxydra-vm.stderr.log"),
        "2026-03-01T10:00:02Z error line\n",
    )
    .expect("stderr log should be writable");

    let (mut client, server) = tokio::io::duplex(16384);
    let server_task = tokio::spawn(async move {
        let mut startup = startup;
        startup
            .serve_control_stream(server)
            .await
            .expect("control stream should serve");
        startup
    });

    let response = send_control_request(
        &mut client,
        &RunnerControl::Logs(RunnerControlLogsRequest {
            role: LogRole::Runtime,
            stream: LogStream::Both,
            tail: Some(100),
            since: None,
            format: LogFormat::Text,
        }),
    )
    .await;

    match response {
        RunnerControlResponse::Logs(logs) => {
            assert!(
                !logs.entries.is_empty(),
                "should have log entries from files"
            );
            assert_eq!(
                logs.entries.len(),
                3,
                "should have 3 entries (2 stdout + 1 stderr)"
            );
            assert!(!logs.truncated);

            // Verify entries contain expected data.
            let messages: Vec<&str> = logs.entries.iter().map(|e| e.message.as_str()).collect();
            assert!(
                messages.contains(&"line one"),
                "should contain stdout line: {messages:?}"
            );
            assert!(
                messages.contains(&"error line"),
                "should contain stderr line: {messages:?}"
            );
        }
        other => panic!("expected Logs response, got {other:?}"),
    }

    // Shutdown.
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

#[tokio::test]
async fn control_logs_filters_by_stream() {
    let root = temp_dir("logs-filter-stream");
    let global_path = write_runner_config_fixture(&root, "process");
    write_user_config(&root.join("users/alice.toml"), "");

    let backend = Arc::new(MockSandboxBackend::default());
    let runner = Runner::from_global_config_path_with_backend(&global_path, backend)
        .expect("runner should initialize");
    let startup = runner
        .start_user_for_host(RunnerStartRequest::new("alice"), "linux")
        .expect("startup should succeed");

    let log_dir = &startup.workspace.logs;
    fs::write(log_dir.join("oxydra-vm.stdout.log"), "stdout line\n")
        .expect("stdout log should be writable");
    fs::write(log_dir.join("oxydra-vm.stderr.log"), "stderr line\n")
        .expect("stderr log should be writable");

    let (mut client, server) = tokio::io::duplex(16384);
    let server_task = tokio::spawn(async move {
        let mut startup = startup;
        startup
            .serve_control_stream(server)
            .await
            .expect("control stream should serve");
        startup
    });

    // Request stderr only.
    let response = send_control_request(
        &mut client,
        &RunnerControl::Logs(RunnerControlLogsRequest {
            role: LogRole::Runtime,
            stream: LogStream::Stderr,
            tail: Some(100),
            since: None,
            format: LogFormat::Text,
        }),
    )
    .await;

    match response {
        RunnerControlResponse::Logs(logs) => {
            assert_eq!(logs.entries.len(), 1, "should have only stderr entries");
            assert_eq!(logs.entries[0].stream, "stderr");
            assert_eq!(logs.entries[0].message, "stderr line");
        }
        other => panic!("expected Logs response, got {other:?}"),
    }

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

#[tokio::test]
async fn control_logs_respects_tail_limit() {
    let root = temp_dir("logs-tail");
    let global_path = write_runner_config_fixture(&root, "process");
    write_user_config(&root.join("users/alice.toml"), "");

    let backend = Arc::new(MockSandboxBackend::default());
    let runner = Runner::from_global_config_path_with_backend(&global_path, backend)
        .expect("runner should initialize");
    let startup = runner
        .start_user_for_host(RunnerStartRequest::new("alice"), "linux")
        .expect("startup should succeed");

    // Write 10 lines.
    let mut content = String::new();
    for i in 0..10 {
        content.push_str(&format!("line {i}\n"));
    }
    fs::write(
        startup.workspace.logs.join("oxydra-vm.stdout.log"),
        &content,
    )
    .expect("log should be writable");

    let (mut client, server) = tokio::io::duplex(16384);
    let server_task = tokio::spawn(async move {
        let mut startup = startup;
        startup
            .serve_control_stream(server)
            .await
            .expect("control stream should serve");
        startup
    });

    // Request only 3 lines.
    let response = send_control_request(
        &mut client,
        &RunnerControl::Logs(RunnerControlLogsRequest {
            role: LogRole::Runtime,
            stream: LogStream::Stdout,
            tail: Some(3),
            since: None,
            format: LogFormat::Text,
        }),
    )
    .await;

    match response {
        RunnerControlResponse::Logs(logs) => {
            assert_eq!(logs.entries.len(), 3, "should return only 3 entries");
            assert!(logs.truncated, "should be marked as truncated");
            // Should return the LAST 3 lines.
            assert_eq!(logs.entries[0].message, "line 7");
            assert_eq!(logs.entries[1].message, "line 8");
            assert_eq!(logs.entries[2].message, "line 9");
        }
        other => panic!("expected Logs response, got {other:?}"),
    }

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

#[tokio::test]
async fn control_logs_warns_when_sidecar_unavailable() {
    let root = temp_dir("logs-no-sidecar");
    let global_path = write_runner_config_fixture(&root, "process");
    write_user_config(&root.join("users/alice.toml"), "");

    let backend = Arc::new(MockSandboxBackend::default());
    let runner = Runner::from_global_config_path_with_backend(&global_path, backend)
        .expect("runner should initialize");
    let startup = runner
        .start_user_for_host(RunnerStartRequest::new("alice"), "linux")
        .expect("startup should succeed");

    let (mut client, server) = tokio::io::duplex(16384);
    let server_task = tokio::spawn(async move {
        let mut startup = startup;
        startup
            .serve_control_stream(server)
            .await
            .expect("control stream should serve");
        startup
    });

    let response = send_control_request(
        &mut client,
        &RunnerControl::Logs(RunnerControlLogsRequest {
            role: LogRole::Sidecar,
            stream: LogStream::Both,
            tail: Some(100),
            since: None,
            format: LogFormat::Text,
        }),
    )
    .await;

    match response {
        RunnerControlResponse::Logs(logs) => {
            assert!(logs.entries.is_empty(), "no sidecar = no entries");
            assert!(
                logs.warnings.iter().any(|w| w.contains("shell-vm")),
                "should warn about missing sidecar: {:?}",
                logs.warnings
            );
        }
        other => panic!("expected Logs response, got {other:?}"),
    }

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

#[tokio::test]
async fn control_logs_returns_empty_after_shutdown() {
    let root = temp_dir("logs-after-shutdown");
    let global_path = write_runner_config_fixture(&root, "process");
    write_user_config(&root.join("users/alice.toml"), "");

    let backend = Arc::new(MockSandboxBackend::default());
    let runner = Runner::from_global_config_path_with_backend(&global_path, backend)
        .expect("runner should initialize");
    let startup = runner
        .start_user_for_host(RunnerStartRequest::new("alice"), "linux")
        .expect("startup should succeed");

    fs::write(
        startup.workspace.logs.join("oxydra-vm.stdout.log"),
        "some data\n",
    )
    .expect("log should be writable");

    let (mut client, server) = tokio::io::duplex(16384);
    let server_task = tokio::spawn(async move {
        let mut startup = startup;
        startup
            .serve_control_stream(server)
            .await
            .expect("control stream should serve");
        startup
    });

    // Shut down first.
    let _ = send_control_request(
        &mut client,
        &RunnerControl::ShutdownUser {
            user_id: "alice".to_owned(),
        },
    )
    .await;

    // Now try to get logs — server has shut down, connection closed.
    // The serve_control_stream exits after shutdown. So we can't send more
    // requests. This test verifies the pre-shutdown Logs handling works.
    drop(client);
    let _ = server_task.await;
    let _ = fs::remove_dir_all(root);
}

// ---------------------------------------------------------------------------
//  Helper function unit tests
// ---------------------------------------------------------------------------

#[test]
fn extract_timestamp_parses_docker_timestamp() {
    let line = "2026-03-01T10:55:12.123456789Z some log message";
    let ts = extract_timestamp(line);
    assert_eq!(ts, Some("2026-03-01T10:55:12.123456789Z".to_owned()));
}

#[test]
fn extract_timestamp_returns_none_for_plain_text() {
    let ts = extract_timestamp("just a plain log line");
    assert_eq!(ts, None);
}

#[test]
fn strip_timestamp_removes_docker_timestamp() {
    let line = "2026-03-01T10:55:12Z gateway bind failed";
    let msg = strip_timestamp(line);
    assert_eq!(msg, "gateway bind failed");
}

#[test]
fn strip_timestamp_preserves_plain_text() {
    let line = "no timestamp here";
    let msg = strip_timestamp(line);
    assert_eq!(msg, "no timestamp here");
}

#[test]
fn parse_since_threshold_handles_duration_shorthand() {
    let result = parse_since_threshold("15m");
    assert!(result.is_some(), "15m should parse as duration");
    let ts = result.unwrap();
    assert!(ts.contains('T'), "result should be RFC 3339 format: {ts}");
}

#[test]
fn parse_since_threshold_handles_rfc3339() {
    let result = parse_since_threshold("2026-03-01T10:00:00Z");
    assert_eq!(result, Some("2026-03-01T10:00:00Z".to_owned()));
}

#[test]
fn parse_since_threshold_returns_none_for_empty() {
    assert_eq!(parse_since_threshold(""), None);
}

#[test]
fn parse_since_threshold_returns_none_for_invalid() {
    assert_eq!(parse_since_threshold("abc"), None);
}

#[test]
fn system_time_to_rfc3339_formats_unix_epoch() {
    let epoch = std::time::UNIX_EPOCH;
    let result = system_time_to_rfc3339(epoch);
    assert_eq!(result, "1970-01-01T00:00:00Z");
}

#[test]
fn read_log_file_tail_returns_last_n_lines() {
    let dir = temp_dir("tail-read");
    let path = dir.join("test.log");
    let mut content = String::new();
    for i in 0..20 {
        content.push_str(&format!("line {i}\n"));
    }
    fs::write(&path, &content).expect("log file should be writable");

    let (lines, truncated) = read_log_file_tail(&path, 5, None).expect("should read log file");
    assert_eq!(lines.len(), 5);
    assert!(
        truncated,
        "should be truncated when file has more lines than requested"
    );
    assert_eq!(lines[0], "line 15");
    assert_eq!(lines[4], "line 19");

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn read_log_file_tail_skips_empty_lines() {
    let dir = temp_dir("tail-empty");
    let path = dir.join("test.log");
    fs::write(&path, "line one\n\n\nline two\n\n").expect("log should be writable");

    let (lines, truncated) = read_log_file_tail(&path, 100, None).expect("should read log file");
    assert_eq!(lines.len(), 2);
    assert!(!truncated);
    assert_eq!(lines[0], "line one");
    assert_eq!(lines[1], "line two");

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn read_log_file_tail_with_since_filter() {
    let dir = temp_dir("tail-since");
    let path = dir.join("test.log");
    let content = "\
2026-03-01T09:00:00Z old line\n\
2026-03-01T10:00:00Z new line one\n\
2026-03-01T11:00:00Z new line two\n";
    fs::write(&path, content).expect("log should be writable");

    let (lines, _) =
        read_log_file_tail(&path, 100, Some("2026-03-01T10:00:00Z")).expect("should read log file");
    assert_eq!(lines.len(), 2);
    assert!(lines[0].contains("new line one"));
    assert!(lines[1].contains("new line two"));

    let _ = fs::remove_dir_all(dir);
}

// ── Browser (Pinchtab) infrastructure tests ─────────────────────────────────

#[test]
fn find_available_port_returns_some_port_in_range() {
    // This test may fail if all ports in the range are in use, which is
    // unlikely in a test environment.
    let port = super::find_available_port();
    assert!(port.is_some(), "should find at least one available port");
    let port = port.unwrap();
    assert!(
        (types::DEFAULT_PINCHTAB_PORT..types::DEFAULT_PINCHTAB_PORT + types::PINCHTAB_PORT_RANGE)
            .contains(&port),
        "port {port} should be within the expected range"
    );
}

#[test]
fn generate_bridge_token_produces_hex_string() {
    let token = super::generate_bridge_token();
    assert!(!token.is_empty(), "token should not be empty");
    assert!(
        token.len() >= 32,
        "token should be at least 32 characters, got {}",
        token.len()
    );
    assert!(
        token.chars().all(|c| c.is_ascii_hexdigit()),
        "token should contain only hex characters, got: {token}"
    );
}

#[test]
fn generate_bridge_token_produces_unique_values() {
    let t1 = super::generate_bridge_token();
    // Sleep a tiny bit to ensure different timestamps.
    std::thread::sleep(std::time::Duration::from_millis(1));
    let t2 = super::generate_bridge_token();
    assert_ne!(t1, t2, "two consecutive tokens should differ");
}

#[test]
fn build_browser_env_returns_expected_vars() {
    let (env_vars, pinchtab_url) = super::build_browser_env(9867, "test-token-abc");
    assert_eq!(pinchtab_url, "http://127.0.0.1:9867");
    assert!(env_vars.contains(&"BROWSER_ENABLED=true".to_owned()));
    assert!(env_vars.contains(&"BRIDGE_PORT=9867".to_owned()));
    assert!(env_vars.contains(&"BRIDGE_BIND=127.0.0.1".to_owned()));
    assert!(env_vars.contains(&"BRIDGE_TOKEN=test-token-abc".to_owned()));
    assert!(env_vars.contains(&"BRIDGE_HEADLESS=true".to_owned()));
    assert!(env_vars.contains(&"CHROME_BINARY=/usr/bin/chromium".to_owned()));
    assert!(env_vars.contains(&"CHROME_FLAGS=--no-sandbox".to_owned()));
}

#[test]
fn apply_browser_shell_overlay_adds_required_commands() {
    let mut config = types::ShellConfig::default();
    super::apply_browser_shell_overlay(&mut config);

    let allow = config.allow.expect("allow should be set");
    assert!(allow.contains(&"curl".to_owned()));
    assert!(allow.contains(&"jq".to_owned()));
    assert!(allow.contains(&"sleep".to_owned()));
    assert_eq!(config.allow_operators, Some(true));
}

#[test]
fn apply_browser_shell_overlay_does_not_duplicate_existing_commands() {
    let mut config = types::ShellConfig {
        allow: Some(vec!["curl".to_owned(), "git".to_owned()]),
        allow_operators: Some(true),
        ..Default::default()
    };
    super::apply_browser_shell_overlay(&mut config);

    let allow = config.allow.expect("allow should be set");
    let curl_count = allow.iter().filter(|c| c.as_str() == "curl").count();
    assert_eq!(curl_count, 1, "curl should not be duplicated");
    assert!(allow.contains(&"jq".to_owned()));
    assert!(allow.contains(&"sleep".to_owned()));
    assert!(
        allow.contains(&"git".to_owned()),
        "existing entries preserved"
    );
}

#[test]
fn apply_browser_shell_overlay_preserves_existing_deny_and_replace_defaults() {
    let mut config = types::ShellConfig {
        allow: Some(vec!["npm".to_owned()]),
        deny: Some(vec!["rm".to_owned()]),
        replace_defaults: Some(true),
        allow_operators: Some(false),
        env_keys: Some(vec!["MY_KEY".to_owned()]),
    };
    super::apply_browser_shell_overlay(&mut config);

    assert_eq!(config.deny.as_ref().unwrap(), &["rm"]);
    assert_eq!(config.replace_defaults, Some(true));
    assert_eq!(config.allow_operators, Some(true)); // overridden
    assert_eq!(config.env_keys.as_ref().unwrap(), &["MY_KEY"]);
    let allow = config.allow.unwrap();
    assert!(allow.contains(&"npm".to_owned()));
    assert!(allow.contains(&"curl".to_owned()));
}

#[test]
fn extract_builtin_references_writes_embedded_reference_files() {
    let shared_dir = temp_dir("extract-refs");
    crate::skills::extract_builtin_references(&shared_dir);

    // The embedded BrowserAutomation skill should have its reference extracted.
    let target = shared_dir.join(".oxydra/skills/BrowserAutomation/references/pinchtab-api.md");
    assert!(
        target.is_file(),
        "expected embedded reference file at {}",
        target.display()
    );

    let content = fs::read_to_string(&target).unwrap();
    assert!(
        content.contains("Pinchtab"),
        "reference file should contain Pinchtab API docs"
    );

    let _ = fs::remove_dir_all(shared_dir);
}

#[test]
fn discover_skills_includes_embedded_builtins() {
    // Use non-existent dirs so only embedded skills are found.
    let system = temp_dir("embed-sys");
    let workspace = temp_dir("embed-ws");

    let skills = crate::skills::discover_skills(&system, None, &workspace);

    // Should discover the embedded BrowserAutomation skill.
    assert!(
        skills
            .iter()
            .any(|s| s.metadata.name == "browser-automation"),
        "embedded browser-automation skill should be discovered; found: {:?}",
        skills.iter().map(|s| &s.metadata.name).collect::<Vec<_>>()
    );

    let _ = fs::remove_dir_all(system);
    let _ = fs::remove_dir_all(workspace);
}

#[test]
fn workspace_skill_overrides_embedded_builtin() {
    let system = temp_dir("override-sys");
    let workspace = temp_dir("override-ws");

    // Create a workspace-level skill that overrides the embedded one.
    let ws_skill_dir = workspace.join("skills/BrowserAutomation");
    fs::create_dir_all(&ws_skill_dir).unwrap();
    fs::write(
        ws_skill_dir.join("SKILL.md"),
        "---\nname: browser-automation\ndescription: Custom override\nrequires:\n  - shell_exec\nenv:\n  - PINCHTAB_URL\n---\nCustom override body.",
    )
    .unwrap();

    let skills = crate::skills::discover_skills(&system, None, &workspace);

    let browser = skills
        .iter()
        .find(|s| s.metadata.name == "browser-automation")
        .expect("browser-automation should exist");
    assert!(
        browser.content.contains("Custom override body"),
        "workspace skill should override embedded; got: {}",
        browser.content
    );

    let _ = fs::remove_dir_all(system);
    let _ = fs::remove_dir_all(workspace);
}

#[test]
fn browser_config_in_bootstrap_envelope_round_trips() {
    use types::BrowserToolConfig;

    let envelope = RunnerBootstrapEnvelope {
        user_id: "alice".to_owned(),
        sandbox_tier: SandboxTier::Container,
        workspace_root: "/workspace/alice".to_owned(),
        sidecar_endpoint: Some(SidecarEndpoint {
            transport: SidecarTransport::Unix,
            address: "/tmp/shell-daemon.sock".to_owned(),
        }),
        runtime_policy: None,
        startup_status: Some(StartupStatusReport {
            sandbox_tier: SandboxTier::Container,
            sidecar_available: true,
            shell_available: true,
            browser_available: true,
            degraded_reasons: Vec::new(),
        }),
        channels: None,
        browser_config: Some(BrowserToolConfig {
            pinchtab_base_url: "http://127.0.0.1:9867".to_owned(),
            bridge_token: Some("test-token-123".to_owned()),
        }),
    };

    let encoded = envelope
        .to_length_prefixed_json()
        .expect("envelope with browser_config should encode");
    let decoded = RunnerBootstrapEnvelope::from_length_prefixed_json(&encoded)
        .expect("envelope with browser_config should decode");
    assert_eq!(decoded, envelope);
    let bc = decoded
        .browser_config
        .expect("browser_config should be present");
    assert_eq!(bc.pinchtab_base_url, "http://127.0.0.1:9867");
    assert_eq!(bc.bridge_token.as_deref(), Some("test-token-123"));
}

#[test]
fn startup_with_browser_enabled_populates_browser_env_in_shell_vm() {
    let root = temp_dir("browser-env");
    let global_path = write_runner_config_fixture(&root, "container");
    write_user_config(
        &root.join("users/alice.toml"),
        r#"
[behavior]
browser_enabled = true
"#,
    );

    let backend = Arc::new(MockSandboxBackend::default());
    let runner = Runner::from_global_config_path_with_backend(&global_path, backend.clone())
        .expect("runner should initialize");
    let startup = runner
        .start_user_for_host(RunnerStartRequest::new("alice"), "linux")
        .expect("startup should succeed");

    // Verify browser_config is set in the bootstrap envelope.
    assert!(
        startup.bootstrap.browser_config.is_some(),
        "browser_config should be populated when browser is enabled"
    );
    let bc = startup.bootstrap.browser_config.as_ref().unwrap();
    assert!(
        bc.pinchtab_base_url.starts_with("http://127.0.0.1:"),
        "pinchtab_base_url should be a loopback URL, got: {}",
        bc.pinchtab_base_url
    );
    assert!(
        bc.bridge_token.is_some(),
        "bridge_token should be generated"
    );

    // Verify shell_env contains browser-specific variables.
    let launches = backend.recorded_launches();
    assert_eq!(launches.len(), 1);
    let shell_env = &launches[0].shell_env;
    assert!(
        shell_env.iter().any(|e| e == "BROWSER_ENABLED=true"),
        "shell_env should contain BROWSER_ENABLED=true, got: {shell_env:?}"
    );
    assert!(
        shell_env.iter().any(|e| e.starts_with("BRIDGE_TOKEN=")),
        "shell_env should contain BRIDGE_TOKEN, got: {shell_env:?}"
    );
    assert!(
        shell_env.iter().any(|e| e.starts_with("BRIDGE_PORT=")),
        "shell_env should contain BRIDGE_PORT, got: {shell_env:?}"
    );

    // Verify oxydra-vm extra_env contains PINCHTAB_URL.
    let extra_env = &launches[0].extra_env;
    assert!(
        extra_env.iter().any(|e| e.starts_with("PINCHTAB_URL=")),
        "extra_env should contain PINCHTAB_URL, got: {extra_env:?}"
    );
    assert!(
        extra_env.iter().any(|e| e.starts_with("BRIDGE_TOKEN=")),
        "extra_env should contain BRIDGE_TOKEN for skill template, got: {extra_env:?}"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn startup_with_browser_disabled_has_no_browser_config() {
    let root = temp_dir("browser-disabled");
    let global_path = write_runner_config_fixture(&root, "container");
    write_user_config(
        &root.join("users/alice.toml"),
        r#"
[behavior]
browser_enabled = false
"#,
    );

    let backend = Arc::new(MockSandboxBackend::default());
    let runner = Runner::from_global_config_path_with_backend(&global_path, backend.clone())
        .expect("runner should initialize");
    let startup = runner
        .start_user_for_host(RunnerStartRequest::new("alice"), "linux")
        .expect("startup should succeed");

    assert!(
        startup.bootstrap.browser_config.is_none(),
        "browser_config should not be set when browser is disabled"
    );

    let launches = backend.recorded_launches();
    let shell_env = &launches[0].shell_env;
    assert!(
        !shell_env.iter().any(|e| e.starts_with("BROWSER_ENABLED=")),
        "shell_env should not contain BROWSER_ENABLED when browser disabled, got: {shell_env:?}"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn startup_process_tier_never_provisions_browser() {
    let root = temp_dir("browser-process");
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

    assert!(
        startup.bootstrap.browser_config.is_none(),
        "browser_config should not be set in process tier"
    );
    assert!(!startup.browser_available);

    let _ = fs::remove_dir_all(root);
}
