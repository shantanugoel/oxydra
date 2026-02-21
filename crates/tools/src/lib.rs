use std::{
    collections::BTreeMap,
    env,
    path::PathBuf,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use sandbox::{
    HostWasmToolRunner, SecurityPolicy, SessionStatus, SessionUnavailable,
    SessionUnavailableReason, ShellSession, ShellSessionConfig, VsockShellSession,
    WasmCapabilityProfile, WasmToolRunner, WorkspaceSecurityPolicy,
};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
#[cfg(unix)]
use tokio::net::UnixStream;
use tokio::sync::Mutex;
use types::{
    FunctionDecl, JsonSchema, JsonSchemaType, RunnerBootstrapEnvelope, SafetyTier, SandboxTier,
    ShellOutputStream, SidecarEndpoint, SidecarTransport, Tool, ToolError,
};

pub const FILE_READ_TOOL_NAME: &str = "file_read";
pub const FILE_SEARCH_TOOL_NAME: &str = "file_search";
pub const FILE_LIST_TOOL_NAME: &str = "file_list";
pub const FILE_WRITE_TOOL_NAME: &str = "file_write";
pub const FILE_EDIT_TOOL_NAME: &str = "file_edit";
pub const FILE_DELETE_TOOL_NAME: &str = "file_delete";
pub const WEB_FETCH_TOOL_NAME: &str = "web_fetch";
pub const WEB_SEARCH_TOOL_NAME: &str = "web_search";
pub const VAULT_COPYTO_TOOL_NAME: &str = "vault_copyto";
pub const SHELL_EXEC_TOOL_NAME: &str = "shell_exec";
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 16 * 1024;

const VAULT_COPYTO_READ_OPERATION: &str = "vault_copyto_read";
const VAULT_COPYTO_WRITE_OPERATION: &str = "vault_copyto_write";
static NEXT_VAULT_COPYTO_OPERATION_NUMBER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
pub struct ReadTool {
    runner: Arc<dyn WasmToolRunner>,
}

#[derive(Clone)]
pub struct SearchTool {
    runner: Arc<dyn WasmToolRunner>,
}

#[derive(Clone)]
pub struct ListTool {
    runner: Arc<dyn WasmToolRunner>,
}

#[derive(Clone)]
pub struct WriteTool {
    runner: Arc<dyn WasmToolRunner>,
}

#[derive(Clone)]
pub struct EditTool {
    runner: Arc<dyn WasmToolRunner>,
}

#[derive(Clone)]
pub struct DeleteTool {
    runner: Arc<dyn WasmToolRunner>,
}

#[derive(Clone)]
pub struct WebFetchTool {
    runner: Arc<dyn WasmToolRunner>,
}

#[derive(Clone)]
pub struct WebSearchTool {
    runner: Arc<dyn WasmToolRunner>,
}

#[derive(Clone)]
pub struct VaultCopyToTool {
    runner: Arc<dyn WasmToolRunner>,
}

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
struct SearchArgs {
    path: String,
    query: String,
}

#[derive(Debug, Deserialize)]
struct ListArgs {
    path: Option<String>,
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
struct DeleteArgs {
    path: String,
}

#[derive(Debug, Deserialize)]
struct WebFetchArgs {
    url: String,
}

#[derive(Debug, Deserialize)]
struct WebSearchArgs {
    query: String,
}

#[derive(Debug, Deserialize)]
struct VaultCopyToArgs {
    source_path: String,
    destination_path: String,
}

#[derive(Debug, Deserialize)]
struct BashArgs {
    command: String,
}

impl ReadTool {
    pub fn new(runner: Arc<dyn WasmToolRunner>) -> Self {
        Self { runner }
    }
}

impl Default for ReadTool {
    fn default() -> Self {
        Self::new(default_wasm_runner())
    }
}

impl SearchTool {
    pub fn new(runner: Arc<dyn WasmToolRunner>) -> Self {
        Self { runner }
    }
}

impl Default for SearchTool {
    fn default() -> Self {
        Self::new(default_wasm_runner())
    }
}

impl ListTool {
    pub fn new(runner: Arc<dyn WasmToolRunner>) -> Self {
        Self { runner }
    }
}

impl Default for ListTool {
    fn default() -> Self {
        Self::new(default_wasm_runner())
    }
}

impl WriteTool {
    pub fn new(runner: Arc<dyn WasmToolRunner>) -> Self {
        Self { runner }
    }
}

impl Default for WriteTool {
    fn default() -> Self {
        Self::new(default_wasm_runner())
    }
}

impl EditTool {
    pub fn new(runner: Arc<dyn WasmToolRunner>) -> Self {
        Self { runner }
    }
}

impl Default for EditTool {
    fn default() -> Self {
        Self::new(default_wasm_runner())
    }
}

impl DeleteTool {
    pub fn new(runner: Arc<dyn WasmToolRunner>) -> Self {
        Self { runner }
    }
}

impl Default for DeleteTool {
    fn default() -> Self {
        Self::new(default_wasm_runner())
    }
}

impl WebFetchTool {
    pub fn new(runner: Arc<dyn WasmToolRunner>) -> Self {
        Self { runner }
    }
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self::new(default_wasm_runner())
    }
}

impl WebSearchTool {
    pub fn new(runner: Arc<dyn WasmToolRunner>) -> Self {
        Self { runner }
    }
}

impl Default for WebSearchTool {
    fn default() -> Self {
        Self::new(default_wasm_runner())
    }
}

impl VaultCopyToTool {
    pub fn new(runner: Arc<dyn WasmToolRunner>) -> Self {
        Self { runner }
    }

    fn next_operation_id() -> String {
        let operation_number =
            NEXT_VAULT_COPYTO_OPERATION_NUMBER.fetch_add(1, Ordering::Relaxed) + 1;
        format!("vault-copyto-{operation_number}")
    }
}

impl Default for VaultCopyToTool {
    fn default() -> Self {
        Self::new(default_wasm_runner())
    }
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
            FILE_READ_TOOL_NAME,
            Some("Read UTF-8 text from a file".to_owned()),
            JsonSchema::object(properties, vec!["path".to_owned()]),
        )
    }

    async fn execute(&self, args: &str) -> Result<String, ToolError> {
        let request: ReadArgs = parse_args(FILE_READ_TOOL_NAME, args)?;
        invoke_wasm_tool(
            &self.runner,
            FILE_READ_TOOL_NAME,
            WasmCapabilityProfile::FileReadOnly,
            &json!({ "path": request.path }),
            None,
        )
        .await
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(10)
    }

    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::ReadOnly
    }
}

