use std::{
    env, fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use runner::{PROCESS_TIER_WARNING, Runner, RunnerStartRequest};
use types::SandboxTier;

#[test]
fn runner_provisions_workspace_layout_on_first_start() {
    let root = temp_dir("integration-workspace");
    let global_path = write_runner_config_fixture(&root, "container");
    write_user_config(&root.join("users/alice.toml"), "");

    let runner = Runner::from_global_config_path(&global_path).expect("runner should initialize");
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

    let runner = Runner::from_global_config_path(&global_path).expect("runner should initialize");
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
