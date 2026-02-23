use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Mutex;
use types::{
    FunctionDecl, MemoryHybridQueryRequest, MemoryRetrieval, SafetyTier, Tool, ToolError,
};

use crate::{execution_failed, invalid_args, parse_args};

pub const MEMORY_SEARCH_TOOL_NAME: &str = "memory_search";
pub const MEMORY_SAVE_TOOL_NAME: &str = "memory_save";
pub const MEMORY_UPDATE_TOOL_NAME: &str = "memory_update";
pub const MEMORY_DELETE_TOOL_NAME: &str = "memory_delete";

const MEMORY_SEARCH_MAX_TOP_K: usize = 20;
const MEMORY_SEARCH_DEFAULT_TOP_K: usize = 5;

/// Compute the well-known session ID for a user's persistent memory namespace.
fn user_memory_session_id(user_id: &str) -> String {
    format!("memory:{user_id}")
}

/// Shared context holders that are set before each turn by the gateway turn
/// runner and read by memory tools during execution.
#[derive(Clone)]
pub struct MemoryToolContext {
    pub user_id: Arc<Mutex<Option<String>>>,
    pub session_id: Arc<Mutex<Option<String>>>,
}

impl MemoryToolContext {
    pub fn new() -> Self {
        Self {
            user_id: Arc::new(Mutex::new(None)),
            session_id: Arc::new(Mutex::new(None)),
        }
    }
}

impl Default for MemoryToolContext {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Argument structs
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct MemorySearchArgs {
    query: String,
    top_k: Option<usize>,
    include_conversation: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct MemorySaveArgs {
    content: String,
}

#[derive(Debug, Deserialize)]
struct MemoryUpdateArgs {
    note_id: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct MemoryDeleteArgs {
    note_id: String,
}

// ---------------------------------------------------------------------------
// Helper: resolve current user_id from shared context
// ---------------------------------------------------------------------------

async fn resolve_user_id(context: &MemoryToolContext, tool_name: &str) -> Result<String, ToolError> {
    context
        .user_id
        .lock()
        .await
        .clone()
        .ok_or_else(|| execution_failed(tool_name, "user context is not available"))
}

async fn resolve_session_id(
    context: &MemoryToolContext,
    tool_name: &str,
) -> Result<String, ToolError> {
    context
        .session_id
        .lock()
        .await
        .clone()
        .ok_or_else(|| execution_failed(tool_name, "session context is not available"))
}

// ---------------------------------------------------------------------------
// MemorySearchTool
// ---------------------------------------------------------------------------

pub struct MemorySearchTool {
    memory: Arc<dyn MemoryRetrieval>,
    context: MemoryToolContext,
    vector_weight: f64,
    fts_weight: f64,
}

impl MemorySearchTool {
    pub fn new(
        memory: Arc<dyn MemoryRetrieval>,
        context: MemoryToolContext,
        vector_weight: f64,
        fts_weight: f64,
    ) -> Self {
        Self {
            memory,
            context,
            vector_weight,
            fts_weight,
        }
    }
}

#[async_trait]
impl Tool for MemorySearchTool {
    fn schema(&self) -> FunctionDecl {
        FunctionDecl::new(
            MEMORY_SEARCH_TOOL_NAME,
            Some(
                "Search your persistent memory for relevant information. Use this when you need \
                 to recall user preferences, past decisions, stored facts, or anything previously \
                 saved. Results are ranked by relevance using semantic similarity and keyword \
                 matching. By default searches only your saved notes; set include_conversation to \
                 true to also search the current conversation history."
                    .to_owned(),
            ),
            json!({
                "type": "object",
                "required": ["query"],
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Natural language search query",
                        "minLength": 1
                    },
                    "top_k": {
                        "type": "integer",
                        "description": "Max results to return (default 5, max 20)",
                        "minimum": 1,
                        "maximum": MEMORY_SEARCH_MAX_TOP_K,
                        "default": MEMORY_SEARCH_DEFAULT_TOP_K
                    },
                    "include_conversation": {
                        "type": "boolean",
                        "description": "Also search the current conversation session (default false)",
                        "default": false
                    }
                }
            }),
        )
    }

    async fn execute(&self, args: &str) -> Result<String, ToolError> {
        let request: MemorySearchArgs = parse_args(MEMORY_SEARCH_TOOL_NAME, args)?;
        let user_id = resolve_user_id(&self.context, MEMORY_SEARCH_TOOL_NAME).await?;
        let memory_session = user_memory_session_id(&user_id);

        let top_k = request
            .top_k
            .unwrap_or(MEMORY_SEARCH_DEFAULT_TOP_K)
            .min(MEMORY_SEARCH_MAX_TOP_K);

        // Search the user's memory namespace.
        let memory_results = self
            .memory
            .hybrid_query(MemoryHybridQueryRequest {
                session_id: memory_session,
                query: request.query.clone(),
                query_embedding: None,
                top_k: Some(top_k),
                vector_weight: Some(self.vector_weight),
                fts_weight: Some(self.fts_weight),
            })
            .await
            .map_err(|error| {
                execution_failed(
                    MEMORY_SEARCH_TOOL_NAME,
                    format!("memory search failed: {error}"),
                )
            })?;

        let mut results: Vec<serde_json::Value> = memory_results
            .iter()
            .map(|result| {
                let note_id = result
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("note_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                json!({
                    "note_id": note_id,
                    "text": result.text,
                    "score": result.score,
                    "source": "user_memory"
                })
            })
            .collect();

        // Optionally also search the current conversation session.
        if request.include_conversation.unwrap_or(false)
            && let Ok(conversation_session) =
                resolve_session_id(&self.context, MEMORY_SEARCH_TOOL_NAME).await
            && let Ok(conversation_results) = self
                .memory
                .hybrid_query(MemoryHybridQueryRequest {
                    session_id: conversation_session,
                    query: request.query.clone(),
                    query_embedding: None,
                    top_k: Some(top_k),
                    vector_weight: Some(self.vector_weight),
                    fts_weight: Some(self.fts_weight),
                })
                .await
        {
            // Collect existing chunk IDs to deduplicate.
            let existing_chunks: std::collections::HashSet<String> = memory_results
                .iter()
                .map(|r| r.chunk_id.clone())
                .collect();

            for result in conversation_results {
                if existing_chunks.contains(&result.chunk_id) {
                    continue;
                }
                results.push(json!({
                    "note_id": "",
                    "text": result.text,
                    "score": result.score,
                    "source": "conversation"
                }));
            }
        }

        // Re-sort merged results by score descending.
        results.sort_by(|a, b| {
            let score_a = a.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let score_b = b.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0);
            score_b
                .partial_cmp(&score_a)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results.truncate(top_k);

        serde_json::to_string_pretty(&results)
            .map_err(|error| execution_failed(MEMORY_SEARCH_TOOL_NAME, error.to_string()))
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(15)
    }

    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::ReadOnly
    }
}

