use std::{fmt, fs, io, path::Path};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::ProviderError;

const PINNED_MODEL_CATALOG_SNAPSHOT: &str = include_str!("../data/pinned_model_catalog.json");

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModelId(pub String);

impl From<&str> for ModelId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<String> for ModelId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl fmt::Display for ModelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderId(pub String);

impl From<&str> for ProviderId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<String> for ProviderId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: MessageRole,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Context {
    pub provider: ProviderId,
    pub model: ModelId,
    #[serde(default)]
    pub messages: Vec<Message>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub message: Message,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallDelta {
    pub index: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UsageUpdate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamItem {
    Text(String),
    ToolCallDelta(ToolCallDelta),
    ReasoningDelta(String),
    UsageUpdate(UsageUpdate),
    ConnectionLost(String),
    FinishReason(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ProviderCaps {
    #[serde(default)]
    pub supports_streaming: bool,
    #[serde(default)]
    pub supports_tools: bool,
    #[serde(default)]
    pub supports_json_mode: bool,
    #[serde(default)]
    pub supports_reasoning_traces: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_input_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_context_tokens: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelDescriptor {
    pub provider: ProviderId,
    pub model: ModelId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default)]
    pub caps: ProviderCaps,
    #[serde(default)]
    pub deprecated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ModelCatalog {
    #[serde(default)]
    pub models: Vec<ModelDescriptor>,
}

impl ModelCatalog {
    pub fn new(models: Vec<ModelDescriptor>) -> Self {
        let mut catalog = Self { models };
        catalog.sort_models();
        catalog
    }

    pub fn from_snapshot_str(snapshot: &str) -> Result<Self, ProviderError> {
        let parsed: Self = serde_json::from_str(snapshot)?;
        Ok(Self::new(parsed.models))
    }

    pub fn from_pinned_snapshot() -> Result<Self, ProviderError> {
        Self::from_snapshot_str(PINNED_MODEL_CATALOG_SNAPSHOT)
    }

    pub fn pinned_snapshot_json() -> &'static str {
        PINNED_MODEL_CATALOG_SNAPSHOT
    }

    pub fn pinned_snapshot_path() -> &'static str {
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/data/pinned_model_catalog.json"
        )
    }

    /// Canonicalizes and writes a pinned catalog snapshot from already-fetched JSON data.
    ///
    /// This intentionally does not fetch `https://models.dev/api.json`.
    /// Network retrieval is deferred to a later operator-facing refresh path (CLI phase),
    /// while runtime/startup continue using committed offline snapshots.
    pub fn regenerate_snapshot(
        source_snapshot: &str,
        output_path: impl AsRef<Path>,
    ) -> io::Result<()> {
        let catalog = Self::from_snapshot_str(source_snapshot)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
        let canonical_snapshot = serde_json::to_string_pretty(&catalog)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
        fs::write(output_path, format!("{canonical_snapshot}\n"))
    }

    pub fn get(&self, provider: &ProviderId, model: &ModelId) -> Option<&ModelDescriptor> {
        self.models
            .iter()
            .find(|descriptor| descriptor.provider == *provider && descriptor.model == *model)
    }

    pub fn validate<'a>(
        &'a self,
        provider: &ProviderId,
        model: &ModelId,
    ) -> Result<&'a ModelDescriptor, ProviderError> {
        self.get(provider, model)
            .ok_or_else(|| ProviderError::UnknownModel {
                provider: provider.clone(),
                model: model.clone(),
            })
    }

    fn sort_models(&mut self) {
        self.models.sort_by(|left, right| {
            left.provider
                .0
                .cmp(&right.provider.0)
                .then_with(|| left.model.0.cmp(&right.model.0))
        });
    }
}
