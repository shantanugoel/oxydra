use std::{collections::BTreeMap, fs, process::Command, sync::Arc, time::Duration};

use async_trait::async_trait;
use sandbox::{
    SessionStatus, SessionUnavailable, SessionUnavailableReason, ShellSession, ShellSessionConfig,
    VsockShellSession,
};
use serde::{Deserialize, de::DeserializeOwned};
#[cfg(unix)]
use tokio::net::UnixStream;
use tokio::sync::Mutex;
use types::{
    FunctionDecl, JsonSchema, JsonSchemaType, RunnerBootstrapEnvelope, SafetyTier, SandboxTier,
    ShellOutputStream, SidecarEndpoint, SidecarTransport, Tool, ToolError,
};

pub const READ_TOOL_NAME: &str = "read_file";
pub const WRITE_TOOL_NAME: &str = "write_file";
pub const EDIT_TOOL_NAME: &str = "edit_file";
pub const BASH_TOOL_NAME: &str = "bash";
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 16 * 1024;

#[derive(Debug, Default, Clone, Copy)]
pub struct ReadTool;

#[derive(Debug, Default, Clone, Copy)]
pub struct WriteTool;

#[derive(Debug, Default, Clone, Copy)]
pub struct EditTool;

pub struct BashTool {
    backend: BashBackend,
}

enum BashBackend {
    Host,
    Session(Arc<Mutex<Box<dyn ShellSession>>>),
    Disabled(SessionStatus),
}

#[derive(Debug, Deserialize)]
struct ReadArgs {
    path: String,
}

#[derive(Debug, Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct EditArgs {
    path: String,
    old_text: String,
    new_text: String,
}

#[derive(Debug, Deserialize)]
struct BashArgs {
    command: String,
}

impl Default for BashTool {
    fn default() -> Self {
        Self::host()
    }
}

impl BashTool {
    pub fn host() -> Self {
        Self {
            backend: BashBackend::Host,
        }
    }

    pub fn from_shell_session(session: Box<dyn ShellSession>) -> Self {
        Self {
            backend: BashBackend::Session(Arc::new(Mutex::new(session))),
        }
    }

    fn from_status(status: SessionStatus) -> Self {
        Self {
            backend: BashBackend::Disabled(status),
        }
    }
}

#[async_trait]
impl Tool for ReadTool {
    fn schema(&self) -> FunctionDecl {
        let mut properties = BTreeMap::new();
        properties.insert(
            "path".to_owned(),
            JsonSchema::new(JsonSchemaType::String).with_description("File path to read"),
        );

        FunctionDecl::new(
            READ_TOOL_NAME,
            Some("Read UTF-8 text from a file".to_owned()),
            JsonSchema::object(properties, vec!["path".to_owned()]),
        )
    }

    async fn execute(&self, args: &str) -> Result<String, ToolError> {
        let request: ReadArgs = parse_args(READ_TOOL_NAME, args)?;
        fs::read_to_string(&request.path)
            .map_err(|error| execution_failed(READ_TOOL_NAME, error.to_string()))
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(10)
    }

    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::ReadOnly
    }
}

#[async_trait]
impl Tool for WriteTool {
    fn schema(&self) -> FunctionDecl {
        let mut properties = BTreeMap::new();
        properties.insert(
            "path".to_owned(),
            JsonSchema::new(JsonSchemaType::String).with_description("File path to write"),
        );
        properties.insert(
            "content".to_owned(),
            JsonSchema::new(JsonSchemaType::String)
                .with_description("UTF-8 file content to persist"),
        );

        FunctionDecl::new(
            WRITE_TOOL_NAME,
            Some("Write UTF-8 text to a file".to_owned()),
            JsonSchema::object(properties, vec!["path".to_owned(), "content".to_owned()]),
        )
    }

