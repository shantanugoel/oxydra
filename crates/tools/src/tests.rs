use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use serde_json::json;
#[cfg(unix)]
use shell_daemon::ShellDaemonServer;
#[cfg(unix)]
use tokio::net::UnixListener;
use tokio::time::sleep;
use tools_macros::tool;
use types::{
    RunnerBootstrapEnvelope, RunnerResolvedMountPaths, RunnerResourceLimits,
    RunnerRuntimePolicy, SandboxTier, SidecarEndpoint, SidecarTransport, StartupDegradedReasonCode,
};

use super::*;

#[tool]
/// Read the contents of a file and return its text. Path is relative to the working directory.
async fn file_read(path: String) -> String {
    path
}

#[tokio::test]
async fn read_tool_executes_through_tool_contract() {
    let path = temp_path("read");
    fs::write(&path, "hello from read tool").expect("test setup should create file");
    let tool = ReadTool::default();
    let contract: &dyn Tool = &tool;

    let result = contract
        .execute(&json!({ "path": path.to_string_lossy() }).to_string())
        .await
        .expect("read should succeed");

    assert_eq!(result, "hello from read tool");
    cleanup_file(&path);
}

#[tokio::test]
async fn write_tool_writes_file_content() {
    let path = temp_path("write");
    let tool = WriteTool::default();

    let result = tool
        .execute(
            &json!({
                "path": path.to_string_lossy(),
                "content": "persisted text"
            })
            .to_string(),
        )
        .await
        .expect("write should succeed");

    assert!(result.contains("wrote"));
    let written = fs::read_to_string(&path).expect("written file should be readable");
    assert_eq!(written, "persisted text");
    cleanup_file(&path);
}

#[tokio::test]
async fn edit_tool_replaces_exact_single_match() {
    let path = temp_path("edit");
    fs::write(&path, "alpha beta gamma").expect("test setup should create file");
    let tool = EditTool::default();

    tool.execute(
        &json!({
            "path": path.to_string_lossy(),
            "old_text": "beta",
            "new_text": "delta"
        })
        .to_string(),
    )
    .await
    .expect("edit should succeed");

    let updated = fs::read_to_string(&path).expect("updated file should be readable");
    assert_eq!(updated, "alpha delta gamma");
    cleanup_file(&path);
}

