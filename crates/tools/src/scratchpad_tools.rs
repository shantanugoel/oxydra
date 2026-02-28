use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use types::{
    FunctionDecl, MemoryRetrieval, MemoryScratchpadClearRequest, MemoryScratchpadReadRequest,
    MemoryScratchpadWriteRequest, SafetyTier, Tool, ToolError, ToolExecutionContext,
};

use crate::{execution_failed, parse_args};

pub const SCRATCHPAD_READ_TOOL_NAME: &str = "scratchpad_read";
pub const SCRATCHPAD_WRITE_TOOL_NAME: &str = "scratchpad_write";
pub const SCRATCHPAD_CLEAR_TOOL_NAME: &str = "scratchpad_clear";

const SCRATCHPAD_MAX_ITEMS: usize = 32;
const SCRATCHPAD_MAX_ITEM_CHARS: usize = 240;

#[derive(Debug, Deserialize, Default)]
struct ScratchpadReadArgs {}

#[derive(Debug, Deserialize)]
struct ScratchpadWriteArgs {
    items: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
struct ScratchpadClearArgs {}

fn resolve_session_id(
    context: &ToolExecutionContext,
    tool_name: &str,
) -> Result<String, ToolError> {
    context
        .session_id
        .clone()
        .ok_or_else(|| execution_failed(tool_name, "session context is not available"))
}

pub struct ScratchpadReadTool {
    memory: Arc<dyn MemoryRetrieval>,
}

impl ScratchpadReadTool {
    pub fn new(memory: Arc<dyn MemoryRetrieval>) -> Self {
        Self { memory }
    }
}

#[async_trait]
impl Tool for ScratchpadReadTool {
    fn schema(&self) -> FunctionDecl {
        FunctionDecl::new(
            SCRATCHPAD_READ_TOOL_NAME,
            Some(
                "Read the session scratchpad used for active task coordination in this \
                 conversation. This is session-scoped working memory, separate from persistent \
                 cross-session notes."
                    .to_owned(),
            ),
            json!({
                "type": "object",
                "properties": {}
            }),
        )
    }

    async fn execute(
        &self,
        args: &str,
        context: &ToolExecutionContext,
    ) -> Result<String, ToolError> {
        let _request: ScratchpadReadArgs = parse_args(SCRATCHPAD_READ_TOOL_NAME, args)?;
        let session_id = resolve_session_id(context, SCRATCHPAD_READ_TOOL_NAME)?;
        let scratchpad = self
            .memory
            .read_scratchpad(MemoryScratchpadReadRequest {
                session_id: session_id.clone(),
            })
            .await
            .map_err(|error| {
                execution_failed(
                    SCRATCHPAD_READ_TOOL_NAME,
                    format!("failed to read scratchpad: {error}"),
                )
            })?;

        let response = if let Some(state) = scratchpad {
            json!({
                "session_id": state.session_id,
                "items": state.items,
                "updated_at": state.updated_at,
                "item_count": state.items.len()
            })
        } else {
            json!({
                "session_id": session_id,
                "items": [],
                "updated_at": null,
                "item_count": 0
            })
        };
        serde_json::to_string(&response)
            .map_err(|error| execution_failed(SCRATCHPAD_READ_TOOL_NAME, error.to_string()))
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(10)
    }

    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::ReadOnly
    }
}

pub struct ScratchpadWriteTool {
    memory: Arc<dyn MemoryRetrieval>,
}

impl ScratchpadWriteTool {
    pub fn new(memory: Arc<dyn MemoryRetrieval>) -> Self {
        Self { memory }
    }
}

#[async_trait]
impl Tool for ScratchpadWriteTool {
    fn schema(&self) -> FunctionDecl {
        FunctionDecl::new(
            SCRATCHPAD_WRITE_TOOL_NAME,
            Some(
                "Replace the session scratchpad with a concise list of active working items. \
                 Use this to track current sub-goals during long tool-heavy execution without \
                 polluting durable memory."
                    .to_owned(),
            ),
            json!({
                "type": "object",
                "required": ["items"],
                "properties": {
                    "items": {
                        "type": "array",
                        "description": "Ordered scratchpad entries for the current session",
                        "minItems": 1,
                        "maxItems": SCRATCHPAD_MAX_ITEMS,
                        "items": {
                            "type": "string",
                            "minLength": 1,
                            "maxLength": SCRATCHPAD_MAX_ITEM_CHARS
                        }
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
        let request: ScratchpadWriteArgs = parse_args(SCRATCHPAD_WRITE_TOOL_NAME, args)?;
        let session_id = resolve_session_id(context, SCRATCHPAD_WRITE_TOOL_NAME)?;
        let result = self
            .memory
            .write_scratchpad(MemoryScratchpadWriteRequest {
                session_id,
                items: request.items,
            })
            .await
            .map_err(|error| {
                execution_failed(
                    SCRATCHPAD_WRITE_TOOL_NAME,
                    format!("failed to write scratchpad: {error}"),
                )
            })?;

        let response = json!({
            "updated": result.updated,
            "item_count": result.item_count,
            "message": "Scratchpad updated successfully."
        });
        serde_json::to_string(&response)
            .map_err(|error| execution_failed(SCRATCHPAD_WRITE_TOOL_NAME, error.to_string()))
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(10)
    }

    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::SideEffecting
    }
}

pub struct ScratchpadClearTool {
    memory: Arc<dyn MemoryRetrieval>,
}

impl ScratchpadClearTool {
    pub fn new(memory: Arc<dyn MemoryRetrieval>) -> Self {
        Self { memory }
    }
}

#[async_trait]
impl Tool for ScratchpadClearTool {
    fn schema(&self) -> FunctionDecl {
        FunctionDecl::new(
            SCRATCHPAD_CLEAR_TOOL_NAME,
            Some(
                "Clear the current session scratchpad. Use this to free space after completing \
                 or reshaping a plan so new working items can be recorded."
                    .to_owned(),
            ),
            json!({
                "type": "object",
                "properties": {}
            }),
        )
    }

    async fn execute(
        &self,
        args: &str,
        context: &ToolExecutionContext,
    ) -> Result<String, ToolError> {
        let _request: ScratchpadClearArgs = parse_args(SCRATCHPAD_CLEAR_TOOL_NAME, args)?;
        let session_id = resolve_session_id(context, SCRATCHPAD_CLEAR_TOOL_NAME)?;
        let cleared = self
            .memory
            .clear_scratchpad(MemoryScratchpadClearRequest { session_id })
            .await
            .map_err(|error| {
                execution_failed(
                    SCRATCHPAD_CLEAR_TOOL_NAME,
                    format!("failed to clear scratchpad: {error}"),
                )
            })?;
        let message = if cleared {
            "Scratchpad cleared successfully."
        } else {
            "Scratchpad was already empty."
        };
        serde_json::to_string(&json!({
            "cleared": cleared,
            "message": message
        }))
        .map_err(|error| execution_failed(SCRATCHPAD_CLEAR_TOOL_NAME, error.to_string()))
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(10)
    }

    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::SideEffecting
    }
}

pub fn register_scratchpad_tools(
    registry: &mut crate::ToolRegistry,
    memory: Arc<dyn MemoryRetrieval>,
) {
    registry.register(
        SCRATCHPAD_READ_TOOL_NAME,
        ScratchpadReadTool::new(memory.clone()),
    );
    registry.register(
        SCRATCHPAD_WRITE_TOOL_NAME,
        ScratchpadWriteTool::new(memory.clone()),
    );
    registry.register(SCRATCHPAD_CLEAR_TOOL_NAME, ScratchpadClearTool::new(memory));
}
