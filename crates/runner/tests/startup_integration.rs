use std::{
    env, fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use runner::{
    PROCESS_TIER_WARNING, Runner, RunnerCommandSpec, RunnerError, RunnerGuestHandle,
    RunnerGuestRole, RunnerLaunchHandle, RunnerScopeHandle, RunnerStartRequest, SandboxBackend,
    SandboxLaunch, SandboxLaunchRequest,
};
use types::{SandboxTier, SidecarEndpoint, SidecarTransport};

#[derive(Debug)]
struct IntegrationSandboxBackend;

impl SandboxBackend for IntegrationSandboxBackend {
    fn launch(&self, request: SandboxLaunchRequest) -> Result<SandboxLaunch, RunnerError> {
        let runtime = RunnerGuestHandle::simulated(
            RunnerGuestRole::OxydraVm,
            RunnerCommandSpec::new("integration-oxydra-vm", Vec::new()),
        );
        let sidecar_requested = request.requested_shell || request.requested_browser;
        let sidecar = sidecar_requested.then(|| {
            RunnerGuestHandle::simulated(
                RunnerGuestRole::ShellVm,
                RunnerCommandSpec::new("integration-shell-vm", Vec::new()),
            )
        });

        let sidecar_transport = match request.sandbox_tier {
            SandboxTier::MicroVm if request.host_os == "linux" => SidecarTransport::Vsock,
            SandboxTier::MicroVm | SandboxTier::Container => SidecarTransport::Unix,
            SandboxTier::Process => SidecarTransport::Unix,
        };
        let sidecar_endpoint = sidecar_requested.then(|| SidecarEndpoint {
            transport: sidecar_transport,
            address: "/tmp/integration-shell-daemon.sock".to_owned(),
        });

        let mut warnings = Vec::new();
        if request.sandbox_tier == SandboxTier::Process {
            warnings.push(PROCESS_TIER_WARNING.to_owned());
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
            warnings,
        })
    }
}

#[test]
fn runner_provisions_workspace_layout_on_first_start() {
    let root = temp_dir("integration-workspace");
    let global_path = write_runner_config_fixture(&root, "container");
    write_user_config(&root.join("users/alice.toml"), "");

    let runner = Runner::from_global_config_path_with_backend(
        &global_path,
        Arc::new(IntegrationSandboxBackend),
    )
    .expect("runner should initialize");
    let startup = runner
        .start_user_for_host(RunnerStartRequest::new("alice"), "linux")
        .expect("startup should succeed");

    assert_eq!(startup.sandbox_tier, SandboxTier::Container);
    assert!(startup.workspace.shared.is_dir());
    assert!(startup.workspace.tmp.is_dir());
    assert!(startup.workspace.vault.is_dir());

    let _ = fs::remove_dir_all(root);
}

#[test]
fn runner_process_mode_reports_explicit_degraded_warning() {
    let root = temp_dir("integration-process-warning");
    let global_path = write_runner_config_fixture(&root, "micro_vm");
    write_user_config(&root.join("users/alice.toml"), "");

    let runner = Runner::from_global_config_path_with_backend(
        &global_path,
        Arc::new(IntegrationSandboxBackend),
    )
    .expect("runner should initialize");
    let startup = runner
        .start_user_for_host(
            RunnerStartRequest {
                user_id: "alice".to_owned(),
                insecure: true,
            },
            "linux",
        )
        .expect("startup should succeed");

    assert_eq!(startup.sandbox_tier, SandboxTier::Process);
    assert!(!startup.shell_available);
    assert!(!startup.browser_available);
    assert_eq!(startup.warnings, vec![PROCESS_TIER_WARNING.to_owned()]);

    let _ = fs::remove_dir_all(root);
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
        "oxydra-runner-integration-{label}-{}-{unique}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("temp dir should be creatable");
    path
}
