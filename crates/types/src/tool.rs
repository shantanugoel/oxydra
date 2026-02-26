use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::ToolError;
use crate::channel::ChannelCapabilities;
use crate::model::StreamItem;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SafetyTier {
    ReadOnly,
    SideEffecting,
    Privileged,
}

/// A tool parameter schema expressed as a raw JSON Schema value.
///
/// Using `serde_json::Value` gives full JSON Schema coverage without
/// maintaining a typed struct that must be extended for every new keyword
/// (`enum`, `minimum`, `maximum`, `pattern`, `anyOf`, etc.).
///
/// Construct schemas with `serde_json::json!({...})`:
///
/// ```rust,ignore
/// use serde_json::json;
/// let params = json!({
///     "type": "object",
///     "required": ["query"],
///     "properties": {
///         "query":     { "type": "string", "minLength": 1 },
///         "count":     { "type": "integer", "minimum": 1, "maximum": 10 },
///         "freshness": { "type": "string", "enum": ["day", "week", "month"] }
///     }
/// });
/// ```
pub type ToolParameterSchema = Value;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionDecl {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parameters: ToolParameterSchema,
}

impl FunctionDecl {
    pub fn new(
        name: impl Into<String>,
        description: Option<String>,
        parameters: ToolParameterSchema,
    ) -> Self {
        Self {
            name: name.into(),
            description,
            parameters,
        }
    }
}

/// Per-invocation context passed to [`Tool::execute`].
///
/// Carries runtime state that varies per turn (user identity, active session,
/// channel capabilities). Tools that do not need this context simply ignore
/// the parameter. Memory tools read `user_id` and `session_id` to scope
/// operations; the `send_media` tool uses `channel_capabilities` and
/// `event_sender` to deliver media through the connected channel.
#[derive(Clone, Default)]
pub struct ToolExecutionContext {
    /// The authenticated user ID for the current turn, if known.
    pub user_id: Option<String>,
    /// The active conversation session ID for the current turn, if known.
    pub session_id: Option<String>,
    /// Capabilities of the channel the user is connected through.
    /// `None` when channel info is not available (e.g., tests).
    pub channel_capabilities: Option<ChannelCapabilities>,
    /// Stream event sender for emitting out-of-band data (e.g., media
    /// attachments) during tool execution. Tools like `send_media` use this
    /// to push media through the gateway to the channel adapter.
    pub event_sender: Option<mpsc::UnboundedSender<StreamItem>>,
    /// Channel that originated the current turn (e.g. "telegram", "tui").
    pub channel_id: Option<String>,
    /// Channel-context identifier within that channel.
    /// - Telegram: "{chat_id}" or "{chat_id}:{thread_id}"
    /// - TUI: gateway `session_id`
    pub channel_context_id: Option<String>,
}

impl std::fmt::Debug for ToolExecutionContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolExecutionContext")
            .field("user_id", &self.user_id)
            .field("session_id", &self.session_id)
            .field("channel_capabilities", &self.channel_capabilities)
            .field(
                "event_sender",
                &self.event_sender.as_ref().map(|_| "<sender>"),
            )
            .field("channel_id", &self.channel_id)
            .field("channel_context_id", &self.channel_context_id)
            .finish()
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn schema(&self) -> FunctionDecl;

    async fn execute(
        &self,
        args: &str,
        context: &ToolExecutionContext,
    ) -> Result<String, ToolError>;

    fn timeout(&self) -> Duration;

    fn safety_tier(&self) -> SafetyTier;
}