// ---------------------------------------------------------------------------
// MemorySaveTool
// ---------------------------------------------------------------------------

pub struct MemorySaveTool {
    memory: Arc<dyn MemoryRetrieval>,
    context: MemoryToolContext,
}

impl MemorySaveTool {
    pub fn new(memory: Arc<dyn MemoryRetrieval>, context: MemoryToolContext) -> Self {
        Self { memory, context }
    }
}

#[async_trait]
impl Tool for MemorySaveTool {
    fn schema(&self) -> FunctionDecl {
        FunctionDecl::new(
            MEMORY_SAVE_TOOL_NAME,
            Some(
                "Save an important piece of information to your persistent memory as a natural \
                 language note. Use this proactively whenever the user shares preferences, makes \
                 decisions, states facts, or tells you something worth remembering across \
                 conversations. Saved notes persist across all sessions and channels. Returns a \
                 note_id for future reference."
                    .to_owned(),
            ),
            json!({
                "type": "object",
                "required": ["content"],
                "properties": {
                    "content": {
                        "type": "string",
                        "description": "Natural language text to save (fact, preference, decision, etc.)",
                        "minLength": 1
                    }
                }
            }),
        )
    }

    async fn execute(&self, args: &str) -> Result<String, ToolError> {
        let request: MemorySaveArgs = parse_args(MEMORY_SAVE_TOOL_NAME, args)?;
        if request.content.trim().is_empty() {
            return Err(invalid_args(
                MEMORY_SAVE_TOOL_NAME,
                "content must not be empty",
            ));
        }
        let user_id = resolve_user_id(&self.context, MEMORY_SAVE_TOOL_NAME).await?;
        let memory_session = user_memory_session_id(&user_id);
        let note_id = format!("note-{}", uuid::Uuid::new_v4());

        self.memory
            .store_note(&memory_session, &note_id, &request.content)
            .await
            .map_err(|error| {
                execution_failed(
                    MEMORY_SAVE_TOOL_NAME,
                    format!("failed to save note: {error}"),
                )
            })?;

        let result = json!({
            "note_id": note_id,
            "message": "Note saved successfully."
        });
        serde_json::to_string(&result)
            .map_err(|error| execution_failed(MEMORY_SAVE_TOOL_NAME, error.to_string()))
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(10)
    }

    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::SideEffecting
    }
}

// ---------------------------------------------------------------------------
// MemoryUpdateTool
// ---------------------------------------------------------------------------

pub struct MemoryUpdateTool {
    memory: Arc<dyn MemoryRetrieval>,
    context: MemoryToolContext,
}

impl MemoryUpdateTool {
    pub fn new(memory: Arc<dyn MemoryRetrieval>, context: MemoryToolContext) -> Self {
        Self { memory, context }
    }
}

