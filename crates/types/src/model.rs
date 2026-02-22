use std::{collections::BTreeMap, fmt, fs, io, path::Path};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{error::ProviderError, tool::FunctionDecl};

const PINNED_MODEL_CATALOG_SNAPSHOT: &str = include_str!("../data/pinned_model_catalog.json");
const PINNED_CAPS_OVERRIDES: &str = include_str!("../data/oxydra_caps_overrides.json");

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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<FunctionDecl>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageUpdate>,
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

// ---------------------------------------------------------------------------
// Runtime capability struct — derivation uses models.dev fields + Oxydra overlay
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// models.dev-aligned model descriptor types
// ---------------------------------------------------------------------------

/// Interleaved reasoning output configuration (e.g. `reasoning_content` field).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterleavedSpec {
    pub field: String,
}

/// Input/output modality lists (e.g. `["text", "image"]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Modalities {
    #[serde(default)]
    pub input: Vec<String>,
    #[serde(default)]
    pub output: Vec<String>,
}

/// Per-million-token pricing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ModelCost {
    #[serde(default)]
    pub input: f64,
    #[serde(default)]
    pub output: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
}

/// Token limits for context window and output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ModelLimits {
    #[serde(default)]
    pub context: u32,
    #[serde(default)]
    pub output: u32,
}

/// A single model entry aligned with the models.dev per-model schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelDescriptor {
    /// Canonical model identifier, e.g. `"gpt-4o"`, `"claude-3-5-sonnet-latest"`.
    pub id: String,
    /// Human-readable display name.
    pub name: String,
    /// Model family grouping, e.g. `"gpt-4o"`, `"claude"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    /// Supports file/image attachments.
    #[serde(default)]
    pub attachment: bool,
    /// Supports reasoning/thinking.
    #[serde(default)]
    pub reasoning: bool,
    /// Supports function/tool calling.
    #[serde(default)]
    pub tool_call: bool,
    /// Interleaved reasoning output configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interleaved: Option<InterleavedSpec>,
    /// Supports JSON mode / structured output.
    #[serde(default)]
    pub structured_output: bool,
    /// Supports temperature parameter.
    #[serde(default)]
    pub temperature: bool,
    /// Training data cutoff, e.g. `"2024-04"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub knowledge: Option<String>,
    /// Release date (ISO format).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_date: Option<String>,
    /// Last updated date (ISO format).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_updated: Option<String>,
    /// Input/output modality lists.
    #[serde(default)]
    pub modalities: Modalities,
    /// Whether model weights are publicly available.
    #[serde(default)]
    pub open_weights: bool,
    /// Per-million-token pricing.
    #[serde(default)]
    pub cost: ModelCost,
    /// Context window and output token limits.
    #[serde(default)]
    pub limit: ModelLimits,
}

impl ModelDescriptor {
    /// Derives a [`ProviderCaps`] from the models.dev fields.
    ///
    /// This provides a baseline derivation. Step 2 will add an Oxydra overlay
    /// mechanism for fields that models.dev cannot express (e.g. per-provider
    /// streaming support).
    pub fn to_provider_caps(&self) -> ProviderCaps {
        let context = if self.limit.context > 0 {
            Some(self.limit.context)
        } else {
            None
        };
        let output = if self.limit.output > 0 {
            Some(self.limit.output)
        } else {
            None
        };
        ProviderCaps {
            // Streaming support cannot be inferred from models.dev; default to
            // true for all known providers. Step 2 adds per-provider overlays.
            supports_streaming: true,
            supports_tools: self.tool_call,
            supports_json_mode: self.structured_output,
            supports_reasoning_traces: self.reasoning || self.interleaved.is_some(),
            max_input_tokens: context,
            max_output_tokens: output,
            max_context_tokens: context,
        }
    }
}

// ---------------------------------------------------------------------------
// Oxydra capability overrides — per-provider and per-model overlay
// ---------------------------------------------------------------------------

/// A single capability override entry. All fields are optional — only
/// present fields override the baseline derived from models.dev.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CapsOverrideEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_streaming: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_tools: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_json_mode: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_reasoning_traces: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_input_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_context_tokens: Option<u32>,
    /// Marks a model as deprecated. Since models.dev does not have a universal
    /// deprecated field, this is tracked exclusively via the Oxydra overlay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deprecated: Option<bool>,
}

/// Oxydra-specific capability overrides loaded from `oxydra_caps_overrides.json`.
///
/// Two tiers:
/// 1. `provider_defaults` — keyed by provider ID (e.g. `"openai"`), applied to
///    all models under that provider.
/// 2. `overrides` — keyed by `"provider/model"` (e.g. `"anthropic/claude-3-5-sonnet-latest"`),
///    applied to a specific model and takes precedence over provider defaults.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CapsOverrides {
    #[serde(default)]
    pub overrides: BTreeMap<String, CapsOverrideEntry>,
    #[serde(default)]
    pub provider_defaults: BTreeMap<String, CapsOverrideEntry>,
}

impl CapsOverrides {
    /// Checks whether a model is marked as deprecated in the overlay.
    ///
    /// Looks up the model-specific override entry `"catalog_provider/model_id"`.
    /// Returns `true` only if the entry explicitly sets `deprecated = true`.
    pub fn is_deprecated(&self, catalog_provider_id: &str, model_id: &str) -> bool {
        let key = format!("{catalog_provider_id}/{model_id}");
        self.overrides
            .get(&key)
            .and_then(|entry| entry.deprecated)
            .unwrap_or(false)
    }
}