    async fn execute(&self, args: &str) -> Result<String, ToolError> {
        let request: WriteArgs = parse_args(WRITE_TOOL_NAME, args)?;
        fs::write(&request.path, request.content.as_bytes())
            .map_err(|error| execution_failed(WRITE_TOOL_NAME, error.to_string()))?;
        Ok(format!(
            "wrote {} bytes to {}",
            request.content.len(),
            request.path
        ))
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(15)
    }

    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::SideEffecting
    }
}

#[async_trait]
impl Tool for EditTool {
    fn schema(&self) -> FunctionDecl {
        let mut properties = BTreeMap::new();
        properties.insert(
            "path".to_owned(),
            JsonSchema::new(JsonSchemaType::String).with_description("File path to edit"),
        );
        properties.insert(
            "old_text".to_owned(),
            JsonSchema::new(JsonSchemaType::String)
                .with_description("Exact text snippet to replace"),
        );
        properties.insert(
            "new_text".to_owned(),
            JsonSchema::new(JsonSchemaType::String).with_description("Replacement text"),
        );

        FunctionDecl::new(
            EDIT_TOOL_NAME,
            Some("Edit a file by replacing one exact text occurrence".to_owned()),
            JsonSchema::object(
                properties,
                vec![
                    "path".to_owned(),
                    "old_text".to_owned(),
                    "new_text".to_owned(),
                ],
            ),
        )
    }

    async fn execute(&self, args: &str) -> Result<String, ToolError> {
        let request: EditArgs = parse_args(EDIT_TOOL_NAME, args)?;
        if request.old_text.is_empty() {
            return Err(invalid_args(EDIT_TOOL_NAME, "old_text must not be empty"));
        }

        let content = fs::read_to_string(&request.path)
            .map_err(|error| execution_failed(EDIT_TOOL_NAME, error.to_string()))?;
        let occurrences = content.matches(&request.old_text).count();

        if occurrences == 0 {
            return Err(execution_failed(
                EDIT_TOOL_NAME,
                "old_text was not found in target file",
            ));
        }
        if occurrences > 1 {
            return Err(execution_failed(
                EDIT_TOOL_NAME,
                format!(
                    "old_text matched {occurrences} locations; provide a more specific snippet"
                ),
            ));
        }

        let updated = content.replacen(&request.old_text, &request.new_text, 1);
        fs::write(&request.path, updated.as_bytes())
            .map_err(|error| execution_failed(EDIT_TOOL_NAME, error.to_string()))?;

        Ok(format!("updated {}", request.path))
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(15)
    }

    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::SideEffecting
    }
}

#[async_trait]
impl Tool for BashTool {
    fn schema(&self) -> FunctionDecl {
        let mut properties = BTreeMap::new();
        properties.insert(
            "command".to_owned(),
            JsonSchema::new(JsonSchemaType::String)
                .with_description("Shell command executed through the active shell backend"),
        );

        FunctionDecl::new(
            BASH_TOOL_NAME,
            Some("Execute a shell command".to_owned()),
            JsonSchema::object(properties, vec!["command".to_owned()]),
        )
    }

    async fn execute(&self, args: &str) -> Result<String, ToolError> {
        let request: BashArgs = parse_args(BASH_TOOL_NAME, args)?;
        match &self.backend {
            BashBackend::Host => {
                let output = run_shell_command(&request.command)
                    .map_err(|error| execution_failed(BASH_TOOL_NAME, error.to_string()))?;
                let combined_output = combine_command_output(&output.stdout, &output.stderr);

                if output.status.success() {
                    if combined_output.is_empty() {
                        Ok("command completed with no output".to_owned())
                    } else {
                        Ok(combined_output)
                    }
                } else {
                    let status = output.status.code().map_or_else(
                        || "terminated by signal".to_owned(),
                        |code| code.to_string(),
                    );
                    let message = if combined_output.is_empty() {
                        format!("command exited with status {status}")
                    } else {
                        format!("command exited with status {status}: {combined_output}")
                    };
                    Err(execution_failed(BASH_TOOL_NAME, message))
                }
            }
            BashBackend::Session(session) => {
                execute_with_shell_session(session, &request.command, self.timeout()).await
            }
            BashBackend::Disabled(status) => Err(execution_failed(
                BASH_TOOL_NAME,
                shell_disabled_message(status),
            )),
        }
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(30)
    }

    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::Privileged
    }
}