#[async_trait]
impl Tool for SearchTool {
    fn schema(&self) -> FunctionDecl {
        let mut properties = BTreeMap::new();
        properties.insert(
            "path".to_owned(),
            JsonSchema::new(JsonSchemaType::String).with_description("Root path to search"),
        );
        properties.insert(
            "query".to_owned(),
            JsonSchema::new(JsonSchemaType::String).with_description("Search query"),
        );

        FunctionDecl::new(
            FILE_SEARCH_TOOL_NAME,
            Some("Search recursively for lines containing the query text".to_owned()),
            JsonSchema::object(properties, vec!["path".to_owned(), "query".to_owned()]),
        )
    }

    async fn execute(&self, args: &str) -> Result<String, ToolError> {
        let request: SearchArgs = parse_args(FILE_SEARCH_TOOL_NAME, args)?;
        invoke_wasm_tool(
            &self.runner,
            FILE_SEARCH_TOOL_NAME,
            WasmCapabilityProfile::FileReadOnly,
            &json!({ "path": request.path, "query": request.query }),
            None,
        )
        .await
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(20)
    }

    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::ReadOnly
    }
}

#[async_trait]
impl Tool for ListTool {
    fn schema(&self) -> FunctionDecl {
        let mut properties = BTreeMap::new();
        properties.insert(
            "path".to_owned(),
            JsonSchema::new(JsonSchemaType::String)
                .with_description("Directory path to list; defaults to current directory"),
        );

        FunctionDecl::new(
            FILE_LIST_TOOL_NAME,
            Some("List entries in a directory".to_owned()),
            JsonSchema::object(properties, Vec::new()),
        )
    }