#[tokio::test]
async fn vault_copyto_uses_two_auditable_wasm_invocations_with_disjoint_mounts() {
    let workspace_root = temp_workspace_root("vault-copyto");
    let source_path = workspace_root.join("vault").join("source.txt");
    let destination_path = workspace_root.join("shared").join("copied.txt");
    fs::write(&source_path, "vault payload").expect("vault source file should be writable");

    let runner = Arc::new(HostWasmToolRunner::for_bootstrap_workspace(&workspace_root));
    let tool = VaultCopyToTool::new(runner.clone());
    let result = tool
        .execute(
            &json!({
                "source_path": source_path.to_string_lossy(),
                "destination_path": destination_path.to_string_lossy(),
            })
            .to_string(),
        )
        .await
        .expect("vault_copyto should succeed");
    assert!(result.contains("copied"));
    assert_eq!(
        fs::read_to_string(&destination_path).expect("destination file should be readable"),
        "vault payload"
    );

    let audit = runner.audit_log();
    assert_eq!(audit.len(), 2);
    let read_step = &audit[0];
    let write_step = &audit[1];
    assert_eq!(read_step.tool_name, VAULT_COPYTO_READ_OPERATION);
    assert_eq!(write_step.tool_name, VAULT_COPYTO_WRITE_OPERATION);
    assert_eq!(read_step.profile, WasmCapabilityProfile::VaultReadStep);
    assert_eq!(write_step.profile, WasmCapabilityProfile::VaultWriteStep);
    assert_eq!(read_step.operation_id, write_step.operation_id);
    assert!(read_step.operation_id.is_some());

    assert_eq!(read_step.mounts.len(), 1);
    assert_eq!(read_step.mounts[0].label, "vault");
    assert!(read_step.mounts[0].read_only);

    let write_labels = write_step
        .mounts
        .iter()
        .map(|mount| mount.label)
        .collect::<BTreeSet<_>>();
    assert_eq!(write_labels, BTreeSet::from(["shared", "tmp"]));
    assert!(write_step.mounts.iter().all(|mount| !mount.read_only));
    assert!(write_step.mounts.iter().all(|mount| mount.label != "vault"));

    let _ = fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn vault_copyto_propagates_read_step_errors_without_running_write_step() {
    let workspace_root = temp_workspace_root("vault-copyto-read-error");
    let source_path = workspace_root.join("vault").join("missing.txt");
    let destination_path = workspace_root.join("shared").join("copied.txt");
    let runner = Arc::new(HostWasmToolRunner::for_bootstrap_workspace(&workspace_root));
    let tool = VaultCopyToTool::new(runner.clone());

    let error = tool
        .execute(
            &json!({
                "source_path": source_path.to_string_lossy(),
                "destination_path": destination_path.to_string_lossy(),
            })
            .to_string(),
        )
        .await
        .expect_err("missing source should fail in read step");
    assert!(matches!(
        error,
        ToolError::ExecutionFailed { tool, message }
            if tool == VAULT_COPYTO_READ_OPERATION
                && message.contains("failed to read vault source")
    ));
    assert_eq!(runner.audit_log().len(), 1);

    let _ = fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn vault_copyto_propagates_write_step_errors_after_read_step() {
    let workspace_root = temp_workspace_root("vault-copyto-write-error");
    let source_path = workspace_root.join("vault").join("source.txt");
    let destination_path = workspace_root
        .join("shared")
        .join("nested")
        .join("copied.txt");
    fs::write(&source_path, "vault payload").expect("vault source file should be writable");
    let runner = Arc::new(HostWasmToolRunner::for_bootstrap_workspace(&workspace_root));
    let tool = VaultCopyToTool::new(runner.clone());

    let error = tool
        .execute(
            &json!({
                "source_path": source_path.to_string_lossy(),
                "destination_path": destination_path.to_string_lossy(),
            })
            .to_string(),
        )
        .await
        .expect_err("missing destination parent should fail in write step");
    assert!(matches!(
        error,
        ToolError::ExecutionFailed { tool, message }
            if tool == VAULT_COPYTO_WRITE_OPERATION
                && message.contains("failed to write destination")
    ));
    assert_eq!(runner.audit_log().len(), 2);

    let _ = fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn bash_tool_executes_command() {
    let command = if cfg!(target_os = "windows") {
        "echo hello"
    } else {
        "printf hello"
    };

    let output = BashTool::default()
        .execute(&json!({ "command": command }).to_string())
        .await
        .expect("bash command should succeed");
    assert!(output.contains("hello"));
}

#[tokio::test]
async fn bootstrap_runtime_tools_without_bootstrap_disables_shell() {
    let RuntimeToolsBootstrap {
        registry,
        availability,
    } = bootstrap_runtime_tools(None).await;

    assert!(!availability.shell.is_ready());
    assert!(!availability.browser.is_ready());
    let error = registry
        .execute(
            SHELL_EXEC_TOOL_NAME,
            r#"{"command":"printf should-not-run"}"#,
        )
        .await
        .expect_err("bash should be explicitly disabled when runner bootstrap is absent");
    assert!(matches!(
        error,
        ToolError::ExecutionFailed { tool, message }
            if tool == SHELL_EXEC_TOOL_NAME && message.contains("disabled")
    ));
    let startup_status = availability.startup_status(None);
    assert!(startup_status.is_degraded());
    assert!(startup_status.has_reason_code(StartupDegradedReasonCode::InsecureProcessTier));
}

#[cfg(unix)]
#[tokio::test]
async fn bootstrap_runtime_tools_runs_bash_via_sidecar_session() {
    let socket_path = temp_socket_path("ws5-shell-daemon");
    let _ = fs::remove_file(&socket_path);
    let listener =
        UnixListener::bind(&socket_path).expect("test unix listener should bind socket path");
    let server = ShellDaemonServer::default();
    let server_task = tokio::spawn({
        let server = server.clone();
        async move { server.serve_unix_listener(listener).await }
    });

    let bootstrap = RunnerBootstrapEnvelope {
        user_id: "alice".to_owned(),
        sandbox_tier: SandboxTier::Container,
        workspace_root: "/tmp/oxydra-test".to_owned(),
        sidecar_endpoint: Some(SidecarEndpoint {
            transport: SidecarTransport::Unix,
            address: socket_path.to_string_lossy().into_owned(),
        }),
        runtime_policy: None,
        startup_status: None,
    };
    let RuntimeToolsBootstrap {
        registry,
        availability,
    } = bootstrap_runtime_tools(Some(&bootstrap)).await;

    let output = registry
        .execute(SHELL_EXEC_TOOL_NAME, r#"{"command":"printf ws5-sidecar"}"#)
        .await
        .expect("sidecar-backed bash execution should succeed");
    assert_eq!(output, "ws5-sidecar");
    assert!(availability.shell.is_ready());
    assert!(availability.browser.is_ready());

    server_task.abort();
    let _ = fs::remove_file(socket_path);
}

#[test]
fn core_tool_schemas_and_metadata_match_tool_contract() {
    let read = ReadTool::default();
    let write = WriteTool::default();
    let edit = EditTool::default();
    let bash = BashTool::default();

    assert_eq!(read.safety_tier(), SafetyTier::ReadOnly);
    assert_eq!(write.safety_tier(), SafetyTier::SideEffecting);
    assert_eq!(edit.safety_tier(), SafetyTier::SideEffecting);
    assert_eq!(bash.safety_tier(), SafetyTier::Privileged);

    assert_eq!(read.schema().parameters["type"], "object");
    assert_eq!(write.schema().parameters["type"], "object");
    assert_eq!(edit.schema().parameters["type"], "object");
    assert_eq!(bash.schema().parameters["type"], "object");

    assert!(read.timeout().as_secs() > 0);
    assert!(write.timeout().as_secs() > 0);
    assert!(edit.timeout().as_secs() > 0);
    assert!(bash.timeout().as_secs() > 0);
}

#[test]
fn macro_generated_schema_matches_read_tool_contract() {
    std::mem::drop(file_read("placeholder".to_owned()));
    let generated = __tool_function_decl_file_read();
    let runtime = ReadTool::default().schema();

    assert_eq!(generated.name, runtime.name);
    // Macro-generated schemas do not include descriptions; verify the top-level
    // structure (type, required, property types) matches the runtime schema.
    assert_eq!(generated.parameters["type"], "object");
    assert_eq!(generated.parameters["required"], runtime.parameters["required"]);
    assert_eq!(
        generated.parameters["properties"]["path"]["type"],
        runtime.parameters["properties"]["path"]["type"]
    );
}

#[tokio::test]
async fn registry_enforces_policy_hooks_timeout_and_truncation() {
    let mut registry = ToolRegistry::new(8);
    registry.register("static_output", StaticOutputTool);
    registry.register("slow_tool", SlowTool);

    let output = registry
        .execute("static_output", "{}")
        .await
        .expect("static output tool should execute");
    assert!(output.starts_with("12345678"));
    assert!(output.contains("output truncated"));

    let timed_out = registry.execute("slow_tool", "{}").await;
    assert!(matches!(
        timed_out,
        Err(ToolError::ExecutionFailed { tool, message })
            if tool == "slow_tool" && message.contains("timed out")
    ));

    let gated = registry
        .execute_with_policy(
            SHELL_EXEC_TOOL_NAME,
            r#"{"command":"echo blocked"}"#,
            |tier| {
                if tier == SafetyTier::Privileged {
                    Err(execution_failed("policy", "blocked by safety gate"))
                } else {
                    Ok(())
                }
            },
        )
        .await;
    assert!(matches!(
        gated,
        Err(ToolError::ExecutionFailed { tool, message })
            if tool == SHELL_EXEC_TOOL_NAME && message.contains("unknown tool")
    ));
}

#[tokio::test]
async fn registry_can_gate_registered_privileged_tools() {
    let mut registry = ToolRegistry::default();
    registry.register(SHELL_EXEC_TOOL_NAME, BashTool::default());

    let gated = registry
        .execute_with_policy(
            SHELL_EXEC_TOOL_NAME,
            r#"{"command":"echo blocked"}"#,
            |tier| {
                if tier == SafetyTier::Privileged {
                    Err(execution_failed("policy", "blocked by safety gate"))
                } else {
                    Ok(())
                }
            },
        )
        .await;

    assert!(matches!(
        gated,
        Err(ToolError::ExecutionFailed { tool, message })
            if tool == "policy" && message == "blocked by safety gate"
    ));
}

#[tokio::test]
async fn bootstrap_registry_denies_file_reads_outside_workspace_roots() {
    let workspace_root = temp_workspace_root("policy-bootstrap");
    let outside = temp_path("policy-outside");
    fs::write(&outside, "outside policy root").expect("outside file should be writable");
    let bootstrap = RunnerBootstrapEnvelope {
        user_id: "alice".to_owned(),
        sandbox_tier: SandboxTier::Container,
        workspace_root: workspace_root.to_string_lossy().into_owned(),
        sidecar_endpoint: None,
        runtime_policy: None,
        startup_status: None,
    };
    let RuntimeToolsBootstrap { registry, .. } = bootstrap_runtime_tools(Some(&bootstrap)).await;
    let args = json!({ "path": outside.to_string_lossy() }).to_string();
    let denied = registry
        .execute(FILE_READ_TOOL_NAME, &args)
        .await
        .expect_err("policy should deny file reads outside configured workspace roots");

    assert!(matches!(
        denied,
        ToolError::ExecutionFailed { tool, message }
            if tool == FILE_READ_TOOL_NAME
                && message.contains("blocked by security policy")
                && message.contains("PathOutsideAllowedRoots")
    ));

    cleanup_file(&outside);
    let _ = fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn bootstrap_registry_honors_runtime_policy_mount_overrides() {
    let workspace_root = temp_workspace_root("policy-workspace-default");
    let override_root = temp_workspace_root("policy-workspace-override");
    let allowed = override_root.join("shared").join("allowed.txt");
    let denied = workspace_root.join("shared").join("denied.txt");
    fs::write(&allowed, "allowed").expect("allowed file should be writable");
    fs::write(&denied, "denied").expect("denied file should be writable");

    let bootstrap = RunnerBootstrapEnvelope {
        user_id: "alice".to_owned(),
        sandbox_tier: SandboxTier::Container,
        workspace_root: workspace_root.to_string_lossy().into_owned(),
        sidecar_endpoint: None,
        runtime_policy: Some(RunnerRuntimePolicy {
            mounts: RunnerResolvedMountPaths {
                shared: override_root.join("shared").to_string_lossy().into_owned(),
                tmp: override_root.join("tmp").to_string_lossy().into_owned(),
                vault: override_root.join("vault").to_string_lossy().into_owned(),
            },
            resources: RunnerResourceLimits::default(),
            credential_refs: BTreeMap::new(),
        }),
        startup_status: None,
    };
    let RuntimeToolsBootstrap { registry, .. } = bootstrap_runtime_tools(Some(&bootstrap)).await;

    let allowed_result = registry
        .execute(
            FILE_READ_TOOL_NAME,
            &json!({ "path": allowed.to_string_lossy() }).to_string(),
        )
        .await
        .expect("override mount path should be readable");
    assert_eq!(allowed_result, "allowed");

    let denied_result = registry
        .execute(
            FILE_READ_TOOL_NAME,
            &json!({ "path": denied.to_string_lossy() }).to_string(),
        )
        .await
        .expect_err("default workspace roots should be ignored when runtime policy mounts are set");
    assert!(matches!(
        denied_result,
        ToolError::ExecutionFailed { tool, message }
            if tool == FILE_READ_TOOL_NAME
                && message.contains("blocked by security policy")
                && message.contains("PathOutsideAllowedRoots")
    ));

    let _ = fs::remove_dir_all(workspace_root);
    let _ = fs::remove_dir_all(override_root);
}

#[tokio::test]
async fn registry_denies_shell_commands_not_in_allowlist() {
    let workspace_root = temp_workspace_root("policy-shell");
    let mut registry = ToolRegistry::default();
    registry.register(SHELL_EXEC_TOOL_NAME, BashTool::default());
    registry.set_security_policy(Arc::new(WorkspaceSecurityPolicy::for_direct_workspace(
        &workspace_root,
    )));

    let denied = registry
        .execute(
            SHELL_EXEC_TOOL_NAME,
            r#"{"command":"curl https://example.com"}"#,
        )
        .await
        .expect_err("security policy should deny shell commands outside allowlist");
    assert!(matches!(
        denied,
        ToolError::ExecutionFailed { tool, message }
            if tool == SHELL_EXEC_TOOL_NAME
                && message.contains("blocked by security policy")
                && message.contains("CommandNotAllowed")
    ));

    let _ = fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn bootstrap_registry_rejects_legacy_tool_name_aliases() {
    let RuntimeToolsBootstrap { registry, .. } = bootstrap_runtime_tools(None).await;
    for legacy_name in ["read_file", "write_file", "edit_file", "bash"] {
        let error = registry
            .execute(legacy_name, "{}")
            .await
            .expect_err("legacy alias should not be registered");
        assert!(matches!(
            error,
            ToolError::ExecutionFailed { tool, message }
                if tool == legacy_name && message.contains("unknown tool")
        ));
    }
}

#[tokio::test]
async fn bootstrap_registry_exposes_runtime_tool_surface_only() {
    let RuntimeToolsBootstrap { registry, .. } = bootstrap_runtime_tools(None).await;
    let exposed = registry
        .schemas()
        .into_iter()
        .map(|schema| schema.name)
        .collect::<BTreeSet<_>>();
    let expected = BTreeSet::from([
        FILE_READ_TOOL_NAME.to_owned(),
        FILE_SEARCH_TOOL_NAME.to_owned(),
        FILE_LIST_TOOL_NAME.to_owned(),
        FILE_WRITE_TOOL_NAME.to_owned(),
        FILE_EDIT_TOOL_NAME.to_owned(),
        FILE_DELETE_TOOL_NAME.to_owned(),
        WEB_FETCH_TOOL_NAME.to_owned(),
        WEB_SEARCH_TOOL_NAME.to_owned(),
        VAULT_COPYTO_TOOL_NAME.to_owned(),
        SHELL_EXEC_TOOL_NAME.to_owned(),
    ]);
    assert_eq!(exposed, expected);
}

fn temp_path(label: &str) -> PathBuf {
    let mut path = env::current_dir().unwrap_or_else(|_| env::temp_dir());
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be monotonic for test unique ids")
        .as_nanos();
    path.push(format!(
        "oxydra-tools-{label}-{}-{unique}.txt",
        std::process::id()
    ));
    path
}

fn temp_workspace_root(label: &str) -> PathBuf {
    let root = temp_path(label).with_extension("workspace");
    fs::create_dir_all(root.join("shared")).expect("shared directory should be created");
    fs::create_dir_all(root.join("tmp")).expect("tmp directory should be created");
    fs::create_dir_all(root.join("vault")).expect("vault directory should be created");
    root
}

#[cfg(unix)]
fn temp_socket_path(label: &str) -> PathBuf {
    let short_label = label
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .take(6)
        .collect::<String>();
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be monotonic for test unique ids")
        .as_nanos()
        % 1_000_000;
    PathBuf::from(format!(
        "/tmp/oxy-{short_label}-{}-{unique}.sock",
        std::process::id()
    ))
}

fn cleanup_file(path: &PathBuf) {
    let _ = fs::remove_file(path);
}

struct StaticOutputTool;

#[async_trait]
impl Tool for StaticOutputTool {
    fn schema(&self) -> FunctionDecl {
        FunctionDecl::new(
            "static_output",
            None,
            json!({ "type": "object", "properties": {} }),
        )
    }

    async fn execute(&self, _args: &str) -> Result<String, ToolError> {
        Ok("1234567890".to_owned())
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(1)
    }

    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::ReadOnly
    }
}

struct SlowTool;

#[async_trait]
impl Tool for SlowTool {
    fn schema(&self) -> FunctionDecl {
        FunctionDecl::new(
            "slow_tool",
            None,
            json!({ "type": "object", "properties": {} }),
        )
    }

    async fn execute(&self, _args: &str) -> Result<String, ToolError> {
        sleep(Duration::from_millis(25)).await;
        Ok("done".to_owned())
    }

    fn timeout(&self) -> Duration {
        Duration::from_millis(5)
    }

    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::ReadOnly
    }
}