pub struct ToolRegistry {
    tools: BTreeMap<String, Box<dyn Tool>>,
    max_output_bytes: usize,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_OUTPUT_BYTES)
    }
}

impl ToolRegistry {
    pub fn new(max_output_bytes: usize) -> Self {
        Self {
            tools: BTreeMap::new(),
            max_output_bytes,
        }
    }

    pub fn register<T>(&mut self, name: impl Into<String>, tool: T)
    where
        T: Tool + 'static,
    {
        self.tools.insert(name.into(), Box::new(tool));
    }

    pub fn register_core_tools(&mut self) {
        self.register(READ_TOOL_NAME, ReadTool);
        self.register(WRITE_TOOL_NAME, WriteTool);
        self.register(EDIT_TOOL_NAME, EditTool);
        self.register(BASH_TOOL_NAME, BashTool::default());
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(Box::as_ref)
    }

    pub fn schemas(&self) -> Vec<FunctionDecl> {
        self.tools.values().map(|tool| tool.schema()).collect()
    }

    pub async fn execute(&self, name: &str, args: &str) -> Result<String, ToolError> {
        self.execute_with_policy(name, args, |_| Ok(())).await
    }

    pub async fn execute_with_policy<F>(
        &self,
        name: &str,
        args: &str,
        mut safety_gate: F,
    ) -> Result<String, ToolError>
    where
        F: FnMut(SafetyTier) -> Result<(), ToolError>,
    {
        let tool = self
            .get(name)
            .ok_or_else(|| execution_failed(name, format!("unknown tool `{name}`")))?;

        safety_gate(tool.safety_tier())?;

        let timeout = tool.timeout();
        let output = tokio::time::timeout(timeout, tool.execute(args))
            .await
            .map_err(|_| execution_failed(name, format!("tool timed out after {timeout:?}")))??;

        Ok(truncate_output(output, self.max_output_bytes))
    }
}

pub fn default_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::default();
    registry.register_core_tools();
    registry
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolAvailability {
    pub shell: SessionStatus,
    pub browser: SessionStatus,
}

pub struct RuntimeToolsBootstrap {
    pub registry: ToolRegistry,
    pub availability: ToolAvailability,
}

pub async fn bootstrap_runtime_tools(
    bootstrap: Option<&RunnerBootstrapEnvelope>,
) -> RuntimeToolsBootstrap {
    let (bash_tool, shell_status, browser_status) = bootstrap_bash_tool(bootstrap).await;
    let mut registry = ToolRegistry::default();
    registry.register(READ_TOOL_NAME, ReadTool);
    registry.register(WRITE_TOOL_NAME, WriteTool);
    registry.register(EDIT_TOOL_NAME, EditTool);
    registry.register(BASH_TOOL_NAME, bash_tool);

    RuntimeToolsBootstrap {
        registry,
        availability: ToolAvailability {
            shell: shell_status,
            browser: browser_status,
        },
    }
}

async fn bootstrap_bash_tool(
    bootstrap: Option<&RunnerBootstrapEnvelope>,
) -> (BashTool, SessionStatus, SessionStatus) {
    let Some(bootstrap) = bootstrap else {
        let status = unavailable_status(
            SessionUnavailableReason::Disabled,
            "runner bootstrap envelope was not provided; shell/browser tools are disabled",
        );
        return (
            BashTool::from_status(status.clone()),
            status.clone(),
            status,
        );
    };

    let Some(endpoint) = bootstrap.sidecar_endpoint.clone() else {
        let detail = if bootstrap.sandbox_tier == SandboxTier::Process {
            "runner bootstrap indicates process tier; shell/browser tools are disabled"
        } else {
            "runner bootstrap did not provide a sidecar endpoint; shell/browser tools are disabled"
        };
        let status = unavailable_status(SessionUnavailableReason::Disabled, detail);
        return (
            BashTool::from_status(status.clone()),
            status.clone(),
            status,
        );
    };

    let (tool, shell_status) = connect_sidecar_bash_tool(endpoint).await;
    let browser_status = shell_status.clone();
    (tool, shell_status, browser_status)
}