    async fn execute(&self, args: &str) -> Result<String, ToolError> {
        let request: ListArgs = parse_args(FILE_LIST_TOOL_NAME, args)?;
        let payload = if let Some(path) = request.path {
            json!({ "path": path })
        } else {
            json!({})
        };
        invoke_wasm_tool(
            &self.runner,
            FILE_LIST_TOOL_NAME,
            WasmCapabilityProfile::FileReadOnly,
            &payload,
            None,
        )
        .await
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
            FILE_WRITE_TOOL_NAME,
            Some("Write UTF-8 text to a file".to_owned()),
            JsonSchema::object(properties, vec!["path".to_owned(), "content".to_owned()]),
        )
    }

    async fn execute(&self, args: &str) -> Result<String, ToolError> {
        let request: WriteArgs = parse_args(FILE_WRITE_TOOL_NAME, args)?;
        invoke_wasm_tool(
            &self.runner,
            FILE_WRITE_TOOL_NAME,
            WasmCapabilityProfile::FileReadWrite,
            &json!({ "path": request.path, "content": request.content }),
            None,
        )
        .await
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
            FILE_EDIT_TOOL_NAME,
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
        let request: EditArgs = parse_args(FILE_EDIT_TOOL_NAME, args)?;
        invoke_wasm_tool(
            &self.runner,
            FILE_EDIT_TOOL_NAME,
            WasmCapabilityProfile::FileReadWrite,
            &json!({
                "path": request.path,
                "old_text": request.old_text,
                "new_text": request.new_text
            }),
            None,
        )
        .await
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(15)
    }

    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::SideEffecting
    }
}

#[async_trait]
impl Tool for DeleteTool {
    fn schema(&self) -> FunctionDecl {
        let mut properties = BTreeMap::new();
        properties.insert(
            "path".to_owned(),
            JsonSchema::new(JsonSchemaType::String)
                .with_description("File or directory path to delete"),
        );

        FunctionDecl::new(
            FILE_DELETE_TOOL_NAME,
            Some("Delete a file or directory".to_owned()),
            JsonSchema::object(properties, vec!["path".to_owned()]),
        )
    }

    async fn execute(&self, args: &str) -> Result<String, ToolError> {
        let request: DeleteArgs = parse_args(FILE_DELETE_TOOL_NAME, args)?;
        invoke_wasm_tool(
            &self.runner,
            FILE_DELETE_TOOL_NAME,
            WasmCapabilityProfile::FileReadWrite,
            &json!({ "path": request.path }),
            None,
        )
        .await
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(15)
    }

    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::SideEffecting
    }
}

#[async_trait]
impl Tool for WebFetchTool {
    fn schema(&self) -> FunctionDecl {
        let mut properties = BTreeMap::new();
        properties.insert(
            "url".to_owned(),
            JsonSchema::new(JsonSchemaType::String).with_description("URL to fetch"),
        );

        FunctionDecl::new(
            WEB_FETCH_TOOL_NAME,
            Some("Fetch a URL and return response text".to_owned()),
            JsonSchema::object(properties, vec!["url".to_owned()]),
        )
    }

    async fn execute(&self, args: &str) -> Result<String, ToolError> {
        let request: WebFetchArgs = parse_args(WEB_FETCH_TOOL_NAME, args)?;
        invoke_wasm_tool(
            &self.runner,
            WEB_FETCH_TOOL_NAME,
            WasmCapabilityProfile::Web,
            &json!({ "url": request.url }),
            None,
        )
        .await
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(30)
    }

    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::SideEffecting
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn schema(&self) -> FunctionDecl {
        let mut properties = BTreeMap::new();
        properties.insert(
            "query".to_owned(),
            JsonSchema::new(JsonSchemaType::String).with_description("Search query"),
        );

        FunctionDecl::new(
            WEB_SEARCH_TOOL_NAME,
            Some("Run a web search and return response text".to_owned()),
            JsonSchema::object(properties, vec!["query".to_owned()]),
        )
    }

