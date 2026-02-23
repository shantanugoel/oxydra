use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ToolError;

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

#[async_trait]
pub trait Tool: Send + Sync {
    fn schema(&self) -> FunctionDecl;

    async fn execute(&self, args: &str) -> Result<String, ToolError>;

    fn timeout(&self) -> Duration;

    fn safety_tier(&self) -> SafetyTier;
}
