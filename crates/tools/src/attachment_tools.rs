use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use base64::Engine;
use serde::Deserialize;
use serde_json::json;
use types::{FunctionDecl, SafetyTier, Tool, ToolError, ToolExecutionContext};

use crate::{
    WasmCapabilityProfile, WasmToolRunner, default_wasm_runner, execution_failed, parse_args,
};

pub const ATTACHMENT_SAVE_TOOL_NAME: &str = "attachment_save";
const FILE_WRITE_BYTES_OPERATION: &str = "file_write_bytes";

#[derive(Debug, Deserialize)]
struct AttachmentSaveArgs {
    index: usize,
    path: String,
    #[serde(default)]
    overwrite: bool,
}

#[derive(Clone)]
pub struct AttachmentSaveTool {
    runner: Arc<dyn WasmToolRunner>,
}

impl AttachmentSaveTool {
    pub fn new(runner: Arc<dyn WasmToolRunner>) -> Self {
        Self { runner }
    }
}

impl Default for AttachmentSaveTool {
    fn default() -> Self {
        Self::new(default_wasm_runner())
    }
}

#[async_trait]
impl Tool for AttachmentSaveTool {
    fn schema(&self) -> FunctionDecl {
        FunctionDecl::new(
            ATTACHMENT_SAVE_TOOL_NAME,
            Some(
                "Save one inbound attachment from the current user turn to a workspace file. \
                 Use index to choose which attachment to save. Paths must be in /shared or /tmp."
                    .to_owned(),
            ),
            json!({
                "type": "object",
                "required": ["index", "path"],
                "properties": {
                    "index": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Zero-based attachment index from the current user turn"
                    },
                    "path": {
                        "type": "string",
                        "description": "Destination file path in /shared or /tmp"
                    },
                    "overwrite": {
                        "type": "boolean",
                        "default": false,
                        "description": "Set true to replace an existing file"
                    }
                }
            }),
        )
    }

    async fn execute(
        &self,
        args: &str,
        context: &ToolExecutionContext,
    ) -> Result<String, ToolError> {
        let request: AttachmentSaveArgs = parse_args(ATTACHMENT_SAVE_TOOL_NAME, args)?;
        let attachments = context.inbound_attachments.as_ref().ok_or_else(|| {
            execution_failed(
                ATTACHMENT_SAVE_TOOL_NAME,
                "no inbound attachments are available in the current turn",
            )
        })?;
        if attachments.is_empty() {
            return Err(execution_failed(
                ATTACHMENT_SAVE_TOOL_NAME,
                "no inbound attachments are available in the current turn",
            ));
        }
        let attachment = attachments.get(request.index).ok_or_else(|| {
            execution_failed(
                ATTACHMENT_SAVE_TOOL_NAME,
                format!(
                    "attachment index {} is out of bounds ({} attachment(s) available)",
                    request.index,
                    attachments.len()
                ),
            )
        })?;

        let encoded = base64::engine::general_purpose::STANDARD.encode(attachment.data.as_slice());
        let result = self
            .runner
            .invoke(
                FILE_WRITE_BYTES_OPERATION,
                WasmCapabilityProfile::FileReadWrite,
                &json!({
                    "path": request.path,
                    "content_base64": encoded,
                    "overwrite": request.overwrite
                }),
                None,
            )
            .await
            .map_err(|error| {
                execution_failed(
                    ATTACHMENT_SAVE_TOOL_NAME,
                    format!("failed to save attachment: {error}"),
                )
            })?;

        Ok(json!({
            "saved": true,
            "path": request.path,
            "mime_type": attachment.mime_type,
            "bytes_written": attachment.data.len(),
            "source_index": request.index,
            "result": result.output,
        })
        .to_string())
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(20)
    }

    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::SideEffecting
    }
}

