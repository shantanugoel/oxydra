use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
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
    RunnerBootstrapEnvelope, RunnerResolvedMountPaths, RunnerResourceLimits, RunnerRuntimePolicy,
    SandboxTier, SidecarEndpoint, SidecarTransport, StartupDegradedReasonCode,
};

use super::*;

#[tool]
/// Read the contents of a file and return its text. Path is relative to the working directory.
async fn file_read(path: String) -> String {
    path
}

#[tokio::test]
async fn read_tool_executes_through_tool_contract() {
    let (workspace, runner) = temp_wasm_runner("read");
    let path = shared_file_path(&workspace, "read");
    fs::write(&path, "hello from read tool").expect("test setup should create file");
    let tool = ReadTool::new(runner);
    let contract: &dyn Tool = &tool;

    let result = contract
        .execute(
            &json!({ "path": path.to_string_lossy() }).to_string(),
            &ToolExecutionContext::default(),
        )
        .await
        .expect("read should succeed");

    assert_eq!(result, "hello from read tool");
    let _ = fs::remove_dir_all(workspace);
}

#[tokio::test]
async fn write_tool_writes_file_content() {
    let (workspace, runner) = temp_wasm_runner("write");
    let path = shared_file_path(&workspace, "write");
    let tool = WriteTool::new(runner);

    let result = tool
        .execute(
            &json!({
                "path": path.to_string_lossy(),
                "content": "persisted text"
            })
            .to_string(),
            &ToolExecutionContext::default(),
        )
        .await
        .expect("write should succeed");

    assert!(result.contains("wrote"));
    let written = fs::read_to_string(&path).expect("written file should be readable");
    assert_eq!(written, "persisted text");
    let _ = fs::remove_dir_all(workspace);
}

#[tokio::test]
async fn edit_tool_replaces_exact_single_match() {
    let (workspace, runner) = temp_wasm_runner("edit");
    let path = shared_file_path(&workspace, "edit");
    fs::write(&path, "alpha beta gamma").expect("test setup should create file");
    let tool = EditTool::new(runner);

    tool.execute(
        &json!({
            "path": path.to_string_lossy(),
            "old_text": "beta",
            "new_text": "delta"
        })
        .to_string(),
        &ToolExecutionContext::default(),
    )
    .await
    .expect("edit should succeed");

    let updated = fs::read_to_string(&path).expect("updated file should be readable");
    assert_eq!(updated, "alpha delta gamma");
    let _ = fs::remove_dir_all(workspace);
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
            &ToolExecutionContext::default(),
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
            &ToolExecutionContext::default(),
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
            &ToolExecutionContext::default(),
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
        .execute(
            &json!({ "command": command }).to_string(),
            &ToolExecutionContext::default(),
        )
        .await
        .expect("bash command should succeed");
    assert!(output.contains("hello"));
}

#[tokio::test]
async fn bootstrap_runtime_tools_without_bootstrap_disables_shell() {
    let RuntimeToolsBootstrap {
        registry,
        availability,
    } = bootstrap_runtime_tools(None, None).await;

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
        channels: None,
    };
    let RuntimeToolsBootstrap {
        registry,
        availability,
    } = bootstrap_runtime_tools(Some(&bootstrap), None).await;

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