async fn connect_sidecar_bash_tool(endpoint: SidecarEndpoint) -> (BashTool, SessionStatus) {
    match endpoint.transport {
        SidecarTransport::Vsock => {
            let session =
                VsockShellSession::connect(Some(endpoint), ShellSessionConfig::default()).await;
            let status = session.status().clone();
            if status.is_ready() {
                (BashTool::from_shell_session(Box::new(session)), status)
            } else {
                (BashTool::from_status(status.clone()), status)
            }
        }
        SidecarTransport::Unix => connect_unix_sidecar_bash_tool(endpoint).await,
    }
}

#[cfg(unix)]
async fn connect_unix_sidecar_bash_tool(endpoint: SidecarEndpoint) -> (BashTool, SessionStatus) {
    let socket_path = match sidecar_unix_socket_path(&endpoint.address) {
        Some(path) => path.to_owned(),
        None => {
            let status = unavailable_status(
                SessionUnavailableReason::InvalidAddress,
                format!(
                    "sidecar unix endpoint `{}` is not a valid unix socket path",
                    endpoint.address
                ),
            );
            return (BashTool::from_status(status.clone()), status);
        }
    };

    match UnixStream::connect(&socket_path).await {
        Ok(stream) => match VsockShellSession::connect_with_stream(
            stream,
            SidecarTransport::Unix,
            endpoint.address.clone(),
            ShellSessionConfig::default(),
        )
        .await
        {
            Ok(session) => {
                let status = session.status().clone();
                (BashTool::from_shell_session(Box::new(session)), status)
            }
            Err(error) => {
                let status = unavailable_status(
                    SessionUnavailableReason::ProtocolError,
                    format!(
                        "failed to initialize shell session over unix sidecar transport: {error}"
                    ),
                );
                (BashTool::from_status(status.clone()), status)
            }
        },
        Err(error) => {
            let status = unavailable_status(
                SessionUnavailableReason::ConnectionFailed,
                format!("failed to connect to unix sidecar socket `{socket_path}`: {error}"),
            );
            (BashTool::from_status(status.clone()), status)
        }
    }
}

#[cfg(not(unix))]
async fn connect_unix_sidecar_bash_tool(endpoint: SidecarEndpoint) -> (BashTool, SessionStatus) {
    let status = unavailable_status(
        SessionUnavailableReason::UnsupportedTransport,
        format!(
            "unix sidecar transport for `{}` is unsupported on this platform",
            endpoint.address
        ),
    );
    (BashTool::from_status(status.clone()), status)
}

fn sidecar_unix_socket_path(address: &str) -> Option<&str> {
    let address = address.trim();
    if address.is_empty() {
        None
    } else if let Some(path) = address.strip_prefix("unix://") {
        if path.is_empty() { None } else { Some(path) }
    } else if address.starts_with('/') {
        Some(address)
    } else {
        None
    }
}

fn unavailable_status(
    reason: SessionUnavailableReason,
    detail: impl Into<String>,
) -> SessionStatus {
    SessionStatus::Unavailable(SessionUnavailable {
        reason,
        detail: detail.into(),
    })
}

fn parse_args<T>(tool: &str, args: &str) -> Result<T, ToolError>
where
    T: DeserializeOwned,
{
    serde_json::from_str(args).map_err(|error| invalid_args(tool, error.to_string()))
}

fn invalid_args(tool: &str, message: impl Into<String>) -> ToolError {
    ToolError::InvalidArguments {
        tool: tool.to_owned(),
        message: message.into(),
    }
}

fn execution_failed(tool: &str, message: impl Into<String>) -> ToolError {
    ToolError::ExecutionFailed {
        tool: tool.to_owned(),
        message: message.into(),
    }
}