pub fn register_attachment_tools(
    registry: &mut crate::ToolRegistry,
    runner: Arc<dyn WasmToolRunner>,
) {
    registry.register(ATTACHMENT_SAVE_TOOL_NAME, AttachmentSaveTool::new(runner));
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs,
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    use serde_json::Value;
    use types::InlineMedia;

    use crate::HostWasmToolRunner;

    use super::*;

    fn temp_workspace_root(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be monotonic for test unique ids")
            .as_nanos();
        let root = env::temp_dir().join(format!(
            "oxydra-tools-attachment-save-{label}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(root.join("shared")).expect("shared directory should be created");
        fs::create_dir_all(root.join("tmp")).expect("tmp directory should be created");
        fs::create_dir_all(root.join("vault")).expect("vault directory should be created");
        root
    }

    fn test_context(attachments: Option<Vec<InlineMedia>>) -> ToolExecutionContext {
        ToolExecutionContext {
            inbound_attachments: attachments.map(Arc::new),
            ..Default::default()
        }
    }

    fn inline_attachment(mime_type: &str, data: &[u8]) -> InlineMedia {
        InlineMedia {
            mime_type: mime_type.to_owned(),
            data: data.to_vec(),
        }
    }

    fn shared_path(workspace: &Path, file_name: &str) -> String {
        workspace
            .join("shared")
            .join(file_name)
            .to_string_lossy()
            .to_string()
    }

    #[test]
    fn attachment_save_schema_has_required_fields() {
        let tool = AttachmentSaveTool::default();
        let schema = tool.schema();
        let required = schema.parameters["required"]
            .as_array()
            .expect("required should be an array");
        assert!(required.iter().any(|value| value == "index"));
        assert!(required.iter().any(|value| value == "path"));
    }

    #[tokio::test]
    async fn attachment_save_rejects_when_no_inbound_attachments_exist() {
        let workspace = temp_workspace_root("no-inbound");
        let runner = Arc::new(HostWasmToolRunner::for_bootstrap_workspace(&workspace));
        let tool = AttachmentSaveTool::new(runner);
        let context = test_context(None);
        let path = shared_path(&workspace, "unused.bin");

        let error = tool
            .execute(&json!({ "index": 0, "path": path }).to_string(), &context)
            .await
            .expect_err("missing inbound attachments should fail");
        assert!(matches!(
            error,
            ToolError::ExecutionFailed { tool, message }
                if tool == ATTACHMENT_SAVE_TOOL_NAME && message.contains("no inbound attachments")
        ));

        let _ = fs::remove_dir_all(workspace);
    }

    #[tokio::test]
    async fn attachment_save_rejects_invalid_index() {
        let workspace = temp_workspace_root("bad-index");
        let runner = Arc::new(HostWasmToolRunner::for_bootstrap_workspace(&workspace));
        let tool = AttachmentSaveTool::new(runner);
        let context = test_context(Some(vec![inline_attachment("image/jpeg", &[1, 2, 3])]));
        let path = shared_path(&workspace, "bad-index.bin");

        let error = tool
            .execute(&json!({ "index": 2, "path": path }).to_string(), &context)
            .await
            .expect_err("out-of-range index should fail");
        assert!(matches!(
            error,
            ToolError::ExecutionFailed { tool, message }
                if tool == ATTACHMENT_SAVE_TOOL_NAME && message.contains("out of bounds")
        ));

        let _ = fs::remove_dir_all(workspace);
    }

    #[tokio::test]
    async fn attachment_save_writes_expected_bytes() {
        let workspace = temp_workspace_root("write-success");
        let runner = Arc::new(HostWasmToolRunner::for_bootstrap_workspace(&workspace));
        let tool = AttachmentSaveTool::new(runner);
        let expected = vec![0_u8, 17, 34, 255];
        let context = test_context(Some(vec![inline_attachment("image/jpeg", &expected)]));
        let path = shared_path(&workspace, "saved.bin");

        let output = tool
            .execute(&json!({ "index": 0, "path": path }).to_string(), &context)
            .await
            .expect("attachment_save should succeed");
        let payload: Value = serde_json::from_str(&output).expect("tool output should be JSON");

        assert_eq!(payload["saved"], true);
        assert_eq!(payload["source_index"], 0);
        assert_eq!(payload["mime_type"], "image/jpeg");
        assert_eq!(payload["bytes_written"], expected.len());
        let written = fs::read(payload["path"].as_str().expect("path should be a string"))
            .expect("saved file should be readable");
        assert_eq!(written, expected);

        let _ = fs::remove_dir_all(workspace);
    }

    #[tokio::test]
    async fn attachment_save_overwrite_flag_controls_replacement() {
        let workspace = temp_workspace_root("overwrite");
        let target_path = workspace.join("shared").join("existing.bin");
        fs::write(&target_path, [1_u8, 2, 3]).expect("existing file should be writable");

        let runner = Arc::new(HostWasmToolRunner::for_bootstrap_workspace(&workspace));
        let tool = AttachmentSaveTool::new(runner);
        let context = test_context(Some(vec![inline_attachment("application/pdf", &[9, 8, 7])]));
        let path = target_path.to_string_lossy().to_string();

        let no_overwrite_error = tool
            .execute(
                &json!({ "index": 0, "path": path, "overwrite": false }).to_string(),
                &context,
            )
            .await
            .expect_err("overwrite=false should fail when destination exists");
        assert!(matches!(
            no_overwrite_error,
            ToolError::ExecutionFailed { tool, message }
                if tool == ATTACHMENT_SAVE_TOOL_NAME && message.contains("already exists")
        ));

        tool.execute(
            &json!({ "index": 0, "path": target_path.to_string_lossy(), "overwrite": true })
                .to_string(),
            &context,
        )
        .await
        .expect("overwrite=true should replace existing file");
        assert_eq!(
            fs::read(&target_path).expect("overwritten file should be readable"),
            vec![9_u8, 8, 7]
        );

        let _ = fs::remove_dir_all(workspace);
    }
}