/// Derives a [`ProviderCaps`] from a model descriptor and the Oxydra overlay.
///
/// Resolution order (later wins):
/// 1. Baseline from `model.to_provider_caps()` (models.dev fields)
/// 2. Provider-level defaults from `overrides.provider_defaults[catalog_provider_id]`
/// 3. Model-specific overrides from `overrides.overrides["catalog_provider_id/model_id"]`
pub fn derive_caps(
    catalog_provider_id: &str,
    model: &ModelDescriptor,
    overrides: &CapsOverrides,
) -> ProviderCaps {
    let mut caps = model.to_provider_caps();

    // Apply provider-level defaults
    if let Some(provider_entry) = overrides.provider_defaults.get(catalog_provider_id) {
        apply_override(&mut caps, provider_entry);
    }

    // Apply model-specific overrides (takes precedence)
    let model_key = format!("{}/{}", catalog_provider_id, model.id);
    if let Some(model_entry) = overrides.overrides.get(&model_key) {
        apply_override(&mut caps, model_entry);
    }

    caps
}

/// Applies non-`None` fields from an override entry onto existing caps.
fn apply_override(caps: &mut ProviderCaps, entry: &CapsOverrideEntry) {
    if let Some(v) = entry.supports_streaming {
        caps.supports_streaming = v;
    }
    if let Some(v) = entry.supports_tools {
        caps.supports_tools = v;
    }
    if let Some(v) = entry.supports_json_mode {
        caps.supports_json_mode = v;
    }
    if let Some(v) = entry.supports_reasoning_traces {
        caps.supports_reasoning_traces = v;
    }
    if let Some(v) = entry.max_input_tokens {
        caps.max_input_tokens = Some(v);
    }
    if let Some(v) = entry.max_output_tokens {
        caps.max_output_tokens = Some(v);
    }
    if let Some(v) = entry.max_context_tokens {
        caps.max_context_tokens = Some(v);
    }
}

// ---------------------------------------------------------------------------
// Catalog structures — provider-keyed layout matching models.dev
// ---------------------------------------------------------------------------

/// A single provider entry in the catalog, containing provider metadata and
/// its models keyed by model ID.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogProvider {
    /// Provider identifier, e.g. `"openai"`, `"anthropic"`.
    pub id: String,
    /// Human-readable provider name, e.g. `"OpenAI"`.
    pub name: String,
    /// Default environment variable names for the API key.
    #[serde(default)]
    pub env: Vec<String>,
    /// Default base URL for API requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    /// Documentation URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
    /// Models keyed by model ID.
    #[serde(default)]
    pub models: BTreeMap<String, ModelDescriptor>,
}

/// Registry of all known models, organized by provider.
///
/// The JSON representation is a provider-keyed map matching models.dev's
/// top-level layout (filtered to relevant providers). The `caps_overrides`
/// field is loaded separately and not included in the serialized form.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ModelCatalog {
    pub providers: BTreeMap<String, CatalogProvider>,
    pub caps_overrides: CapsOverrides,
}

impl Serialize for ModelCatalog {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.providers.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ModelCatalog {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let providers = BTreeMap::<String, CatalogProvider>::deserialize(deserializer)?;
        Ok(Self {
            providers,
            caps_overrides: CapsOverrides::default(),
        })
    }
}

impl ModelCatalog {
    pub fn new(providers: BTreeMap<String, CatalogProvider>) -> Self {
        Self {
            providers,
            caps_overrides: CapsOverrides::default(),
        }
    }

    /// Returns a new catalog with the given capability overrides applied.
    pub fn with_caps_overrides(mut self, overrides: CapsOverrides) -> Self {
        self.caps_overrides = overrides;
        self
    }

    pub fn from_snapshot_str(snapshot: &str) -> Result<Self, ProviderError> {
        let providers: BTreeMap<String, CatalogProvider> = serde_json::from_str(snapshot)?;
        Ok(Self {
            providers,
            caps_overrides: CapsOverrides::default(),
        })
    }

    pub fn from_pinned_snapshot() -> Result<Self, ProviderError> {
        let catalog = Self::from_snapshot_str(PINNED_MODEL_CATALOG_SNAPSHOT)?;
        let overrides: CapsOverrides = serde_json::from_str(PINNED_CAPS_OVERRIDES)?;
        Ok(catalog.with_caps_overrides(overrides))
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

    /// Returns the compiled-in content of `oxydra_caps_overrides.json`.
    pub fn pinned_overrides_json() -> &'static str {
        PINNED_CAPS_OVERRIDES
    }

    /// Returns the absolute path to the `oxydra_caps_overrides.json` source file.
    pub fn pinned_overrides_path() -> &'static str {
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/data/oxydra_caps_overrides.json"
        )
    }

    /// Canonicalizes and writes a pinned catalog snapshot from already-fetched JSON data.
    ///
    /// This intentionally does not fetch `https://models.dev/api.json`.
    /// Network retrieval is deferred to a later operator-facing refresh path (CLI workflow),
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

    /// Looks up a model descriptor by catalog provider ID and model ID.
    pub fn get(&self, provider: &ProviderId, model: &ModelId) -> Option<&ModelDescriptor> {
        self.providers.get(&provider.0)?.models.get(&model.0)
    }

    /// Validates that a model exists in the catalog, returning an error on miss.
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

    /// Iterates over all models in the catalog as
    /// `(catalog_provider_id, model_id, &ModelDescriptor)` tuples.
    pub fn all_models(&self) -> impl Iterator<Item = (&str, &str, &ModelDescriptor)> {
        self.providers.iter().flat_map(|(provider_id, provider)| {
            provider.models.iter().map(move |(model_id, descriptor)| {
                (provider_id.as_str(), model_id.as_str(), descriptor)
            })
        })
    }
}