async fn execute_with_shell_session(
    session: &Arc<Mutex<Box<dyn ShellSession>>>,
    command: &str,
    timeout: Duration,
) -> Result<String, ToolError> {
    let timeout_secs = timeout.as_secs();
    let mut session = session.lock().await;
    session
        .exec_command(command, Some(timeout_secs))
        .await
        .map_err(|error| {
            execution_failed(
                BASH_TOOL_NAME,
                format!("failed to execute shell command via sidecar session: {error}"),
            )
        })?;

    let mut stdout = String::new();
    let mut stderr = String::new();
    loop {
        let chunk = session.stream_output(None).await.map_err(|error| {
            execution_failed(
                BASH_TOOL_NAME,
                format!("failed to stream shell output from sidecar session: {error}"),
            )
        })?;
        match chunk.stream {
            ShellOutputStream::Stdout => stdout.push_str(&chunk.data),
            ShellOutputStream::Stderr => stderr.push_str(&chunk.data),
        }
        if chunk.eof {
            break;
        }
    }

    let output = combine_command_output(stdout.as_bytes(), stderr.as_bytes());
    if output.is_empty() {
        Ok("command completed with no output".to_owned())
    } else {
        Ok(output)
    }
}

fn shell_disabled_message(status: &SessionStatus) -> String {
    match status {
        SessionStatus::Unavailable(unavailable) => {
            format!("shell tool is disabled: {}", unavailable.detail)
        }
        SessionStatus::Ready(_) => "shell tool is disabled".to_owned(),
    }
}

fn truncate_output(mut output: String, max_output_bytes: usize) -> String {
    if output.len() <= max_output_bytes {
        return output;
    }

    let original_bytes = output.len();
    let mut cutoff = max_output_bytes.min(original_bytes);
    while cutoff > 0 && !output.is_char_boundary(cutoff) {
        cutoff -= 1;
    }
    output.truncate(cutoff);
    output.push_str(&format!(
        "\n...[output truncated: {} bytes total]",
        original_bytes
    ));
    output
}

fn combine_command_output(stdout: &[u8], stderr: &[u8]) -> String {
    let stdout_text = String::from_utf8_lossy(stdout).trim_end().to_owned();
    let stderr_text = String::from_utf8_lossy(stderr).trim_end().to_owned();

    match (stdout_text.is_empty(), stderr_text.is_empty()) {
        (true, true) => String::new(),
        (false, true) => stdout_text,
        (true, false) => stderr_text,
        (false, false) => format!("{stdout_text}\n{stderr_text}"),
    }
}

#[cfg(target_os = "windows")]
fn run_shell_command(command: &str) -> std::io::Result<std::process::Output> {
    Command::new("cmd").args(["/C", command]).output()
}

#[cfg(not(target_os = "windows"))]
fn run_shell_command(command: &str) -> std::io::Result<std::process::Output> {
    Command::new("sh").args(["-lc", command]).output()
}

#[cfg(test)]
mod tests {
    use std::{
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
        JsonSchemaType, RunnerBootstrapEnvelope, SandboxTier, SidecarEndpoint, SidecarTransport,
    };

    use super::*;

    #[tool]
    /// Read UTF-8 text from a file
    async fn read_file(path: String) -> String {
        path
    }

