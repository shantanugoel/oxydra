use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use types::{
    FunctionDecl, MemoryHybridQueryRequest, MemoryNoteStoreRequest, MemoryRetrieval, SafetyTier,
    Tool, ToolError, ToolExecutionContext,
};

use crate::{execution_failed, invalid_args, parse_args};

pub const MEMORY_SEARCH_TOOL_NAME: &str = "memory_search";
pub const MEMORY_SAVE_TOOL_NAME: &str = "memory_save";
pub const MEMORY_UPDATE_TOOL_NAME: &str = "memory_update";
pub const MEMORY_DELETE_TOOL_NAME: &str = "memory_delete";

const MEMORY_SEARCH_MAX_TOP_K: usize = 20;
const MEMORY_SEARCH_DEFAULT_TOP_K: usize = 5;
const MEMORY_NOTE_TAG_MAX_COUNT: usize = 16;
const MEMORY_NOTE_TAG_MAX_CHARS: usize = 64;

/// Compute the well-known session ID for a user's persistent memory namespace.
fn user_memory_session_id(user_id: &str) -> String {
    format!("memory:{user_id}")
}

// ---------------------------------------------------------------------------
// Argument structs
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct MemorySearchArgs {
    query: String,
    top_k: Option<usize>,
    include_conversation: Option<bool>,
    tags: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct MemorySaveArgs {
    content: String,
    tags: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct MemoryUpdateArgs {
    note_id: String,
    content: String,
    tags: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct MemoryDeleteArgs {
    note_id: String,
}

// ---------------------------------------------------------------------------
// Helper: resolve current user_id / session_id from ToolExecutionContext
// ---------------------------------------------------------------------------

fn resolve_user_id(context: &ToolExecutionContext, tool_name: &str) -> Result<String, ToolError> {
    context
        .user_id
        .clone()
        .ok_or_else(|| execution_failed(tool_name, "user context is not available"))
}

fn resolve_session_id(
    context: &ToolExecutionContext,
    tool_name: &str,
) -> Result<String, ToolError> {
    context
        .session_id
        .clone()
        .ok_or_else(|| execution_failed(tool_name, "session context is not available"))
}

fn metadata_string_field(metadata: Option<&serde_json::Value>, key: &str) -> Option<String> {
    metadata
        .and_then(|value| value.get(key))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

fn metadata_tags_field(metadata: Option<&serde_json::Value>) -> Vec<String> {
    metadata
        .and_then(|value| value.get("tags"))
        .and_then(serde_json::Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// MemorySearchTool
// ---------------------------------------------------------------------------

pub struct MemorySearchTool {
    memory: Arc<dyn MemoryRetrieval>,
    vector_weight: f64,
    fts_weight: f64,
}

impl MemorySearchTool {
    pub fn new(memory: Arc<dyn MemoryRetrieval>, vector_weight: f64, fts_weight: f64) -> Self {
        Self {
            memory,
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
                 true to also search the current conversation history. Results with an empty \
                 note_id (from conversation search) cannot be updated or deleted via memory tools."
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
                    },
                    "tags": {
                        "type": "array",
                        "description": "Optional tag filters. When provided, results must include every requested tag.",
                        "maxItems": MEMORY_NOTE_TAG_MAX_COUNT,
                        "items": {
                            "type": "string",
                            "minLength": 1,
                            "maxLength": MEMORY_NOTE_TAG_MAX_CHARS
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
        let request: MemorySearchArgs = parse_args(MEMORY_SEARCH_TOOL_NAME, args)?;
        let user_id = resolve_user_id(context, MEMORY_SEARCH_TOOL_NAME)?;
        let memory_session = user_memory_session_id(&user_id);

        let top_k = request
            .top_k
            .unwrap_or(MEMORY_SEARCH_DEFAULT_TOP_K)
            .min(MEMORY_SEARCH_MAX_TOP_K);

        tracing::debug!(
            tool = MEMORY_SEARCH_TOOL_NAME,
            user_id = %user_id,
            query = %request.query,
            top_k,
            include_conversation = request.include_conversation.unwrap_or(false),
            "searching user memory"
        );

        // Search the user's memory namespace.
        let memory_results = self
            .memory
            .hybrid_query(MemoryHybridQueryRequest {
                session_id: memory_session,
                query: request.query.clone(),
                tags: request.tags.clone(),
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
                let metadata = result.metadata.as_ref();
                let note_id = metadata_string_field(metadata, "note_id").unwrap_or_default();
                json!({
                    "note_id": note_id,
                    "text": result.text,
                    "score": result.score,
                    "source": "user_memory",
                    "tags": metadata_tags_field(metadata),
                    "created_at": metadata_string_field(metadata, "created_at"),
                    "updated_at": metadata_string_field(metadata, "updated_at")
                })
            })
            .collect();

        // Optionally also search the current conversation session.
        if request.include_conversation.unwrap_or(false) {
            match resolve_session_id(context, MEMORY_SEARCH_TOOL_NAME) {
                Ok(conversation_session) => {
                    match self
                        .memory
                        .hybrid_query(MemoryHybridQueryRequest {
                            session_id: conversation_session,
                            query: request.query.clone(),
                            tags: request.tags.clone(),
                            query_embedding: None,
                            top_k: Some(top_k),
                            vector_weight: Some(self.vector_weight),
                            fts_weight: Some(self.fts_weight),
                        })
                        .await
                    {
                        Ok(conversation_results) => {
                            // Collect existing chunk IDs to deduplicate.
                            let existing_chunks: std::collections::HashSet<String> =
                                memory_results.iter().map(|r| r.chunk_id.clone()).collect();

                            for result in conversation_results {
                                if existing_chunks.contains(&result.chunk_id) {
                                    continue;
                                }
                                results.push(json!({
                                    "note_id": "",
                                    "text": result.text,
                                    "score": result.score,
                                    "source": "conversation",
                                    "tags": [],
                                    "created_at": null,
                                    "updated_at": null
                                }));
                            }
                        }
                        Err(error) => {
                            tracing::warn!(
                                tool = MEMORY_SEARCH_TOOL_NAME,
                                error = %error,
                                "conversation session search failed; returning memory results only"
                            );
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        tool = MEMORY_SEARCH_TOOL_NAME,
                        error = %error,
                        "could not resolve conversation session; returning memory results only"
                    );
                }
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

        tracing::debug!(
            tool = MEMORY_SEARCH_TOOL_NAME,
            user_id = %user_id,
            result_count = results.len(),
            "memory search complete"
        );

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
}

impl MemorySaveTool {
    pub fn new(memory: Arc<dyn MemoryRetrieval>) -> Self {
        Self { memory }
    }
}

#[async_trait]
impl Tool for MemorySaveTool {
    fn schema(&self) -> FunctionDecl {
        FunctionDecl::new(
            MEMORY_SAVE_TOOL_NAME,
            Some(
                "Save an important piece of information to your persistent memory as a natural \
                 language note. Use this proactively when the user shares preferences, makes \
                 decisions, states facts, or tells you something worth remembering across \
                 conversations. Also save corrected procedures — when you discover the right \
                 approach after initial failed attempts via any tools or other methods, save the working method for next time. \
                 Add tags when helpful to improve future filtering (for example: preference, workflow, project). \
                 Do NOT save ephemeral details, secrets, or one-off outputs. Saved notes persist \
                 across all sessions and channels. Returns a note_id for future reference."
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
                    },
                    "tags": {
                        "type": "array",
                        "description": "Optional tags for retrieval filtering and organization",
                        "maxItems": MEMORY_NOTE_TAG_MAX_COUNT,
                        "items": {
                            "type": "string",
                            "minLength": 1,
                            "maxLength": MEMORY_NOTE_TAG_MAX_CHARS
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
        let request: MemorySaveArgs = parse_args(MEMORY_SAVE_TOOL_NAME, args)?;
        if request.content.trim().is_empty() {
            return Err(invalid_args(
                MEMORY_SAVE_TOOL_NAME,
                "content must not be empty",
            ));
        }
        let user_id = resolve_user_id(context, MEMORY_SAVE_TOOL_NAME)?;
        let memory_session = user_memory_session_id(&user_id);
        let note_id = format!("note-{}", uuid::Uuid::new_v4());

        tracing::info!(
            tool = MEMORY_SAVE_TOOL_NAME,
            user_id = %user_id,
            note_id = %note_id,
            "saving note to user memory"
        );

        self.memory
            .store_note(MemoryNoteStoreRequest {
                session_id: memory_session,
                note_id: note_id.clone(),
                content: request.content,
                source: Some(MEMORY_SAVE_TOOL_NAME.to_owned()),
                tags: request.tags,
            })
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
}

impl MemoryUpdateTool {
    pub fn new(memory: Arc<dyn MemoryRetrieval>) -> Self {
        Self { memory }
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
                 preference), or when a saved procedure is superseded by a better approach. First \
                 use memory_search to find the note and its note_id, then call this with the \
                 note_id and the new content. Update tags when categorization changes, and keep \
                 corrected procedures current."
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
                    },
                    "tags": {
                        "type": "array",
                        "description": "Optional replacement tags for the updated note",
                        "maxItems": MEMORY_NOTE_TAG_MAX_COUNT,
                        "items": {
                            "type": "string",
                            "minLength": 1,
                            "maxLength": MEMORY_NOTE_TAG_MAX_CHARS
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
        let request: MemoryUpdateArgs = parse_args(MEMORY_UPDATE_TOOL_NAME, args)?;
        if request.content.trim().is_empty() {
            return Err(invalid_args(
                MEMORY_UPDATE_TOOL_NAME,
                "content must not be empty",
            ));
        }
        let user_id = resolve_user_id(context, MEMORY_UPDATE_TOOL_NAME)?;
        let memory_session = user_memory_session_id(&user_id);

        tracing::info!(
            tool = MEMORY_UPDATE_TOOL_NAME,
            user_id = %user_id,
            note_id = %request.note_id,
            "updating note in user memory"
        );

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
            .store_note(MemoryNoteStoreRequest {
                session_id: memory_session,
                note_id: request.note_id.clone(),
                content: request.content,
                source: Some(MEMORY_UPDATE_TOOL_NAME.to_owned()),
                tags: request.tags,
            })
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
}

impl MemoryDeleteTool {
    pub fn new(memory: Arc<dyn MemoryRetrieval>) -> Self {
        Self { memory }
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

    async fn execute(
        &self,
        args: &str,
        context: &ToolExecutionContext,
    ) -> Result<String, ToolError> {
        let request: MemoryDeleteArgs = parse_args(MEMORY_DELETE_TOOL_NAME, args)?;
        let user_id = resolve_user_id(context, MEMORY_DELETE_TOOL_NAME)?;
        let memory_session = user_memory_session_id(&user_id);

        tracing::info!(
            tool = MEMORY_DELETE_TOOL_NAME,
            user_id = %user_id,
            note_id = %request.note_id,
            "deleting note from user memory"
        );

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
    vector_weight: f64,
    fts_weight: f64,
) {
    registry.register(
        MEMORY_SEARCH_TOOL_NAME,
        MemorySearchTool::new(memory.clone(), vector_weight, fts_weight),
    );
    registry.register(MEMORY_SAVE_TOOL_NAME, MemorySaveTool::new(memory.clone()));
    registry.register(
        MEMORY_UPDATE_TOOL_NAME,
        MemoryUpdateTool::new(memory.clone()),
    );
    registry.register(MEMORY_DELETE_TOOL_NAME, MemoryDeleteTool::new(memory));
}