#[cfg(unix)]
#[tokio::test]
async fn bootstrap_runtime_tools_waits_for_sidecar_socket_before_disabling_shell() {
    const DELAYED_SIDECAR_BIND: Duration = Duration::from_millis(500);
    let socket_path = temp_socket_path("ws5-shell-daemon-delayed");
    let _ = fs::remove_file(&socket_path);

    let delayed_socket_path = socket_path.clone();
    let server_task = tokio::spawn(async move {
        // Delay sidecar bind to exercise bootstrap retry behavior.
        sleep(DELAYED_SIDECAR_BIND).await;
        let listener = UnixListener::bind(&delayed_socket_path)
            .expect("delayed test unix listener should bind socket path");
        let server = ShellDaemonServer::default();
        let _ = server.serve_unix_listener(listener).await;
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
        channels: None,
    };
    let RuntimeToolsBootstrap {
        registry,
        availability,
    } = bootstrap_runtime_tools(Some(&bootstrap), None).await;

    let output = registry
        .execute(
            SHELL_EXEC_TOOL_NAME,
            r#"{"command":"printf ws5-delayed-sidecar"}"#,
        )
        .await
        .expect("sidecar-backed bash execution should succeed after delayed socket startup");
    assert_eq!(output, "ws5-delayed-sidecar");
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
    assert_eq!(
        generated.parameters["required"],
        runtime.parameters["required"]
    );
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
    let outside = env::temp_dir().join(format!(
        "oxydra-tools-policy-outside-{}-{}.txt",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    fs::write(&outside, "outside policy root").expect("outside file should be writable");
    let bootstrap = RunnerBootstrapEnvelope {
        user_id: "alice".to_owned(),
        sandbox_tier: SandboxTier::Container,
        workspace_root: workspace_root.to_string_lossy().into_owned(),
        sidecar_endpoint: None,
        runtime_policy: None,
        startup_status: None,
        channels: None,
    };
    let RuntimeToolsBootstrap { registry, .. } =
        bootstrap_runtime_tools(Some(&bootstrap), None).await;
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
        channels: None,
    };
    let RuntimeToolsBootstrap { registry, .. } =
        bootstrap_runtime_tools(Some(&bootstrap), None).await;

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
    registry.set_security_policy(Arc::new(WorkspaceSecurityPolicy::for_bootstrap_workspace(
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
    let RuntimeToolsBootstrap { registry, .. } = bootstrap_runtime_tools(None, None).await;
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
    let RuntimeToolsBootstrap { registry, .. } = bootstrap_runtime_tools(None, None).await;
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
        SEND_MEDIA_TOOL_NAME.to_owned(),
        SHELL_EXEC_TOOL_NAME.to_owned(),
    ]);
    assert_eq!(exposed, expected);
}

/// Create a temporary workspace under `/tmp` with the standard subdirectory
/// structure (`shared/`, `tmp/`, `vault/`).  Returns a WASM tool runner rooted
/// at the new workspace so tool invocations stay inside `/tmp`.
fn temp_wasm_runner(label: &str) -> (PathBuf, Arc<dyn WasmToolRunner>) {
    let root = temp_workspace_root(label);
    let runner: Arc<dyn WasmToolRunner> =
        Arc::new(HostWasmToolRunner::for_bootstrap_workspace(&root));
    (root, runner)
}

/// Return a file path inside the `shared/` subdirectory of a workspace root.
fn shared_file_path(workspace_root: &Path, label: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be monotonic for test unique ids")
        .as_nanos();
    workspace_root.join("shared").join(format!(
        "oxydra-tools-{label}-{}-{unique}.txt",
        std::process::id()
    ))
}

fn temp_workspace_root(label: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be monotonic for test unique ids")
        .as_nanos();
    let root = env::temp_dir().join(format!(
        "oxydra-tools-{label}-{}-{unique}",
        std::process::id()
    ));
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

    async fn execute(
        &self,
        _args: &str,
        _context: &ToolExecutionContext,
    ) -> Result<String, ToolError> {
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

    async fn execute(
        &self,
        _args: &str,
        _context: &ToolExecutionContext,
    ) -> Result<String, ToolError> {
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

// ---------------------------------------------------------------------------
// Memory tool tests
// ---------------------------------------------------------------------------

mod memory_tool_tests {
    use std::sync::Arc;

    use memory::LibsqlMemory;
    use serde_json::json;
    use types::{MemoryRetrieval, SafetyTier, Tool, ToolExecutionContext};

    use crate::{memory_tools::*, scratchpad_tools::*};

    fn temp_db_path(label: &str) -> String {
        use std::{
            env,
            sync::atomic::{AtomicU64, Ordering},
            time::{SystemTime, UNIX_EPOCH},
        };
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let mut path = env::temp_dir();
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should move forward")
            .as_nanos();
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        path.push(format!(
            "oxydra-tools-memory-{label}-{}-{unique}-{seq}.db",
            std::process::id()
        ));
        path.to_string_lossy().to_string()
    }

    async fn test_memory_backend() -> Arc<dyn MemoryRetrieval> {
        let db_path = temp_db_path("tool-test");
        Arc::new(
            LibsqlMemory::new_local(db_path)
                .await
                .expect("test memory should initialize"),
        )
    }

    fn test_context(user_id: &str, session_id: &str) -> ToolExecutionContext {
        ToolExecutionContext {
            user_id: Some(user_id.to_owned()),
            session_id: Some(session_id.to_owned()),
            provider: None,
            model: None,
            channel_capabilities: None,
            event_sender: None,
            channel_id: None,
            channel_context_id: None,
        }
    }

    #[tokio::test]
    async fn memory_search_tool_schema_is_valid() {
        let memory = test_memory_backend().await;
        let tool = MemorySearchTool::new(memory, 0.7, 0.3);
        let schema = tool.schema();
        assert_eq!(schema.name, MEMORY_SEARCH_TOOL_NAME);
        assert!(schema.description.is_some());
        assert_eq!(schema.parameters["type"], "object");
        assert!(
            schema.parameters["required"]
                .as_array()
                .expect("required should be array")
                .iter()
                .any(|v| v.as_str() == Some("query"))
        );
        assert!(schema.parameters["properties"]["tags"].is_object());
        assert_eq!(tool.safety_tier(), SafetyTier::ReadOnly);
    }

    #[tokio::test]
    async fn memory_save_tool_schema_is_valid() {
        let memory = test_memory_backend().await;
        let tool = MemorySaveTool::new(memory);
        let schema = tool.schema();
        assert_eq!(schema.name, MEMORY_SAVE_TOOL_NAME);
        assert!(schema.description.is_some());
        let description = schema.description.as_deref().unwrap_or_default();
        assert!(description.contains("corrected procedures"));
        assert!(description.contains("Do NOT save ephemeral"));
        assert!(
            schema.parameters["required"]
                .as_array()
                .expect("required should be array")
                .iter()
                .any(|v| v.as_str() == Some("content"))
        );
        assert!(schema.parameters["properties"]["tags"].is_object());
        assert_eq!(tool.safety_tier(), SafetyTier::SideEffecting);
    }

    #[tokio::test]
    async fn memory_update_tool_schema_is_valid() {
        let memory = test_memory_backend().await;
        let tool = MemoryUpdateTool::new(memory);
        let schema = tool.schema();
        assert_eq!(schema.name, MEMORY_UPDATE_TOOL_NAME);
        let description = schema.description.as_deref().unwrap_or_default();
        assert!(description.contains("superseded by a better approach"));
        let required = schema.parameters["required"]
            .as_array()
            .expect("required should be array");
        assert!(required.iter().any(|v| v.as_str() == Some("note_id")));
        assert!(required.iter().any(|v| v.as_str() == Some("content")));
        assert!(schema.parameters["properties"]["tags"].is_object());
        assert_eq!(tool.safety_tier(), SafetyTier::SideEffecting);
    }

    #[tokio::test]
    async fn memory_delete_tool_schema_is_valid() {
        let memory = test_memory_backend().await;
        let tool = MemoryDeleteTool::new(memory);
        let schema = tool.schema();
        assert_eq!(schema.name, MEMORY_DELETE_TOOL_NAME);
        assert!(
            schema.parameters["required"]
                .as_array()
                .expect("required should be array")
                .iter()
                .any(|v| v.as_str() == Some("note_id"))
        );
        assert_eq!(tool.safety_tier(), SafetyTier::SideEffecting);
    }

    #[tokio::test]
    async fn memory_save_returns_note_id() {
        let memory = test_memory_backend().await;
        let ctx = test_context("alice", "runtime-alice");
        let tool = MemorySaveTool::new(memory);

        let result = tool
            .execute(&json!({"content": "User likes pizza"}).to_string(), &ctx)
            .await
            .expect("save should succeed");

        let parsed: serde_json::Value =
            serde_json::from_str(&result).expect("result should be valid JSON");
        let note_id = parsed["note_id"].as_str().expect("should have note_id");
        assert!(
            note_id.starts_with("note-"),
            "note_id should start with 'note-'"
        );
        assert_eq!(parsed["message"].as_str(), Some("Note saved successfully."));
    }

    #[tokio::test]
    async fn memory_save_rejects_empty_content() {
        let memory = test_memory_backend().await;
        let ctx = test_context("alice", "runtime-alice");
        let tool = MemorySaveTool::new(memory);

        let error = tool
            .execute(&json!({"content": "   "}).to_string(), &ctx)
            .await
            .expect_err("empty content should fail");
        assert!(matches!(
            error,
            types::ToolError::InvalidArguments { tool, .. } if tool == MEMORY_SAVE_TOOL_NAME
        ));
    }

    #[tokio::test]
    async fn memory_search_returns_saved_notes() {
        let memory = test_memory_backend().await;
        let ctx = test_context("alice", "runtime-alice");
        let save_tool = MemorySaveTool::new(memory.clone());
        let search_tool = MemorySearchTool::new(memory, 0.7, 0.3);

        save_tool
            .execute(
                &json!({"content": "User prefers dark mode"}).to_string(),
                &ctx,
            )
            .await
            .expect("save should succeed");

        let result = search_tool
            .execute(&json!({"query": "dark mode"}).to_string(), &ctx)
            .await
            .expect("search should succeed");

        let parsed: serde_json::Value =
            serde_json::from_str(&result).expect("result should be JSON");
        let results = parsed.as_array().expect("should be array");
        assert!(!results.is_empty(), "search should return results");
        let first = &results[0];
        assert!(first["text"].as_str().unwrap_or("").contains("dark mode"));
        assert_eq!(first["source"].as_str(), Some("user_memory"));
        assert!(
            !first["note_id"].as_str().unwrap_or("").is_empty(),
            "note_id should be present"
        );
        assert!(first["tags"].is_array(), "result should include tags");
        assert!(
            first["created_at"].is_string(),
            "result should include created_at"
        );
        assert!(
            first["updated_at"].is_string(),
            "result should include updated_at"
        );
    }

    #[tokio::test]
    async fn memory_search_can_filter_by_tags() {
        let memory = test_memory_backend().await;
        let ctx = test_context("alice", "runtime-alice");
        let save_tool = MemorySaveTool::new(memory.clone());
        let search_tool = MemorySearchTool::new(memory, 0.7, 0.3);

        save_tool
            .execute(
                &json!({"content": "Use rust for backend work", "tags": ["backend", "project-x"]})
                    .to_string(),
                &ctx,
            )
            .await
            .expect("first save should succeed");
        save_tool
            .execute(
                &json!({"content": "Use react for dashboard", "tags": ["frontend"]}).to_string(),
                &ctx,
            )
            .await
            .expect("second save should succeed");

        let filtered = search_tool
            .execute(
                &json!({"query": "use", "tags": ["project-x"]}).to_string(),
                &ctx,
            )
            .await
            .expect("tag-filtered search should succeed");
        let parsed: serde_json::Value = serde_json::from_str(&filtered).expect("json output");
        let results = parsed.as_array().expect("array output");
        assert_eq!(results.len(), 1, "tag filtering should narrow results");
        let first = &results[0];
        assert!(
            first["text"].as_str().unwrap_or("").contains("rust"),
            "filtered result should match tagged note"
        );
        assert_eq!(first["tags"], json!(["backend", "project-x"]));
    }

    #[tokio::test]
    async fn memory_delete_removes_note() {
        let memory = test_memory_backend().await;
        let ctx = test_context("alice", "runtime-alice");
        let save_tool = MemorySaveTool::new(memory.clone());
        let delete_tool = MemoryDeleteTool::new(memory.clone());
        let search_tool = MemorySearchTool::new(memory, 0.7, 0.3);

        let save_result = save_tool
            .execute(
                &json!({"content": "User likes chocolates"}).to_string(),
                &ctx,
            )
            .await
            .expect("save should succeed");
        let parsed: serde_json::Value = serde_json::from_str(&save_result).unwrap();
        let note_id = parsed["note_id"].as_str().unwrap();

        let delete_result = delete_tool
            .execute(&json!({"note_id": note_id}).to_string(), &ctx)
            .await
            .expect("delete should succeed");
        let parsed: serde_json::Value = serde_json::from_str(&delete_result).unwrap();
        assert_eq!(
            parsed["message"].as_str(),
            Some("Note deleted successfully.")
        );

        let search_result = search_tool
            .execute(&json!({"query": "chocolates"}).to_string(), &ctx)
            .await
            .expect("search should succeed");
        let parsed: serde_json::Value = serde_json::from_str(&search_result).unwrap();
        let results = parsed.as_array().unwrap();
        assert!(
            results.is_empty(),
            "deleted note should not appear in search"
        );
    }

    #[tokio::test]
    async fn memory_delete_returns_error_for_nonexistent_note() {
        let memory = test_memory_backend().await;
        let ctx = test_context("alice", "runtime-alice");
        let delete_tool = MemoryDeleteTool::new(memory);

        let error = delete_tool
            .execute(&json!({"note_id": "note-nonexistent"}).to_string(), &ctx)
            .await
            .expect_err("delete of nonexistent note should fail");
        assert!(matches!(
            error,
            types::ToolError::ExecutionFailed { tool, message }
                if tool == MEMORY_DELETE_TOOL_NAME && message.contains("not found")
        ));
    }

    #[tokio::test]
    async fn memory_update_replaces_note_content() {
        let memory = test_memory_backend().await;
        let ctx = test_context("alice", "runtime-alice");
        let save_tool = MemorySaveTool::new(memory.clone());
        let update_tool = MemoryUpdateTool::new(memory.clone());
        let search_tool = MemorySearchTool::new(memory, 0.7, 0.3);

        let save_result = save_tool
            .execute(
                &json!({"content": "User's name is Shantanu"}).to_string(),
                &ctx,
            )
            .await
            .expect("save should succeed");
        let parsed: serde_json::Value = serde_json::from_str(&save_result).unwrap();
        let note_id = parsed["note_id"].as_str().unwrap().to_owned();

        let update_result = update_tool
            .execute(
                &json!({"note_id": note_id, "content": "User prefers SG"}).to_string(),
                &ctx,
            )
            .await
            .expect("update should succeed");
        let parsed: serde_json::Value = serde_json::from_str(&update_result).unwrap();
        assert_eq!(parsed["note_id"].as_str(), Some(note_id.as_str()));
        assert_eq!(
            parsed["message"].as_str(),
            Some("Note updated successfully.")
        );

        let search_result = search_tool
            .execute(&json!({"query": "name preference"}).to_string(), &ctx)
            .await
            .expect("search should succeed");
        let parsed: serde_json::Value = serde_json::from_str(&search_result).unwrap();
        let results = parsed.as_array().unwrap();
        assert!(!results.is_empty(), "search should find the updated note");
        let has_sg = results
            .iter()
            .any(|r| r["text"].as_str().unwrap_or("").contains("SG"));
        assert!(has_sg, "updated note should contain 'SG'");
    }

    #[tokio::test]
    async fn memory_update_returns_error_for_nonexistent_note() {
        let memory = test_memory_backend().await;
        let ctx = test_context("alice", "runtime-alice");
        let update_tool = MemoryUpdateTool::new(memory);

        let error = update_tool
            .execute(
                &json!({"note_id": "note-nonexistent", "content": "new content"}).to_string(),
                &ctx,
            )
            .await
            .expect_err("update of nonexistent note should fail");
        assert!(matches!(
            error,
            types::ToolError::ExecutionFailed { tool, message }
                if tool == MEMORY_UPDATE_TOOL_NAME && message.contains("not found")
        ));
    }

    #[tokio::test]
    async fn memory_search_fails_without_user_context() {
        let memory = test_memory_backend().await;
        let empty_ctx = ToolExecutionContext::default();
        let tool = MemorySearchTool::new(memory, 0.7, 0.3);

        let error = tool
            .execute(&json!({"query": "test"}).to_string(), &empty_ctx)
            .await
            .expect_err("search without user context should fail");
        assert!(matches!(
            error,
            types::ToolError::ExecutionFailed { tool, message }
                if tool == MEMORY_SEARCH_TOOL_NAME && message.contains("user context")
        ));
    }

    #[tokio::test]
    async fn register_memory_tools_adds_four_tools_to_registry() {
        let memory = test_memory_backend().await;
        let mut registry = crate::ToolRegistry::default();

        register_memory_tools(&mut registry, memory, 0.7, 0.3);

        assert!(registry.get(MEMORY_SEARCH_TOOL_NAME).is_some());
        assert!(registry.get(MEMORY_SAVE_TOOL_NAME).is_some());
        assert!(registry.get(MEMORY_UPDATE_TOOL_NAME).is_some());
        assert!(registry.get(MEMORY_DELETE_TOOL_NAME).is_some());

        let schemas = registry.schemas();
        let names: Vec<String> = schemas.iter().map(|s| s.name.clone()).collect();
        assert!(names.contains(&MEMORY_SEARCH_TOOL_NAME.to_owned()));
        assert!(names.contains(&MEMORY_SAVE_TOOL_NAME.to_owned()));
        assert!(names.contains(&MEMORY_UPDATE_TOOL_NAME.to_owned()));
        assert!(names.contains(&MEMORY_DELETE_TOOL_NAME.to_owned()));
    }

    #[tokio::test]
    async fn memory_notes_are_isolated_per_user() {
        let memory = test_memory_backend().await;
        let ctx_alice = test_context("alice", "runtime-alice");
        let ctx_bob = test_context("bob", "runtime-bob");

        let save_tool = MemorySaveTool::new(memory.clone());
        let search_tool = MemorySearchTool::new(memory, 0.7, 0.3);

        save_tool
            .execute(
                &json!({"content": "Alice's secret preference"}).to_string(),
                &ctx_alice,
            )
            .await
            .expect("save for alice should succeed");

        // Bob should not see Alice's notes
        let result = search_tool
            .execute(&json!({"query": "secret preference"}).to_string(), &ctx_bob)
            .await
            .expect("search for bob should succeed");
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let results = parsed.as_array().unwrap();
        assert!(results.is_empty(), "bob should not see alice's notes");

        // Alice should see her own notes
        let result = search_tool
            .execute(
                &json!({"query": "secret preference"}).to_string(),
                &ctx_alice,
            )
            .await
            .expect("search for alice should succeed");
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let results = parsed.as_array().unwrap();
        assert!(!results.is_empty(), "alice should see her own notes");
    }

    #[tokio::test]
    async fn scratchpad_tools_schema_is_valid() {
        let memory = test_memory_backend().await;
        let read_tool = ScratchpadReadTool::new(memory.clone());
        let write_tool = ScratchpadWriteTool::new(memory.clone());
        let clear_tool = ScratchpadClearTool::new(memory);

        let read_schema = read_tool.schema();
        assert_eq!(read_schema.name, SCRATCHPAD_READ_TOOL_NAME);
        assert_eq!(read_tool.safety_tier(), SafetyTier::ReadOnly);

        let write_schema = write_tool.schema();
        assert_eq!(write_schema.name, SCRATCHPAD_WRITE_TOOL_NAME);
        assert!(
            write_schema.parameters["required"]
                .as_array()
                .expect("required should be array")
                .iter()
                .any(|value| value.as_str() == Some("items"))
        );
        assert_eq!(write_tool.safety_tier(), SafetyTier::SideEffecting);

        let clear_schema = clear_tool.schema();
        assert_eq!(clear_schema.name, SCRATCHPAD_CLEAR_TOOL_NAME);
        assert_eq!(clear_tool.safety_tier(), SafetyTier::SideEffecting);
    }

    #[tokio::test]
    async fn scratchpad_tools_write_read_clear_roundtrip() {
        let memory = test_memory_backend().await;
        let ctx = test_context("alice", "runtime-scratchpad");
        let write_tool = ScratchpadWriteTool::new(memory.clone());
        let read_tool = ScratchpadReadTool::new(memory.clone());
        let clear_tool = ScratchpadClearTool::new(memory.clone());

        let write_result = write_tool
            .execute(
                &json!({"items": ["Collect logs", "Patch parser"]}).to_string(),
                &ctx,
            )
            .await
            .expect("scratchpad write should succeed");
        let parsed_write: serde_json::Value =
            serde_json::from_str(&write_result).expect("write output should parse");
        assert_eq!(parsed_write["item_count"].as_u64(), Some(2));

        let read_result = read_tool
            .execute(&json!({}).to_string(), &ctx)
            .await
            .expect("scratchpad read should succeed");
        let parsed_read: serde_json::Value =
            serde_json::from_str(&read_result).expect("read output should parse");
        assert_eq!(parsed_read["item_count"].as_u64(), Some(2));
        assert_eq!(
            parsed_read["items"],
            json!(["Collect logs", "Patch parser"])
        );
        assert!(parsed_read["updated_at"].is_string());

        let clear_result = clear_tool
            .execute(&json!({}).to_string(), &ctx)
            .await
            .expect("scratchpad clear should succeed");
        let parsed_clear: serde_json::Value =
            serde_json::from_str(&clear_result).expect("clear output should parse");
        assert_eq!(parsed_clear["cleared"].as_bool(), Some(true));

        let read_after_clear = read_tool
            .execute(&json!({}).to_string(), &ctx)
            .await
            .expect("scratchpad read after clear should succeed");
        let parsed_after_clear: serde_json::Value =
            serde_json::from_str(&read_after_clear).expect("read output should parse");
        assert_eq!(parsed_after_clear["item_count"].as_u64(), Some(0));
        assert_eq!(parsed_after_clear["items"], json!([]));
        assert!(parsed_after_clear["updated_at"].is_null());
    }

    #[tokio::test]
    async fn register_scratchpad_tools_adds_tools_to_registry() {
        let memory = test_memory_backend().await;
        let mut registry = crate::ToolRegistry::default();
        register_scratchpad_tools(&mut registry, memory);

        assert!(registry.get(SCRATCHPAD_READ_TOOL_NAME).is_some());
        assert!(registry.get(SCRATCHPAD_WRITE_TOOL_NAME).is_some());
        assert!(registry.get(SCRATCHPAD_CLEAR_TOOL_NAME).is_some());
    }
}