    async fn execute(&self, args: &str) -> Result<String, ToolError> {
        let request: WebSearchArgs = parse_args(WEB_SEARCH_TOOL_NAME, args)?;
        invoke_wasm_tool(
            &self.runner,
            WEB_SEARCH_TOOL_NAME,
            WasmCapabilityProfile::Web,
            &json!({ "query": request.query }),
            None,
        )
        .await
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(30)
    }

    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::SideEffecting
    }
}

#[async_trait]
impl Tool for VaultCopyToTool {
    fn schema(&self) -> FunctionDecl {
        let mut properties = BTreeMap::new();
        properties.insert(
            "source_path".to_owned(),
            JsonSchema::new(JsonSchemaType::String)
                .with_description("Vault source path to read from"),
        );
        properties.insert(
            "destination_path".to_owned(),
            JsonSchema::new(JsonSchemaType::String)
                .with_description("Destination path in shared/tmp to write to"),
        );

        FunctionDecl::new(
            VAULT_COPYTO_TOOL_NAME,
            Some("Copy data from vault into shared/tmp via two-step WASM invocation".to_owned()),
            JsonSchema::object(
                properties,
                vec!["source_path".to_owned(), "destination_path".to_owned()],
            ),
        )
    }

    async fn execute(&self, args: &str) -> Result<String, ToolError> {
        let request: VaultCopyToArgs = parse_args(VAULT_COPYTO_TOOL_NAME, args)?;
        let operation_id = Self::next_operation_id();
        let read_result = invoke_wasm_tool(
            &self.runner,
            VAULT_COPYTO_READ_OPERATION,
            WasmCapabilityProfile::VaultReadStep,
            &json!({ "source_path": request.source_path }),
            Some(operation_id.as_str()),
        )
        .await?;

        let write_result = invoke_wasm_tool(
            &self.runner,
            VAULT_COPYTO_WRITE_OPERATION,
            WasmCapabilityProfile::VaultWriteStep,
            &json!({
                "destination_path": request.destination_path,
                "content": read_result
            }),
            Some(operation_id.as_str()),
        )
        .await?;
        Ok(write_result)
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(30)
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
            SHELL_EXEC_TOOL_NAME,
            Some("Execute a shell command".to_owned()),
            JsonSchema::object(properties, vec!["command".to_owned()]),
        )
    }

    async fn execute(&self, args: &str) -> Result<String, ToolError> {
        let request: BashArgs = parse_args(SHELL_EXEC_TOOL_NAME, args)?;
        match &self.backend {
            BashBackend::Host => {
                let output = run_shell_command(&request.command)
                    .map_err(|error| execution_failed(SHELL_EXEC_TOOL_NAME, error.to_string()))?;
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
                    Err(execution_failed(SHELL_EXEC_TOOL_NAME, message))
                }
            }
            BashBackend::Session(session) => {
                execute_with_shell_session(session, &request.command, self.timeout()).await
            }
            BashBackend::Disabled(status) => Err(execution_failed(
                SHELL_EXEC_TOOL_NAME,
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
    security_policy: Option<Arc<dyn SecurityPolicy>>,
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
            security_policy: None,
        }
    }

    pub fn register<T>(&mut self, name: impl Into<String>, tool: T)
    where
        T: Tool + 'static,
    {
        self.tools.insert(name.into(), Box::new(tool));
    }

    pub fn register_core_tools(&mut self) {
        let wasm_runner = default_wasm_runner();
        register_runtime_tools(self, wasm_runner, BashTool::default());
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(Box::as_ref)
    }

    pub fn schemas(&self) -> Vec<FunctionDecl> {
        self.tools.values().map(|tool| tool.schema()).collect()
    }

    pub fn set_security_policy(&mut self, policy: Arc<dyn SecurityPolicy>) {
        self.security_policy = Some(policy);
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
        if let Some(policy) = &self.security_policy {
            let arguments = parse_policy_args(name, args)?;
            policy
                .enforce(name, tool.safety_tier(), &arguments)
                .map_err(|violation| {
                    execution_failed(
                        name,
                        format!(
                            "blocked by security policy ({:?}): {}",
                            violation.reason, violation.detail
                        ),
                    )
                })?;
        }

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
    let wasm_runner = runtime_wasm_runner(bootstrap);
    let mut registry = ToolRegistry::default();
    register_runtime_tools(&mut registry, wasm_runner, bash_tool);
    registry.set_security_policy(Arc::new(workspace_security_policy(bootstrap)));

    RuntimeToolsBootstrap {
        registry,
        availability: ToolAvailability {
            shell: shell_status,
            browser: browser_status,
        },
    }
}

fn register_runtime_tools(
    registry: &mut ToolRegistry,
    wasm_runner: Arc<dyn WasmToolRunner>,
    shell_tool: BashTool,
) {
    registry.register(FILE_READ_TOOL_NAME, ReadTool::new(wasm_runner.clone()));
    registry.register(FILE_SEARCH_TOOL_NAME, SearchTool::new(wasm_runner.clone()));
    registry.register(FILE_LIST_TOOL_NAME, ListTool::new(wasm_runner.clone()));
    registry.register(FILE_WRITE_TOOL_NAME, WriteTool::new(wasm_runner.clone()));
    registry.register(FILE_EDIT_TOOL_NAME, EditTool::new(wasm_runner.clone()));
    registry.register(FILE_DELETE_TOOL_NAME, DeleteTool::new(wasm_runner.clone()));
    registry.register(WEB_FETCH_TOOL_NAME, WebFetchTool::new(wasm_runner.clone()));
    registry.register(
        WEB_SEARCH_TOOL_NAME,
        WebSearchTool::new(wasm_runner.clone()),
    );
    registry.register(VAULT_COPYTO_TOOL_NAME, VaultCopyToTool::new(wasm_runner));
    registry.register(SHELL_EXEC_TOOL_NAME, shell_tool);
}

fn default_wasm_runner() -> Arc<dyn WasmToolRunner> {
    let workspace_root = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    Arc::new(HostWasmToolRunner::for_direct_workspace(workspace_root))
}

fn runtime_wasm_runner(bootstrap: Option<&RunnerBootstrapEnvelope>) -> Arc<dyn WasmToolRunner> {
    match bootstrap {
        Some(bootstrap) => Arc::new(HostWasmToolRunner::for_bootstrap_workspace(
            &bootstrap.workspace_root,
        )),
        None => default_wasm_runner(),
    }
}

fn workspace_security_policy(
    bootstrap: Option<&RunnerBootstrapEnvelope>,
) -> WorkspaceSecurityPolicy {
    match bootstrap {
        Some(bootstrap) => {
            WorkspaceSecurityPolicy::for_bootstrap_workspace(&bootstrap.workspace_root)
        }
        None => {
            let workspace_root = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            WorkspaceSecurityPolicy::for_direct_workspace(workspace_root)
        }
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

fn parse_policy_args(tool: &str, args: &str) -> Result<Value, ToolError> {
    serde_json::from_str(args)
        .map_err(|error| invalid_args(tool, format!("invalid JSON arguments payload: {error}")))
}

async fn invoke_wasm_tool(
    runner: &Arc<dyn WasmToolRunner>,
    tool_name: &str,
    profile: WasmCapabilityProfile,
    arguments: &Value,
    operation_id: Option<&str>,
) -> Result<String, ToolError> {
    runner
        .invoke(tool_name, profile, arguments, operation_id)
        .await
        .map(|result| result.output)
        .map_err(|error| execution_failed(tool_name, error.to_string()))
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
                SHELL_EXEC_TOOL_NAME,
                format!("failed to execute shell command via sidecar session: {error}"),
            )
        })?;

    let mut stdout = String::new();
    let mut stderr = String::new();
    loop {
        let chunk = session.stream_output(None).await.map_err(|error| {
            execution_failed(
                SHELL_EXEC_TOOL_NAME,
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
        collections::BTreeSet,
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
        std::mem::drop(file_read("placeholder".to_owned()));
        let generated = __tool_function_decl_file_read();
        let runtime = ReadTool::default().schema();

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
        };
        let RuntimeToolsBootstrap { registry, .. } =
            bootstrap_runtime_tools(Some(&bootstrap)).await;
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