#[async_trait]
impl Tool for MemoryUpdateTool {
    fn schema(&self) -> FunctionDecl {
        FunctionDecl::new(
            MEMORY_UPDATE_TOOL_NAME,
            Some(
                "Update a previously saved note with new content. Use this when the user corrects \
                 or changes previously stored information (e.g., a new preferred name, an updated \
                 preference). First use memory_search to find the note and its note_id, then call \
                 this with the note_id and the new content."
                    .to_owned(),
            ),
            json!({
                "type": "object",
                "required": ["note_id", "content"],
                "properties": {
                    "note_id": {
                        "type": "string",
                        "description": "ID of the note to update (from memory_search or memory_save results)",
                        "minLength": 1
                    },
                    "content": {
                        "type": "string",
                        "description": "New content to replace the existing note",
                        "minLength": 1
                    }
                }
            }),
        )
    }

    async fn execute(&self, args: &str) -> Result<String, ToolError> {
        let request: MemoryUpdateArgs = parse_args(MEMORY_UPDATE_TOOL_NAME, args)?;
        if request.content.trim().is_empty() {
            return Err(invalid_args(
                MEMORY_UPDATE_TOOL_NAME,
                "content must not be empty",
            ));
        }
        let user_id = resolve_user_id(&self.context, MEMORY_UPDATE_TOOL_NAME).await?;
        let memory_session = user_memory_session_id(&user_id);

        // Delete the old note first.
        let found = self
            .memory
            .delete_note(&memory_session, &request.note_id)
            .await
            .map_err(|error| {
                execution_failed(
                    MEMORY_UPDATE_TOOL_NAME,
                    format!("failed to delete old note: {error}"),
                )
            })?;
        if !found {
            return Err(execution_failed(
                MEMORY_UPDATE_TOOL_NAME,
                format!("note `{}` not found", request.note_id),
            ));
        }

        // Store with the same note_id.
        self.memory
            .store_note(&memory_session, &request.note_id, &request.content)
            .await
            .map_err(|error| {
                execution_failed(
                    MEMORY_UPDATE_TOOL_NAME,
                    format!("failed to store updated note: {error}"),
                )
            })?;

        let result = json!({
            "note_id": request.note_id,
            "message": "Note updated successfully."
        });
        serde_json::to_string(&result)
            .map_err(|error| execution_failed(MEMORY_UPDATE_TOOL_NAME, error.to_string()))
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(10)
    }

    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::SideEffecting
    }
}

// ---------------------------------------------------------------------------
// MemoryDeleteTool
// ---------------------------------------------------------------------------

pub struct MemoryDeleteTool {
    memory: Arc<dyn MemoryRetrieval>,
    context: MemoryToolContext,
}

impl MemoryDeleteTool {
    pub fn new(memory: Arc<dyn MemoryRetrieval>, context: MemoryToolContext) -> Self {
        Self { memory, context }
    }
}

#[async_trait]
impl Tool for MemoryDeleteTool {
    fn schema(&self) -> FunctionDecl {
        FunctionDecl::new(
            MEMORY_DELETE_TOOL_NAME,
            Some(
                "Delete a previously saved note from memory. Use this when the user asks you to \
                 forget specific information. First use memory_search to find the note and its \
                 note_id, then call this with the note_id to remove it."
                    .to_owned(),
            ),
            json!({
                "type": "object",
                "required": ["note_id"],
                "properties": {
                    "note_id": {
                        "type": "string",
                        "description": "ID of the note to delete (from memory_search results)",
                        "minLength": 1
                    }
                }
            }),
        )
    }

    async fn execute(&self, args: &str) -> Result<String, ToolError> {
        let request: MemoryDeleteArgs = parse_args(MEMORY_DELETE_TOOL_NAME, args)?;
        let user_id = resolve_user_id(&self.context, MEMORY_DELETE_TOOL_NAME).await?;
        let memory_session = user_memory_session_id(&user_id);

        let found = self
            .memory
            .delete_note(&memory_session, &request.note_id)
            .await
            .map_err(|error| {
                execution_failed(
                    MEMORY_DELETE_TOOL_NAME,
                    format!("failed to delete note: {error}"),
                )
            })?;

        if !found {
            return Err(execution_failed(
                MEMORY_DELETE_TOOL_NAME,
                format!("note `{}` not found", request.note_id),
            ));
        }

        let result = json!({
            "note_id": request.note_id,
            "message": "Note deleted successfully."
        });
        serde_json::to_string(&result)
            .map_err(|error| execution_failed(MEMORY_DELETE_TOOL_NAME, error.to_string()))
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(10)
    }

    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::SideEffecting
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Register all four memory tools in the given tool registry.
pub fn register_memory_tools(
    registry: &mut crate::ToolRegistry,
    memory: Arc<dyn MemoryRetrieval>,
    context: MemoryToolContext,
    vector_weight: f64,
    fts_weight: f64,
) {
    registry.register(
        MEMORY_SEARCH_TOOL_NAME,
        MemorySearchTool::new(memory.clone(), context.clone(), vector_weight, fts_weight),
    );
    registry.register(
        MEMORY_SAVE_TOOL_NAME,
        MemorySaveTool::new(memory.clone(), context.clone()),
    );
    registry.register(
        MEMORY_UPDATE_TOOL_NAME,
        MemoryUpdateTool::new(memory.clone(), context.clone()),
    );
    registry.register(
        MEMORY_DELETE_TOOL_NAME,
        MemoryDeleteTool::new(memory, context),
    );
}
