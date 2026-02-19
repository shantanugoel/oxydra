use std::{collections::BTreeMap, fs, process::Command, time::Duration};

use async_trait::async_trait;
use serde::{Deserialize, de::DeserializeOwned};
use types::{FunctionDecl, JsonSchema, JsonSchemaType, SafetyTier, Tool, ToolError};

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

#[derive(Debug, Default, Clone, Copy)]
pub struct BashTool;

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
                .with_description("Shell command executed with the host shell"),
        );

        FunctionDecl::new(
            BASH_TOOL_NAME,
            Some("Execute a shell command".to_owned()),
            JsonSchema::object(properties, vec!["command".to_owned()]),
        )
    }

    async fn execute(&self, args: &str) -> Result<String, ToolError> {
        let request: BashArgs = parse_args(BASH_TOOL_NAME, args)?;
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
        self.register(BASH_TOOL_NAME, BashTool);
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
    use tokio::time::sleep;
    use types::JsonSchemaType;

    use super::*;

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

        let output = BashTool
            .execute(&json!({ "command": command }).to_string())
            .await
            .expect("bash command should succeed");
        assert!(output.contains("hello"));
    }

    #[test]
    fn core_tool_schemas_and_metadata_match_phase4_contract() {
        let read = ReadTool;
        let write = WriteTool;
        let edit = EditTool;
        let bash = BashTool;

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
        registry.register(BASH_TOOL_NAME, BashTool);

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