    #[tokio::test]
    async fn read_tool_executes_through_tool_contract() {
        let path = temp_path("read");
        fs::write(&path, "hello from read tool").expect("test setup should create file");
        let tool = ReadTool;
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
        let tool = WriteTool;

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
        let tool = EditTool;

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
            .execute(BASH_TOOL_NAME, r#"{"command":"printf should-not-run"}"#)
            .await
            .expect_err("bash should be explicitly disabled when runner bootstrap is absent");
        assert!(matches!(
            error,
            ToolError::ExecutionFailed { tool, message }
                if tool == BASH_TOOL_NAME && message.contains("disabled")
        ));
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
        };
        let RuntimeToolsBootstrap {
            registry,
            availability,
        } = bootstrap_runtime_tools(Some(&bootstrap)).await;

        let output = registry
            .execute(BASH_TOOL_NAME, r#"{"command":"printf ws5-sidecar"}"#)
            .await
            .expect("sidecar-backed bash execution should succeed");
        assert_eq!(output, "ws5-sidecar");
        assert!(availability.shell.is_ready());
        assert!(availability.browser.is_ready());

        server_task.abort();
        let _ = fs::remove_file(socket_path);
    }

    #[test]
    fn core_tool_schemas_and_metadata_match_phase4_contract() {
        let read = ReadTool;
        let write = WriteTool;
        let edit = EditTool;
        let bash = BashTool::default();

        assert_eq!(read.safety_tier(), SafetyTier::ReadOnly);
        assert_eq!(write.safety_tier(), SafetyTier::SideEffecting);
        assert_eq!(edit.safety_tier(), SafetyTier::SideEffecting);
        assert_eq!(bash.safety_tier(), SafetyTier::Privileged);

        assert_eq!(read.schema().parameters.schema_type, JsonSchemaType::Object);
        assert_eq!(
            write.schema().parameters.schema_type,
            JsonSchemaType::Object
        );
        assert_eq!(edit.schema().parameters.schema_type, JsonSchemaType::Object);
        assert_eq!(bash.schema().parameters.schema_type, JsonSchemaType::Object);

        assert!(read.timeout().as_secs() > 0);
        assert!(write.timeout().as_secs() > 0);
        assert!(edit.timeout().as_secs() > 0);
        assert!(bash.timeout().as_secs() > 0);
    }

    #[test]
    fn macro_generated_schema_matches_read_tool_contract() {
        std::mem::drop(read_file("placeholder".to_owned()));
        let generated = __tool_function_decl_read_file();
        let runtime = ReadTool.schema();

        assert_eq!(generated.name, runtime.name);
        assert_eq!(generated.description, runtime.description);
        assert_eq!(
            generated.parameters.schema_type,
            runtime.parameters.schema_type
        );
        assert_eq!(generated.parameters.required, runtime.parameters.required);
        assert_eq!(
            generated.parameters.additional_properties,
            runtime.parameters.additional_properties
        );

        let generated_path = generated
            .parameters
            .properties
            .get("path")
            .expect("generated schema should expose path");
        let runtime_path = runtime
            .parameters
            .properties
            .get("path")
            .expect("runtime schema should expose path");
        assert_eq!(generated_path.schema_type, runtime_path.schema_type);
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
            .execute_with_policy(BASH_TOOL_NAME, r#"{"command":"echo blocked"}"#, |tier| {
                if tier == SafetyTier::Privileged {
                    Err(execution_failed("policy", "blocked by safety gate"))
                } else {
                    Ok(())
                }
            })
            .await;
        assert!(matches!(
            gated,
            Err(ToolError::ExecutionFailed { tool, message })
                if tool == "bash" && message.contains("unknown tool")
        ));
    }

    #[tokio::test]
    async fn registry_can_gate_registered_privileged_tools() {
        let mut registry = ToolRegistry::default();
        registry.register(BASH_TOOL_NAME, BashTool::default());

        let gated = registry
            .execute_with_policy(BASH_TOOL_NAME, r#"{"command":"echo blocked"}"#, |tier| {
                if tier == SafetyTier::Privileged {
                    Err(execution_failed("policy", "blocked by safety gate"))
                } else {
                    Ok(())
                }
            })
            .await;

        assert!(matches!(
            gated,
            Err(ToolError::ExecutionFailed { tool, message })
                if tool == "policy" && message == "blocked by safety gate"
        ));
    }

    fn temp_path(label: &str) -> PathBuf {
        let mut path = env::temp_dir();
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
                JsonSchema::object(BTreeMap::new(), vec![]),
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
                JsonSchema::object(BTreeMap::new(), vec![]),
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
}
