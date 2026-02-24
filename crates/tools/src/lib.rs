use std::{
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
#[cfg(feature = "wasm-isolation")]
use sandbox::WasmWasiToolRunner;
use sandbox::{
    SecurityPolicy, SessionStatus, SessionUnavailable, SessionUnavailableReason, ShellSession,
    ShellSessionConfig, VsockShellSession, WasmCapabilityProfile, WorkspaceSecurityPolicy,
};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
#[cfg(unix)]
use tokio::net::UnixStream;
use tokio::sync::Mutex;
use types::{
    FunctionDecl, RunnerBootstrapEnvelope, SafetyTier, SandboxTier, ShellConfig, ShellOutputStream,
    SidecarEndpoint, SidecarTransport, StartupDegradedReasonCode, StartupStatusReport, Tool,
    ToolError, ToolExecutionContext,
};

mod registry;
pub mod sandbox;

pub mod memory_tools;
pub mod scheduler_tools;

#[cfg(test)]
mod tests;

pub use memory_tools::{
    MEMORY_DELETE_TOOL_NAME, MEMORY_SAVE_TOOL_NAME, MEMORY_SEARCH_TOOL_NAME,
    MEMORY_UPDATE_TOOL_NAME, register_memory_tools,
};
pub use registry::{
    RuntimeToolsBootstrap, ToolAvailability, ToolRegistry, bootstrap_runtime_tools,
    default_registry,
};
pub use sandbox::{
    HostWasmToolRunner, ProcessHardeningOutcome, WasmToolRunner, WasmWorkspaceMounts,
    attempt_process_tier_hardening,
};
pub use scheduler_tools::{
    SCHEDULE_CREATE_TOOL_NAME, SCHEDULE_DELETE_TOOL_NAME, SCHEDULE_EDIT_TOOL_NAME,
    SCHEDULE_SEARCH_TOOL_NAME, register_scheduler_tools,
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
    count: Option<u64>,
    freshness: Option<String>,
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
        FunctionDecl::new(
            FILE_READ_TOOL_NAME,
            Some("Read the contents of a file and return its text. Paths must be within /shared, /tmp, or /vault.".to_owned()),
            json!({
                "type": "object",
                "required": ["path"],
                "properties": {
                    "path": { "type": "string", "description": "File path to read (e.g. /shared/notes.txt, /vault/data.csv)" }
                }
            }),
        )
    }

    async fn execute(
        &self,
        args: &str,
        _context: &ToolExecutionContext,
    ) -> Result<String, ToolError> {
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
        FunctionDecl::new(
            FILE_SEARCH_TOOL_NAME,
            Some("Search recursively for lines matching a text query. Returns matching lines with file paths and line numbers. Paths must be within /shared, /tmp, or /vault.".to_owned()),
            json!({
                "type": "object",
                "required": ["path", "query"],
                "properties": {
                    "path":  { "type": "string", "description": "Root directory to search (e.g. /shared)" },
                    "query": { "type": "string", "description": "Search query" }
                }
            }),
        )
    }

    async fn execute(
        &self,
        args: &str,
        _context: &ToolExecutionContext,
    ) -> Result<String, ToolError> {
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
        FunctionDecl::new(
            FILE_LIST_TOOL_NAME,
            Some("List files and directories within /shared, /tmp, or /vault. Returns names with type indicators.".to_owned()),
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Directory path to list (e.g. /shared, /tmp); defaults to /shared" }
                }
            }),
        )
    }

    async fn execute(
        &self,
        args: &str,
        _context: &ToolExecutionContext,
    ) -> Result<String, ToolError> {
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
        FunctionDecl::new(
            FILE_WRITE_TOOL_NAME,
            Some("Create or overwrite a file with the given content. Creates parent directories as needed. Paths must be within /shared or /tmp.".to_owned()),
            json!({
                "type": "object",
                "required": ["path", "content"],
                "properties": {
                    "path":    { "type": "string", "description": "File path to write (e.g. /shared/output.txt)" },
                    "content": { "type": "string", "description": "UTF-8 file content to persist" }
                }
            }),
        )
    }

    async fn execute(
        &self,
        args: &str,
        _context: &ToolExecutionContext,
    ) -> Result<String, ToolError> {
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
        FunctionDecl::new(
            FILE_EDIT_TOOL_NAME,
            Some("Edit a file by replacing an exact text snippet. old_text must match exactly one occurrence in the file. Paths must be within /shared or /tmp. Use file_read first to see the current contents.".to_owned()),
            json!({
                "type": "object",
                "required": ["path", "old_text", "new_text"],
                "properties": {
                    "path":     { "type": "string", "description": "File path to edit (e.g. /shared/config.txt)" },
                    "old_text": { "type": "string", "description": "Exact text snippet to replace" },
                    "new_text": { "type": "string", "description": "Replacement text" }
                }
            }),
        )
    }

    async fn execute(
        &self,
        args: &str,
        _context: &ToolExecutionContext,
    ) -> Result<String, ToolError> {
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
        FunctionDecl::new(
            FILE_DELETE_TOOL_NAME,
            Some("Delete a file or directory. Directories are removed recursively. Paths must be within /shared or /tmp.".to_owned()),
            json!({
                "type": "object",
                "required": ["path"],
                "properties": {
                    "path": { "type": "string", "description": "File or directory path to delete (e.g. /shared/old-file.txt)" }
                }
            }),
        )
    }

    async fn execute(
        &self,
        args: &str,
        _context: &ToolExecutionContext,
    ) -> Result<String, ToolError> {
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
        FunctionDecl::new(
            WEB_FETCH_TOOL_NAME,
            Some("Fetch a URL over HTTP and return its content. Returns status, content type, and body. HTML is automatically converted to readable text. Binary content returns metadata only.".to_owned()),
            json!({
                "type": "object",
                "required": ["url"],
                "properties": {
                    "url": { "type": "string", "description": "URL to fetch", "minLength": 1 }
                }
            }),
        )
    }

    async fn execute(
        &self,
        args: &str,
        _context: &ToolExecutionContext,
    ) -> Result<String, ToolError> {
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
        FunctionDecl::new(
            WEB_SEARCH_TOOL_NAME,
            Some("Search the web and return a list of results with title, url, and snippet. Use web_fetch to retrieve full page content when needed.".to_owned()),
            json!({
                "type": "object",
                "required": ["query"],
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search query",
                        "minLength": 1
                    },
                    "count": {
                        "type": "integer",
                        "description": "Optional result count (default 5)",
                        "minimum": 1,
                        "maximum": 10,
                        "default": 5
                    },
                    "freshness": {
                        "type": "string",
                        "description": "Optional freshness filter",
                        "enum": ["day", "week", "month", "year"]
                    }
                }
            }),
        )
    }

    async fn execute(
        &self,
        args: &str,
        _context: &ToolExecutionContext,
    ) -> Result<String, ToolError> {
        let request: WebSearchArgs = parse_args(WEB_SEARCH_TOOL_NAME, args)?;
        let mut payload = serde_json::Map::new();
        payload.insert("query".to_owned(), json!(request.query));
        if let Some(count) = request.count {
            payload.insert("count".to_owned(), json!(count));
        }
        if let Some(freshness) = request.freshness {
            payload.insert("freshness".to_owned(), json!(freshness));
        }
        invoke_wasm_tool(
            &self.runner,
            WEB_SEARCH_TOOL_NAME,
            WasmCapabilityProfile::Web,
            &serde_json::Value::Object(payload),
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
        FunctionDecl::new(
            VAULT_COPYTO_TOOL_NAME,
            Some("Copy a file from the read-only /vault into /shared or /tmp so it can be read or modified.".to_owned()),
            json!({
                "type": "object",
                "required": ["source_path", "destination_path"],
                "properties": {
                    "source_path":      { "type": "string", "description": "Source path within /vault (e.g. /vault/data.csv)" },
                    "destination_path": { "type": "string", "description": "Destination path in /shared or /tmp (e.g. /shared/data.csv)" }
                }
            }),
        )
    }

    async fn execute(
        &self,
        args: &str,
        _context: &ToolExecutionContext,
    ) -> Result<String, ToolError> {
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
        FunctionDecl::new(
            SHELL_EXEC_TOOL_NAME,
            Some("Execute a shell command and return its output. Returns combined stdout and stderr. The working directory is the workspace root; files are in /shared, /tmp, and /vault.".to_owned()),
            json!({
                "type": "object",
                "required": ["command"],
                "properties": {
                    "command": { "type": "string", "description": "Shell command to execute (e.g. 'ls /shared' or 'cat /shared/notes.txt')" }
                }
            }),
        )
    }

    async fn execute(
        &self,
        args: &str,
        _context: &ToolExecutionContext,
    ) -> Result<String, ToolError> {
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

fn default_wasm_runner() -> Arc<dyn WasmToolRunner> {
    let workspace_root = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    #[cfg(feature = "wasm-isolation")]
    {
        match WasmWasiToolRunner::for_bootstrap_workspace(&workspace_root) {
            Ok(runner) => return Arc::new(runner),
            Err(e) => {
                tracing::warn!(
                    "failed to initialize WasmWasiToolRunner, falling back to host runner: {e}"
                );
            }
        }
    }
    Arc::new(HostWasmToolRunner::for_bootstrap_workspace(workspace_root))
}

fn runtime_wasm_runner(bootstrap: Option<&RunnerBootstrapEnvelope>) -> Arc<dyn WasmToolRunner> {
    match bootstrap {
        Some(bootstrap) => {
            let mounts = if let Some(runtime_policy) = bootstrap.runtime_policy.as_ref() {
                WasmWorkspaceMounts::from_mount_roots(
                    &runtime_policy.mounts.shared,
                    &runtime_policy.mounts.tmp,
                    &runtime_policy.mounts.vault,
                )
            } else {
                WasmWorkspaceMounts::for_bootstrap_workspace(&bootstrap.workspace_root)
            };
            #[cfg(feature = "wasm-isolation")]
            {
                match WasmWasiToolRunner::new(mounts.clone()) {
                    Ok(runner) => return Arc::new(runner),
                    Err(e) => {
                        tracing::warn!(
                            "failed to initialize WasmWasiToolRunner, falling back to host runner: {e}"
                        );
                    }
                }
            }
            Arc::new(HostWasmToolRunner::new(mounts))
        }
        None => default_wasm_runner(),
    }
}

fn workspace_security_policy(
    bootstrap: Option<&RunnerBootstrapEnvelope>,
    shell_config: Option<&ShellConfig>,
) -> WorkspaceSecurityPolicy {
    let policy = match bootstrap {
        Some(bootstrap) => {
            if let Some(runtime_policy) = bootstrap.runtime_policy.as_ref() {
                WorkspaceSecurityPolicy::for_mount_roots(
                    &runtime_policy.mounts.shared,
                    &runtime_policy.mounts.tmp,
                    &runtime_policy.mounts.vault,
                    Vec::new(),
                )
            } else {
                WorkspaceSecurityPolicy::for_bootstrap_workspace(&bootstrap.workspace_root)
            }
        }
        None => {
            let workspace_root = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            WorkspaceSecurityPolicy::for_bootstrap_workspace(workspace_root)
        }
    };
    policy.with_shell_config(shell_config)
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
