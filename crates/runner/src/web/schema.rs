//! Schema metadata endpoint for the web configurator.
//!
//! Provides field-level metadata (labels, descriptions, input types, defaults,
//! constraints, enum options, dynamic sources) so the frontend can render
//! purpose-built forms instead of generic text inputs.

use std::sync::Arc;

use axum::extract::State;
use axum::response::IntoResponse;
use serde::Serialize;
use serde_json::Value;

use super::response::ok_response;
use super::state::WebState;

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

/// Top-level schema metadata returned by `GET /api/v1/meta/schema`.
#[derive(Debug, Clone, Serialize)]
pub struct ConfigSchemaResponse {
    pub agent: ConfigSchema,
    pub runner: ConfigSchema,
    pub user: ConfigSchema,
    pub dynamic_sources: DynamicSources,
}

/// Schema for one config type, organized by sections.
#[derive(Debug, Clone, Serialize)]
pub struct ConfigSchema {
    pub sections: Vec<SchemaSection>,
}

/// A section groups related fields under a common heading.
#[derive(Debug, Clone, Serialize)]
pub struct SchemaSection {
    pub id: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub fields: Vec<SchemaField>,
    /// Sub-sections (e.g. "Advanced Overrides" inside Providers).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub subsections: Vec<SchemaSection>,
    /// Whether this section represents a dynamic collection (map or array).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collection: Option<CollectionMeta>,
    /// Whether this entire section is optional and toggled on/off.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub optional_section: bool,
}

/// Metadata for collection-type sections (providers, agents, etc.).
#[derive(Debug, Clone, Serialize)]
pub struct CollectionMeta {
    /// `"map"` or `"array"`.
    pub kind: &'static str,
    /// Label for the "Add" button.
    pub add_label: String,
    /// The field used as the entry key (for map collections).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_field: Option<SchemaField>,
}

/// Metadata for a single config field.
#[derive(Debug, Clone, Serialize)]
pub struct SchemaField {
    pub path: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub input_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub required: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub nullable: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_custom: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dynamic_source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enum_options: Option<Vec<EnumOption>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub constraints: Option<FieldConstraints>,
}

/// A single enum option for select/multi_select widgets.
#[derive(Debug, Clone, Serialize)]
pub struct EnumOption {
    pub value: String,
    pub label: String,
}

/// Numeric and string constraints for a field.
#[derive(Debug, Clone, Serialize)]
pub struct FieldConstraints {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step: Option<f64>,
}

/// Dynamic data sources populated at runtime.
#[derive(Debug, Clone, Serialize)]
pub struct DynamicSources {
    pub registered_providers: Vec<String>,
    pub tool_names: Vec<String>,
    pub timezone_suggestions: Vec<String>,
    pub provider_types: Vec<EnumOption>,
    pub sandbox_tiers: Vec<EnumOption>,
    pub auth_modes: Vec<EnumOption>,
    pub embedding_backends: Vec<EnumOption>,
    pub model2vec_models: Vec<EnumOption>,
    pub web_search_providers: Vec<EnumOption>,
    pub input_modalities: Vec<EnumOption>,
    pub safesearch_levels: Vec<EnumOption>,
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

/// `GET /api/v1/meta/schema` — Returns complete field metadata for all config types.
pub async fn get_config_schema(State(state): State<Arc<WebState>>) -> impl IntoResponse {
    let dynamic_sources = build_dynamic_sources(&state);
    let response = ConfigSchemaResponse {
        agent: build_agent_schema(),
        runner: build_runner_schema(),
        user: build_user_schema(),
        dynamic_sources,
    };
    ok_response(response)
}

// ---------------------------------------------------------------------------
// Dynamic sources
// ---------------------------------------------------------------------------

fn build_dynamic_sources(state: &WebState) -> DynamicSources {
    // Load effective agent config to discover registered providers.
    let registered_providers = load_registered_providers(state);

    let tool_names: Vec<String> = tools::canonical_tool_names()
        .into_iter()
        .map(String::from)
        .collect();

    DynamicSources {
        registered_providers,
        tool_names,
        timezone_suggestions: timezone_suggestions(),
        provider_types: vec![
            EnumOption {
                value: "openai".into(),
                label: "OpenAI".into(),
            },
            EnumOption {
                value: "anthropic".into(),
                label: "Anthropic".into(),
            },
            EnumOption {
                value: "gemini".into(),
                label: "Gemini".into(),
            },
            EnumOption {
                value: "openai_responses".into(),
                label: "OpenAI Responses".into(),
            },
        ],
        sandbox_tiers: vec![
            EnumOption {
                value: "micro_vm".into(),
                label: "Micro VM".into(),
            },
            EnumOption {
                value: "container".into(),
                label: "Container".into(),
            },
            EnumOption {
                value: "process".into(),
                label: "Process".into(),
            },
        ],
        auth_modes: vec![
            EnumOption {
                value: "disabled".into(),
                label: "Disabled (loopback only)".into(),
            },
            EnumOption {
                value: "token".into(),
                label: "Bearer Token".into(),
            },
        ],
        embedding_backends: vec![
            EnumOption {
                value: "model2vec".into(),
                label: "Model2Vec".into(),
            },
            EnumOption {
                value: "deterministic".into(),
                label: "Deterministic".into(),
            },
        ],
        model2vec_models: vec![
            EnumOption {
                value: "potion_8m".into(),
                label: "Potion 8M".into(),
            },
            EnumOption {
                value: "potion_32m".into(),
                label: "Potion 32M".into(),
            },
        ],
        web_search_providers: vec![
            EnumOption {
                value: "duckduckgo".into(),
                label: "DuckDuckGo".into(),
            },
            EnumOption {
                value: "google".into(),
                label: "Google".into(),
            },
            EnumOption {
                value: "searxng".into(),
                label: "SearxNG".into(),
            },
        ],
        input_modalities: vec![
            EnumOption {
                value: "image".into(),
                label: "Image".into(),
            },
            EnumOption {
                value: "audio".into(),
                label: "Audio".into(),
            },
            EnumOption {
                value: "video".into(),
                label: "Video".into(),
            },
            EnumOption {
                value: "pdf".into(),
                label: "PDF".into(),
            },
            EnumOption {
                value: "document".into(),
                label: "Document".into(),
            },
        ],
        safesearch_levels: vec![
            EnumOption {
                value: "0".into(),
                label: "Off".into(),
            },
            EnumOption {
                value: "1".into(),
                label: "Moderate".into(),
            },
            EnumOption {
                value: "2".into(),
                label: "Strict".into(),
            },
        ],
    }
}

/// Loads registered provider names from the effective agent config.
/// Falls back to default config provider names on any error.
fn load_registered_providers(state: &WebState) -> Vec<String> {
    // Try loading the effective layered agent config (includes providers.toml).
    let config =
        crate::bootstrap::load_agent_config(None, crate::bootstrap::CliOverrides::default())
            .or_else(|_| {
                // Fallback: try loading agent.toml from the config directory.
                let agent_path = state.config_dir().join("agent.toml");
                if agent_path.exists() {
                    let contents = std::fs::read_to_string(&agent_path).ok();
                    contents
                        .and_then(|c| toml::from_str::<types::AgentConfig>(&c).ok())
                        .ok_or(())
                } else {
                    Ok(types::AgentConfig::default())
                }
            })
            .unwrap_or_default();

    config.providers.registry.keys().cloned().collect()
}

fn timezone_suggestions() -> Vec<String> {
    vec![
        "UTC",
        "America/New_York",
        "America/Chicago",
        "America/Denver",
        "America/Los_Angeles",
        "America/Sao_Paulo",
        "Europe/London",
        "Europe/Paris",
        "Europe/Berlin",
        "Europe/Moscow",
        "Asia/Dubai",
        "Asia/Kolkata",
        "Asia/Shanghai",
        "Asia/Tokyo",
        "Asia/Singapore",
        "Australia/Sydney",
        "Pacific/Auckland",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

// ---------------------------------------------------------------------------
// Agent schema
// ---------------------------------------------------------------------------

fn build_agent_schema() -> ConfigSchema {
    ConfigSchema {
        sections: vec![
            agent_general_section(),
            agent_selection_section(),
            agent_runtime_section(),
            agent_context_budget_section(),
            agent_summarization_section(),
            agent_memory_section(),
            agent_memory_retrieval_section(),
            agent_providers_section(),
            agent_reliability_section(),
            agent_catalog_section(),
            agent_tools_web_search_section(),
            agent_tools_shell_section(),
            agent_tools_attachment_save_section(),
            agent_scheduler_section(),
            agent_gateway_section(),
            agent_agents_section(),
        ],
    }
}

fn agent_general_section() -> SchemaSection {
    SchemaSection {
        id: "general".into(),
        label: "General".into(),
        description: None,
        fields: vec![SchemaField {
            path: "config_version".into(),
            label: "Config Version".into(),
            description: Some("Agent configuration schema version".into()),
            input_type: "readonly",
            default: Some(Value::String("1.0.0".into())),
            required: false,
            nullable: false,
            allow_custom: false,
            dynamic_source: None,
            enum_options: None,
            constraints: None,
        }],
        subsections: vec![],
        collection: None,
        optional_section: false,
    }
}

fn agent_selection_section() -> SchemaSection {
    SchemaSection {
        id: "selection".into(),
        label: "Model Selection".into(),
        description: Some("Choose the LLM provider and model for agent conversations".into()),
        fields: vec![
            SchemaField {
                path: "selection.provider".into(),
                label: "Provider".into(),
                description: Some("The provider registry name to use for LLM API calls".into()),
                input_type: "select_dynamic",
                default: Some(Value::String("openai".into())),
                required: true,
                nullable: false,
                allow_custom: true,
                dynamic_source: Some("registered_providers".into()),
                enum_options: None,
                constraints: None,
            },
            SchemaField {
                path: "selection.model".into(),
                label: "Model".into(),
                description: Some(
                    "The model ID to use. Choose from the catalog or enter a custom model ID."
                        .into(),
                ),
                input_type: "model_picker",
                default: Some(Value::String("gpt-4o-mini".into())),
                required: true,
                nullable: false,
                allow_custom: true,
                dynamic_source: None,
                enum_options: None,
                constraints: None,
            },
        ],
        subsections: vec![],
        collection: None,
        optional_section: false,
    }
}

fn agent_runtime_section() -> SchemaSection {
    SchemaSection {
        id: "runtime".into(),
        label: "Runtime Limits".into(),
        description: Some("Configure execution timeouts, turn limits, and cost caps".into()),
        fields: vec![
            SchemaField {
                path: "runtime.turn_timeout_secs".into(),
                label: "Turn Timeout (seconds)".into(),
                description: Some("Maximum time for a single LLM turn before timeout".into()),
                input_type: "number",
                default: Some(Value::Number(60.into())),
                required: false,
                nullable: false,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: Some(FieldConstraints {
                    min: Some(1.0),
                    max: None,
                    step: None,
                }),
            },
            SchemaField {
                path: "runtime.max_turns".into(),
                label: "Max Turns".into(),
                description: Some("Maximum agentic loop iterations per session".into()),
                input_type: "number",
                default: Some(Value::Number(8.into())),
                required: false,
                nullable: false,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: Some(FieldConstraints {
                    min: Some(1.0),
                    max: None,
                    step: None,
                }),
            },
            SchemaField {
                path: "runtime.max_cost".into(),
                label: "Max Cost ($)".into(),
                description: Some("Maximum USD cost per session. Empty = no limit.".into()),
                input_type: "number",
                default: None,
                required: false,
                nullable: true,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: Some(FieldConstraints {
                    min: Some(0.0),
                    max: None,
                    step: None,
                }),
            },
        ],
        subsections: vec![],
        collection: None,
        optional_section: false,
    }
}

fn agent_context_budget_section() -> SchemaSection {
    SchemaSection {
        id: "runtime.context_budget".into(),
        label: "Context Budget".into(),
        description: Some("Control when and how context window summarization triggers".into()),
        fields: vec![
            SchemaField {
                path: "runtime.context_budget.trigger_ratio".into(),
                label: "Trigger Ratio".into(),
                description: Some(
                    "Context usage ratio that triggers summarization (e.g., 0.85 = 85% full)"
                        .into(),
                ),
                input_type: "number",
                default: Some(serde_json::json!(0.85)),
                required: false,
                nullable: false,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: Some(FieldConstraints {
                    min: Some(0.0),
                    max: Some(1.0),
                    step: Some(0.05),
                }),
            },
            SchemaField {
                path: "runtime.context_budget.safety_buffer_tokens".into(),
                label: "Safety Buffer (tokens)".into(),
                description: Some(
                    "Tokens reserved as safety margin when computing context budget".into(),
                ),
                input_type: "number",
                default: Some(Value::Number(1024.into())),
                required: false,
                nullable: false,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: Some(FieldConstraints {
                    min: Some(1.0),
                    max: None,
                    step: None,
                }),
            },
            SchemaField {
                path: "runtime.context_budget.fallback_max_context_tokens".into(),
                label: "Fallback Max Context".into(),
                description: Some(
                    "Context window size fallback when model catalog has no data".into(),
                ),
                input_type: "number",
                default: Some(Value::Number(128_000.into())),
                required: false,
                nullable: false,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: Some(FieldConstraints {
                    min: Some(1.0),
                    max: None,
                    step: None,
                }),
            },
        ],
        subsections: vec![],
        collection: None,
        optional_section: false,
    }
}

fn agent_summarization_section() -> SchemaSection {
    SchemaSection {
        id: "runtime.summarization".into(),
        label: "Summarization".into(),
        description: Some("Configure context rolling summary behavior".into()),
        fields: vec![
            SchemaField {
                path: "runtime.summarization.target_ratio".into(),
                label: "Target Ratio".into(),
                description: Some(
                    "How much to compress context during summarization (0.0-1.0)".into(),
                ),
                input_type: "number",
                default: Some(serde_json::json!(0.5)),
                required: false,
                nullable: false,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: Some(FieldConstraints {
                    min: Some(0.0),
                    max: Some(1.0),
                    step: Some(0.05),
                }),
            },
            SchemaField {
                path: "runtime.summarization.min_turns".into(),
                label: "Min Turns".into(),
                description: Some(
                    "Minimum conversation turns before summarization can trigger".into(),
                ),
                input_type: "number",
                default: Some(Value::Number(6.into())),
                required: false,
                nullable: false,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: Some(FieldConstraints {
                    min: Some(1.0),
                    max: None,
                    step: None,
                }),
            },
        ],
        subsections: vec![],
        collection: None,
        optional_section: false,
    }
}

fn agent_memory_section() -> SchemaSection {
    SchemaSection {
        id: "memory".into(),
        label: "Memory".into(),
        description: Some("Persistent conversational memory settings".into()),
        fields: vec![
            SchemaField {
                path: "memory.enabled".into(),
                label: "Enable Memory".into(),
                description: Some("Enable persistent conversational memory".into()),
                input_type: "boolean",
                default: Some(Value::Bool(false)),
                required: false,
                nullable: false,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: None,
            },
            SchemaField {
                path: "memory.remote_url".into(),
                label: "Remote URL".into(),
                description: Some(
                    "URL for remote memory storage. Leave empty for local storage.".into(),
                ),
                input_type: "text",
                default: None,
                required: false,
                nullable: true,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: None,
            },
            SchemaField {
                path: "memory.auth_token".into(),
                label: "Auth Token".into(),
                description: Some("Authentication token for remote memory server".into()),
                input_type: "secret",
                default: None,
                required: false,
                nullable: true,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: None,
            },
            SchemaField {
                path: "memory.embedding_backend".into(),
                label: "Embedding Backend".into(),
                description: Some("Backend for generating vector embeddings".into()),
                input_type: "select",
                default: Some(Value::String("model2vec".into())),
                required: false,
                nullable: false,
                allow_custom: false,
                dynamic_source: None,
                enum_options: Some(vec![
                    EnumOption {
                        value: "model2vec".into(),
                        label: "Model2Vec".into(),
                    },
                    EnumOption {
                        value: "deterministic".into(),
                        label: "Deterministic".into(),
                    },
                ]),
                constraints: None,
            },
            SchemaField {
                path: "memory.model2vec_model".into(),
                label: "Model2Vec Model".into(),
                description: Some("Model size for Model2Vec embeddings".into()),
                input_type: "select",
                default: Some(Value::String("potion_32m".into())),
                required: false,
                nullable: false,
                allow_custom: false,
                dynamic_source: None,
                enum_options: Some(vec![
                    EnumOption {
                        value: "potion_8m".into(),
                        label: "Potion 8M".into(),
                    },
                    EnumOption {
                        value: "potion_32m".into(),
                        label: "Potion 32M".into(),
                    },
                ]),
                constraints: None,
            },
        ],
        subsections: vec![],
        collection: None,
        optional_section: false,
    }
}

fn agent_memory_retrieval_section() -> SchemaSection {
    SchemaSection {
        id: "memory.retrieval".into(),
        label: "Memory Retrieval".into(),
        description: Some("Configure how memory entries are searched and ranked".into()),
        fields: vec![
            SchemaField {
                path: "memory.retrieval.top_k".into(),
                label: "Top K Results".into(),
                description: Some("Number of most relevant memory entries to retrieve".into()),
                input_type: "number",
                default: Some(Value::Number(8.into())),
                required: false,
                nullable: false,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: Some(FieldConstraints {
                    min: Some(1.0),
                    max: None,
                    step: None,
                }),
            },
            SchemaField {
                path: "memory.retrieval.vector_weight".into(),
                label: "Vector Weight".into(),
                description: Some(
                    "Weight for vector similarity search (0.0-1.0). Vector + FTS must sum to 1.0."
                        .into(),
                ),
                input_type: "number",
                default: Some(serde_json::json!(0.7)),
                required: false,
                nullable: false,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: Some(FieldConstraints {
                    min: Some(0.0),
                    max: Some(1.0),
                    step: Some(0.1),
                }),
            },
            SchemaField {
                path: "memory.retrieval.fts_weight".into(),
                label: "Full-Text Weight".into(),
                description: Some(
                    "Weight for full-text search (0.0-1.0). Vector + FTS must sum to 1.0.".into(),
                ),
                input_type: "number",
                default: Some(serde_json::json!(0.3)),
                required: false,
                nullable: false,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: Some(FieldConstraints {
                    min: Some(0.0),
                    max: Some(1.0),
                    step: Some(0.1),
                }),
            },
        ],
        subsections: vec![],
        collection: None,
        optional_section: false,
    }
}

fn agent_providers_section() -> SchemaSection {
    SchemaSection {
        id: "providers".into(),
        label: "Providers".into(),
        description: Some(
            "LLM provider configurations. Each entry maps a name to connection details.".into(),
        ),
        fields: vec![
            SchemaField {
                path: "provider_type".into(),
                label: "Provider Type".into(),
                description: Some("LLM API protocol implementation".into()),
                input_type: "select",
                default: Some(Value::String("openai".into())),
                required: true,
                nullable: false,
                allow_custom: false,
                dynamic_source: None,
                enum_options: Some(vec![
                    EnumOption {
                        value: "openai".into(),
                        label: "OpenAI".into(),
                    },
                    EnumOption {
                        value: "anthropic".into(),
                        label: "Anthropic".into(),
                    },
                    EnumOption {
                        value: "gemini".into(),
                        label: "Gemini".into(),
                    },
                    EnumOption {
                        value: "openai_responses".into(),
                        label: "OpenAI Responses".into(),
                    },
                ]),
                constraints: None,
            },
            SchemaField {
                path: "base_url".into(),
                label: "Base URL".into(),
                description: Some("Custom API base URL. Leave empty for provider default.".into()),
                input_type: "text",
                default: None,
                required: false,
                nullable: true,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: None,
            },
            SchemaField {
                path: "api_key".into(),
                label: "API Key".into(),
                description: Some("Direct API key. Prefer environment variable instead.".into()),
                input_type: "secret",
                default: None,
                required: false,
                nullable: true,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: None,
            },
            SchemaField {
                path: "api_key_env".into(),
                label: "API Key Env Variable".into(),
                description: Some("Environment variable name containing the API key".into()),
                input_type: "text",
                default: None,
                required: false,
                nullable: true,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: None,
            },
            SchemaField {
                path: "extra_headers".into(),
                label: "Extra Headers".into(),
                description: Some("Additional HTTP headers sent with every request".into()),
                input_type: "key_value_map",
                default: None,
                required: false,
                nullable: true,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: None,
            },
            SchemaField {
                path: "catalog_provider".into(),
                label: "Catalog Provider".into(),
                description: Some(
                    "Model catalog namespace for validation. Auto-detected from provider type."
                        .into(),
                ),
                input_type: "text",
                default: None,
                required: false,
                nullable: true,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: None,
            },
        ],
        subsections: vec![SchemaSection {
            id: "providers.advanced".into(),
            label: "Advanced Overrides".into(),
            description: Some(
                "Capability overrides for custom/proxy providers with models not in the catalog"
                    .into(),
            ),
            fields: vec![
                SchemaField {
                    path: "attachment".into(),
                    label: "Supports Attachments".into(),
                    description: Some(
                        "Override: whether this provider handles file/image attachments".into(),
                    ),
                    input_type: "boolean",
                    default: None,
                    required: false,
                    nullable: true,
                    allow_custom: false,
                    dynamic_source: None,
                    enum_options: None,
                    constraints: None,
                },
                SchemaField {
                    path: "input_modalities".into(),
                    label: "Input Modalities".into(),
                    description: Some("Override: accepted input types".into()),
                    input_type: "multi_select",
                    default: None,
                    required: false,
                    nullable: true,
                    allow_custom: false,
                    dynamic_source: Some("input_modalities".into()),
                    enum_options: None,
                    constraints: None,
                },
                SchemaField {
                    path: "reasoning".into(),
                    label: "Supports Reasoning".into(),
                    description: Some("Override: whether models support reasoning/thinking".into()),
                    input_type: "boolean",
                    default: None,
                    required: false,
                    nullable: true,
                    allow_custom: false,
                    dynamic_source: None,
                    enum_options: None,
                    constraints: None,
                },
                SchemaField {
                    path: "max_input_tokens".into(),
                    label: "Max Input Tokens".into(),
                    description: Some("Override: maximum input token limit".into()),
                    input_type: "number",
                    default: None,
                    required: false,
                    nullable: true,
                    allow_custom: false,
                    dynamic_source: None,
                    enum_options: None,
                    constraints: Some(FieldConstraints {
                        min: Some(1.0),
                        max: None,
                        step: None,
                    }),
                },
                SchemaField {
                    path: "max_output_tokens".into(),
                    label: "Max Output Tokens".into(),
                    description: Some("Override: maximum output token limit".into()),
                    input_type: "number",
                    default: None,
                    required: false,
                    nullable: true,
                    allow_custom: false,
                    dynamic_source: None,
                    enum_options: None,
                    constraints: Some(FieldConstraints {
                        min: Some(1.0),
                        max: None,
                        step: None,
                    }),
                },
                SchemaField {
                    path: "max_context_tokens".into(),
                    label: "Max Context Tokens".into(),
                    description: Some("Override: maximum context window size".into()),
                    input_type: "number",
                    default: None,
                    required: false,
                    nullable: true,
                    allow_custom: false,
                    dynamic_source: None,
                    enum_options: None,
                    constraints: Some(FieldConstraints {
                        min: Some(1.0),
                        max: None,
                        step: None,
                    }),
                },
            ],
            subsections: vec![],
            collection: None,
            optional_section: false,
        }],
        collection: Some(CollectionMeta {
            kind: "map",
            add_label: "Add Provider".into(),
            key_field: Some(SchemaField {
                path: "_key".into(),
                label: "Provider Name".into(),
                description: Some("Unique name for this provider configuration".into()),
                input_type: "text",
                default: None,
                required: true,
                nullable: false,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: None,
            }),
        }),
        optional_section: false,
    }
}

fn agent_reliability_section() -> SchemaSection {
    SchemaSection {
        id: "reliability".into(),
        label: "Reliability".into(),
        description: Some("Retry and backoff settings for failed API calls".into()),
        fields: vec![
            SchemaField {
                path: "reliability.max_attempts".into(),
                label: "Max Retry Attempts".into(),
                description: Some("Number of retry attempts for failed API calls".into()),
                input_type: "number",
                default: Some(Value::Number(3.into())),
                required: false,
                nullable: false,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: Some(FieldConstraints {
                    min: Some(1.0),
                    max: None,
                    step: None,
                }),
            },
            SchemaField {
                path: "reliability.backoff_base_ms".into(),
                label: "Backoff Base (ms)".into(),
                description: Some("Base delay for exponential backoff".into()),
                input_type: "number",
                default: Some(Value::Number(250.into())),
                required: false,
                nullable: false,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: Some(FieldConstraints {
                    min: Some(1.0),
                    max: None,
                    step: None,
                }),
            },
            SchemaField {
                path: "reliability.backoff_max_ms".into(),
                label: "Backoff Max (ms)".into(),
                description: Some("Maximum backoff delay. Must be ≥ base.".into()),
                input_type: "number",
                default: Some(Value::Number(2000.into())),
                required: false,
                nullable: false,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: Some(FieldConstraints {
                    min: Some(1.0),
                    max: None,
                    step: None,
                }),
            },
            SchemaField {
                path: "reliability.jitter".into(),
                label: "Enable Jitter".into(),
                description: Some("Add random jitter to backoff to prevent thundering herd".into()),
                input_type: "boolean",
                default: Some(Value::Bool(false)),
                required: false,
                nullable: false,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: None,
            },
        ],
        subsections: vec![],
        collection: None,
        optional_section: false,
    }
}

fn agent_catalog_section() -> SchemaSection {
    SchemaSection {
        id: "catalog".into(),
        label: "Catalog".into(),
        description: Some("Model catalog validation and refresh settings".into()),
        fields: vec![
            SchemaField {
                path: "catalog.skip_catalog_validation".into(),
                label: "Skip Catalog Validation".into(),
                description: Some(
                    "Allow models not found in the catalog. Enable for custom/proxy providers."
                        .into(),
                ),
                input_type: "boolean",
                default: Some(Value::Bool(false)),
                required: false,
                nullable: false,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: None,
            },
            SchemaField {
                path: "catalog.pinned_url".into(),
                label: "Pinned Catalog URL".into(),
                description: Some(
                    "URL for fetching the model catalog snapshot. Leave empty for default.".into(),
                ),
                input_type: "text",
                default: None,
                required: false,
                nullable: true,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: None,
            },
        ],
        subsections: vec![],
        collection: None,
        optional_section: false,
    }
}

fn agent_tools_web_search_section() -> SchemaSection {
    SchemaSection {
        id: "tools.web_search".into(),
        label: "Tools — Web Search".into(),
        description: Some("Web search provider configuration".into()),
        optional_section: true,
        fields: vec![
            SchemaField {
                path: "tools.web_search.provider".into(),
                label: "Search Provider".into(),
                description: Some("Web search backend".into()),
                input_type: "select",
                default: None,
                required: false,
                nullable: true,
                allow_custom: false,
                dynamic_source: None,
                enum_options: Some(vec![
                    EnumOption {
                        value: "duckduckgo".into(),
                        label: "DuckDuckGo".into(),
                    },
                    EnumOption {
                        value: "google".into(),
                        label: "Google".into(),
                    },
                    EnumOption {
                        value: "searxng".into(),
                        label: "SearxNG".into(),
                    },
                ]),
                constraints: None,
            },
            SchemaField {
                path: "tools.web_search.base_url".into(),
                label: "Base URL".into(),
                description: Some("Custom base URL for the search provider".into()),
                input_type: "text",
                default: None,
                required: false,
                nullable: true,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: None,
            },
            SchemaField {
                path: "tools.web_search.base_urls".into(),
                label: "Fallback URLs".into(),
                description: Some("Comma-separated fallback base URLs".into()),
                input_type: "text",
                default: None,
                required: false,
                nullable: true,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: None,
            },
            SchemaField {
                path: "tools.web_search.api_key_env".into(),
                label: "API Key Env".into(),
                description: Some("Env var name for Google API key".into()),
                input_type: "text",
                default: None,
                required: false,
                nullable: true,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: None,
            },
            SchemaField {
                path: "tools.web_search.engine_id_env".into(),
                label: "Engine ID Env".into(),
                description: Some("Env var name for Google Custom Search engine ID".into()),
                input_type: "text",
                default: None,
                required: false,
                nullable: true,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: None,
            },
            SchemaField {
                path: "tools.web_search.query_params".into(),
                label: "Extra Query Params".into(),
                description: Some("Additional parameters as `key=value&key2=value2`".into()),
                input_type: "text",
                default: None,
                required: false,
                nullable: true,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: None,
            },
            SchemaField {
                path: "tools.web_search.engines".into(),
                label: "SearxNG Engines".into(),
                description: Some("Comma-separated SearxNG search engines".into()),
                input_type: "text",
                default: None,
                required: false,
                nullable: true,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: None,
            },
            SchemaField {
                path: "tools.web_search.categories".into(),
                label: "SearxNG Categories".into(),
                description: Some("Comma-separated SearxNG categories".into()),
                input_type: "text",
                default: None,
                required: false,
                nullable: true,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: None,
            },
            SchemaField {
                path: "tools.web_search.safesearch".into(),
                label: "Safe Search Level".into(),
                description: Some("SearxNG safe search level".into()),
                input_type: "select",
                default: None,
                required: false,
                nullable: true,
                allow_custom: false,
                dynamic_source: None,
                enum_options: Some(vec![
                    EnumOption {
                        value: "0".into(),
                        label: "Off".into(),
                    },
                    EnumOption {
                        value: "1".into(),
                        label: "Moderate".into(),
                    },
                    EnumOption {
                        value: "2".into(),
                        label: "Strict".into(),
                    },
                ]),
                constraints: None,
            },
            SchemaField {
                path: "tools.web_search.egress_allowlist".into(),
                label: "Egress Allowlist".into(),
                description: Some("Hosts allowed to resolve to private/loopback addresses".into()),
                input_type: "tag_list",
                default: None,
                required: false,
                nullable: true,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: None,
            },
        ],
        subsections: vec![],
        collection: None,
    }
}

fn agent_tools_shell_section() -> SchemaSection {
    SchemaSection {
        id: "tools.shell".into(),
        label: "Tools — Shell".into(),
        description: Some("Shell command execution security policy".into()),
        optional_section: true,
        fields: vec![
            SchemaField {
                path: "tools.shell.allow".into(),
                label: "Allow Commands".into(),
                description: Some("Additional commands/patterns to add to the allowlist".into()),
                input_type: "tag_list",
                default: None,
                required: false,
                nullable: true,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: None,
            },
            SchemaField {
                path: "tools.shell.deny".into(),
                label: "Deny Commands".into(),
                description: Some(
                    "Commands/patterns to remove from the allowlist (deny wins)".into(),
                ),
                input_type: "tag_list",
                default: None,
                required: false,
                nullable: true,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: None,
            },
            SchemaField {
                path: "tools.shell.replace_defaults".into(),
                label: "Replace Default Allowlist".into(),
                description: Some(
                    "If enabled, `allow` replaces the built-in list instead of extending it".into(),
                ),
                input_type: "boolean",
                default: Some(Value::Bool(false)),
                required: false,
                nullable: true,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: None,
            },
            SchemaField {
                path: "tools.shell.allow_operators".into(),
                label: "Allow Shell Operators".into(),
                description: Some("Allow shell control operators (&&, ||, |, etc.)".into()),
                input_type: "boolean",
                default: Some(Value::Bool(false)),
                required: false,
                nullable: true,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: None,
            },
            SchemaField {
                path: "tools.shell.env_keys".into(),
                label: "Environment Variables".into(),
                description: Some("Env var names to forward into the shell container".into()),
                input_type: "tag_list",
                default: None,
                required: false,
                nullable: true,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: None,
            },
        ],
        subsections: vec![],
        collection: None,
    }
}

fn agent_tools_attachment_save_section() -> SchemaSection {
    SchemaSection {
        id: "tools.attachment_save".into(),
        label: "Tools — Attachment Save".into(),
        description: Some("File save operation settings".into()),
        optional_section: true,
        fields: vec![SchemaField {
            path: "tools.attachment_save.timeout_secs".into(),
            label: "Save Timeout (seconds)".into(),
            description: Some("Timeout for file save operations".into()),
            input_type: "number",
            default: Some(Value::Number(60.into())),
            required: false,
            nullable: false,
            allow_custom: false,
            dynamic_source: None,
            enum_options: None,
            constraints: Some(FieldConstraints {
                min: Some(1.0),
                max: None,
                step: None,
            }),
        }],
        subsections: vec![],
        collection: None,
    }
}

fn agent_scheduler_section() -> SchemaSection {
    SchemaSection {
        id: "scheduler".into(),
        label: "Scheduler".into(),
        description: Some("Scheduled and recurring task execution settings".into()),
        fields: vec![
            SchemaField {
                path: "scheduler.enabled".into(),
                label: "Enable Scheduler".into(),
                description: Some("Enable scheduled/recurring task execution".into()),
                input_type: "boolean",
                default: Some(Value::Bool(false)),
                required: false,
                nullable: false,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: None,
            },
            SchemaField {
                path: "scheduler.poll_interval_secs".into(),
                label: "Poll Interval (seconds)".into(),
                description: Some("How often the executor checks for due schedules".into()),
                input_type: "number",
                default: Some(Value::Number(15.into())),
                required: false,
                nullable: false,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: Some(FieldConstraints {
                    min: Some(1.0),
                    max: None,
                    step: None,
                }),
            },
            SchemaField {
                path: "scheduler.max_concurrent".into(),
                label: "Max Concurrent".into(),
                description: Some("Maximum simultaneous scheduled executions".into()),
                input_type: "number",
                default: Some(Value::Number(2.into())),
                required: false,
                nullable: false,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: Some(FieldConstraints {
                    min: Some(1.0),
                    max: None,
                    step: None,
                }),
            },
            SchemaField {
                path: "scheduler.max_schedules_per_user".into(),
                label: "Max Schedules Per User".into(),
                description: Some("Maximum number of schedules a user can create".into()),
                input_type: "number",
                default: Some(Value::Number(50.into())),
                required: false,
                nullable: false,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: Some(FieldConstraints {
                    min: Some(1.0),
                    max: None,
                    step: None,
                }),
            },
            SchemaField {
                path: "scheduler.max_turns".into(),
                label: "Max Turns Per Run".into(),
                description: Some("Maximum turns for each scheduled execution".into()),
                input_type: "number",
                default: Some(Value::Number(10.into())),
                required: false,
                nullable: false,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: Some(FieldConstraints {
                    min: Some(1.0),
                    max: None,
                    step: None,
                }),
            },
            SchemaField {
                path: "scheduler.max_cost".into(),
                label: "Max Cost Per Run ($)".into(),
                description: Some("Maximum cost per scheduled execution".into()),
                input_type: "number",
                default: Some(serde_json::json!(0.50)),
                required: false,
                nullable: false,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: Some(FieldConstraints {
                    min: Some(0.0),
                    max: None,
                    step: None,
                }),
            },
            SchemaField {
                path: "scheduler.max_run_history".into(),
                label: "Max Run History".into(),
                description: Some("History entries to keep per schedule".into()),
                input_type: "number",
                default: Some(Value::Number(20.into())),
                required: false,
                nullable: false,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: Some(FieldConstraints {
                    min: Some(1.0),
                    max: None,
                    step: None,
                }),
            },
            SchemaField {
                path: "scheduler.min_interval_secs".into(),
                label: "Min Interval (seconds)".into(),
                description: Some("Minimum time between runs (anti-abuse)".into()),
                input_type: "number",
                default: Some(Value::Number(60.into())),
                required: false,
                nullable: false,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: Some(FieldConstraints {
                    min: Some(1.0),
                    max: None,
                    step: None,
                }),
            },
            SchemaField {
                path: "scheduler.default_timezone".into(),
                label: "Default Timezone".into(),
                description: Some("Timezone for cron schedule interpretation".into()),
                input_type: "text",
                default: Some(Value::String("Asia/Kolkata".into())),
                required: false,
                nullable: false,
                allow_custom: false,
                dynamic_source: Some("timezone_suggestions".into()),
                enum_options: None,
                constraints: None,
            },
            SchemaField {
                path: "scheduler.auto_disable_after_failures".into(),
                label: "Auto-Disable After Failures".into(),
                description: Some("Consecutive failures before auto-disabling a schedule".into()),
                input_type: "number",
                default: Some(Value::Number(5.into())),
                required: false,
                nullable: false,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: Some(FieldConstraints {
                    min: Some(1.0),
                    max: None,
                    step: None,
                }),
            },
            SchemaField {
                path: "scheduler.notify_after_failures".into(),
                label: "Notify After Failures".into(),
                description: Some(
                    "Consecutive failures before notifying the user. 0 = disabled.".into(),
                ),
                input_type: "number",
                default: Some(Value::Number(3.into())),
                required: false,
                nullable: false,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: Some(FieldConstraints {
                    min: Some(0.0),
                    max: None,
                    step: None,
                }),
            },
        ],
        subsections: vec![],
        collection: None,
        optional_section: false,
    }
}

fn agent_gateway_section() -> SchemaSection {
    SchemaSection {
        id: "gateway".into(),
        label: "Gateway".into(),
        description: Some("Session and concurrency limits for the agent gateway".into()),
        fields: vec![
            SchemaField {
                path: "gateway.max_sessions_per_user".into(),
                label: "Max Sessions".into(),
                description: Some("Maximum concurrent sessions per user".into()),
                input_type: "number",
                default: Some(Value::Number(50.into())),
                required: false,
                nullable: false,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: Some(FieldConstraints {
                    min: Some(1.0),
                    max: None,
                    step: None,
                }),
            },
            SchemaField {
                path: "gateway.max_concurrent_turns_per_user".into(),
                label: "Max Concurrent Turns".into(),
                description: Some("Maximum simultaneous turns per user".into()),
                input_type: "number",
                default: Some(Value::Number(10.into())),
                required: false,
                nullable: false,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: Some(FieldConstraints {
                    min: Some(1.0),
                    max: None,
                    step: None,
                }),
            },
            SchemaField {
                path: "gateway.session_idle_ttl_hours".into(),
                label: "Session Idle TTL (hours)".into(),
                description: Some("Hours before an idle session is cleaned up".into()),
                input_type: "number",
                default: Some(Value::Number(48.into())),
                required: false,
                nullable: false,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: Some(FieldConstraints {
                    min: Some(1.0),
                    max: None,
                    step: None,
                }),
            },
        ],
        subsections: vec![],
        collection: None,
        optional_section: false,
    }
}

fn agent_agents_section() -> SchemaSection {
    SchemaSection {
        id: "agents".into(),
        label: "Agent Definitions".into(),
        description: Some(
            "Named agent definitions with optional provider/model overrides and tool restrictions"
                .into(),
        ),
        fields: vec![
            SchemaField {
                path: "system_prompt".into(),
                label: "System Prompt".into(),
                description: Some(
                    "Custom system prompt text. Overrides default agent behavior.".into(),
                ),
                input_type: "multiline",
                default: None,
                required: false,
                nullable: true,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: None,
            },
            SchemaField {
                path: "system_prompt_file".into(),
                label: "System Prompt File".into(),
                description: Some(
                    "Path to a file containing the system prompt. Alternative to inline.".into(),
                ),
                input_type: "text",
                default: None,
                required: false,
                nullable: true,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: None,
            },
            SchemaField {
                path: "selection.provider".into(),
                label: "Provider Override".into(),
                description: Some("Use a different provider for this agent".into()),
                input_type: "select_dynamic",
                default: None,
                required: false,
                nullable: true,
                allow_custom: true,
                dynamic_source: Some("registered_providers".into()),
                enum_options: None,
                constraints: None,
            },
            SchemaField {
                path: "selection.model".into(),
                label: "Model Override".into(),
                description: Some("Use a different model for this agent".into()),
                input_type: "model_picker",
                default: None,
                required: false,
                nullable: true,
                allow_custom: true,
                dynamic_source: None,
                enum_options: None,
                constraints: None,
            },
            SchemaField {
                path: "tools".into(),
                label: "Allowed Tools".into(),
                description: Some(
                    "Restrict this agent to specific tools. Empty = all tools.".into(),
                ),
                input_type: "multi_select",
                default: None,
                required: false,
                nullable: true,
                allow_custom: false,
                dynamic_source: Some("tool_names".into()),
                enum_options: None,
                constraints: None,
            },
            SchemaField {
                path: "max_turns".into(),
                label: "Max Turns Override".into(),
                description: Some("Override the global max turns for this agent".into()),
                input_type: "number",
                default: None,
                required: false,
                nullable: true,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: Some(FieldConstraints {
                    min: Some(1.0),
                    max: None,
                    step: None,
                }),
            },
            SchemaField {
                path: "max_cost".into(),
                label: "Max Cost Override ($)".into(),
                description: Some("Override the global max cost for this agent".into()),
                input_type: "number",
                default: None,
                required: false,
                nullable: true,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: Some(FieldConstraints {
                    min: Some(0.0),
                    max: None,
                    step: None,
                }),
            },
        ],
        subsections: vec![],
        collection: Some(CollectionMeta {
            kind: "map",
            add_label: "Add Agent".into(),
            key_field: Some(SchemaField {
                path: "_key".into(),
                label: "Agent Name".into(),
                description: Some("Unique identifier for this agent definition".into()),
                input_type: "text",
                default: None,
                required: true,
                nullable: false,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: None,
            }),
        }),
        optional_section: false,
    }
}

// ---------------------------------------------------------------------------
// Runner schema
// ---------------------------------------------------------------------------

fn build_runner_schema() -> ConfigSchema {
    ConfigSchema {
        sections: vec![
            runner_general_section(),
            runner_guest_images_section(),
            runner_web_section(),
        ],
    }
}

fn runner_general_section() -> SchemaSection {
    SchemaSection {
        id: "general".into(),
        label: "General".into(),
        description: None,
        fields: vec![
            SchemaField {
                path: "config_version".into(),
                label: "Config Version".into(),
                description: Some("Runner configuration schema version".into()),
                input_type: "readonly",
                default: Some(Value::String("1.0.1".into())),
                required: false,
                nullable: false,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: None,
            },
            SchemaField {
                path: "workspace_root".into(),
                label: "Workspace Root".into(),
                description: Some("Directory for user workspace data".into()),
                input_type: "text",
                default: Some(Value::String(".oxydra/workspaces".into())),
                required: true,
                nullable: false,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: None,
            },
            SchemaField {
                path: "default_tier".into(),
                label: "Default Sandbox Tier".into(),
                description: Some("Default isolation level for new users".into()),
                input_type: "select",
                default: Some(Value::String("micro_vm".into())),
                required: false,
                nullable: false,
                allow_custom: false,
                dynamic_source: None,
                enum_options: Some(vec![
                    EnumOption {
                        value: "micro_vm".into(),
                        label: "Micro VM".into(),
                    },
                    EnumOption {
                        value: "container".into(),
                        label: "Container".into(),
                    },
                    EnumOption {
                        value: "process".into(),
                        label: "Process".into(),
                    },
                ]),
                constraints: None,
            },
        ],
        subsections: vec![],
        collection: None,
        optional_section: false,
    }
}

fn runner_guest_images_section() -> SchemaSection {
    SchemaSection {
        id: "guest_images".into(),
        label: "Guest Images".into(),
        description: Some("Container and VM image references".into()),
        fields: vec![
            SchemaField {
                path: "guest_images.oxydra_vm".into(),
                label: "Oxydra VM Image".into(),
                description: Some("Container/VM image for the oxydra runtime".into()),
                input_type: "text",
                default: Some(Value::String("oxydra-vm:latest".into())),
                required: true,
                nullable: false,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: None,
            },
            SchemaField {
                path: "guest_images.shell_vm".into(),
                label: "Shell VM Image".into(),
                description: Some("Container/VM image for shell execution".into()),
                input_type: "text",
                default: Some(Value::String("shell-vm:latest".into())),
                required: true,
                nullable: false,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: None,
            },
            SchemaField {
                path: "guest_images.firecracker_oxydra_vm_config".into(),
                label: "Firecracker Oxydra Config".into(),
                description: Some("JSON config file path for microvm (Linux only)".into()),
                input_type: "text",
                default: None,
                required: false,
                nullable: true,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: None,
            },
            SchemaField {
                path: "guest_images.firecracker_shell_vm_config".into(),
                label: "Firecracker Shell Config".into(),
                description: Some("JSON config file path for microvm (Linux only)".into()),
                input_type: "text",
                default: None,
                required: false,
                nullable: true,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: None,
            },
        ],
        subsections: vec![],
        collection: None,
        optional_section: false,
    }
}

fn runner_web_section() -> SchemaSection {
    SchemaSection {
        id: "web".into(),
        label: "Web Configurator".into(),
        description: Some("Embedded web UI settings".into()),
        fields: vec![
            SchemaField {
                path: "web.enabled".into(),
                label: "Enable Web Configurator".into(),
                description: Some("Whether the web UI is available".into()),
                input_type: "boolean",
                default: Some(Value::Bool(true)),
                required: false,
                nullable: false,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: None,
            },
            SchemaField {
                path: "web.bind".into(),
                label: "Bind Address".into(),
                description: Some(
                    "IP address and port for the web server (e.g., 127.0.0.1:9400)".into(),
                ),
                input_type: "text",
                default: Some(Value::String("127.0.0.1:9400".into())),
                required: true,
                nullable: false,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: None,
            },
            SchemaField {
                path: "web.auth_mode".into(),
                label: "Authentication Mode".into(),
                description: Some("How to protect the web interface".into()),
                input_type: "select",
                default: Some(Value::String("disabled".into())),
                required: false,
                nullable: false,
                allow_custom: false,
                dynamic_source: None,
                enum_options: Some(vec![
                    EnumOption {
                        value: "disabled".into(),
                        label: "Disabled (loopback only)".into(),
                    },
                    EnumOption {
                        value: "token".into(),
                        label: "Bearer Token".into(),
                    },
                ]),
                constraints: None,
            },
            SchemaField {
                path: "web.auth_token_env".into(),
                label: "Auth Token Env Variable".into(),
                description: Some("Environment variable containing the bearer token".into()),
                input_type: "text",
                default: None,
                required: false,
                nullable: true,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: None,
            },
            SchemaField {
                path: "web.auth_token".into(),
                label: "Auth Token (inline)".into(),
                description: Some(
                    "Inline bearer token. Prefer using an environment variable.".into(),
                ),
                input_type: "secret",
                default: None,
                required: false,
                nullable: true,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: None,
            },
        ],
        subsections: vec![],
        collection: None,
        optional_section: false,
    }
}

// ---------------------------------------------------------------------------
// User schema
// ---------------------------------------------------------------------------

fn build_user_schema() -> ConfigSchema {
    ConfigSchema {
        sections: vec![
            user_general_section(),
            user_mounts_section(),
            user_resources_section(),
            user_credential_refs_section(),
            user_behavior_section(),
            user_telegram_section(),
        ],
    }
}

fn user_general_section() -> SchemaSection {
    SchemaSection {
        id: "general".into(),
        label: "General".into(),
        description: None,
        fields: vec![SchemaField {
            path: "config_version".into(),
            label: "Config Version".into(),
            description: Some("User configuration schema version".into()),
            input_type: "readonly",
            default: Some(Value::String("1.0.1".into())),
            required: false,
            nullable: false,
            allow_custom: false,
            dynamic_source: None,
            enum_options: None,
            constraints: None,
        }],
        subsections: vec![],
        collection: None,
        optional_section: false,
    }
}

fn user_mounts_section() -> SchemaSection {
    SchemaSection {
        id: "mounts".into(),
        label: "Mount Paths".into(),
        description: Some("Override workspace mount paths for this user".into()),
        fields: vec![
            SchemaField {
                path: "mounts.shared".into(),
                label: "Shared Mount".into(),
                description: Some("Path for shared workspace data".into()),
                input_type: "text",
                default: None,
                required: false,
                nullable: true,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: None,
            },
            SchemaField {
                path: "mounts.tmp".into(),
                label: "Temp Mount".into(),
                description: Some("Path for temporary files".into()),
                input_type: "text",
                default: None,
                required: false,
                nullable: true,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: None,
            },
            SchemaField {
                path: "mounts.vault".into(),
                label: "Vault Mount".into(),
                description: Some("Path for secure credential storage".into()),
                input_type: "text",
                default: None,
                required: false,
                nullable: true,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: None,
            },
        ],
        subsections: vec![],
        collection: None,
        optional_section: false,
    }
}

fn user_resources_section() -> SchemaSection {
    SchemaSection {
        id: "resources".into(),
        label: "Resource Limits".into(),
        description: Some("Maximum compute resources for this user".into()),
        fields: vec![
            SchemaField {
                path: "resources.max_vcpus".into(),
                label: "Max vCPUs".into(),
                description: Some("Maximum virtual CPU cores. Empty = no limit.".into()),
                input_type: "number",
                default: None,
                required: false,
                nullable: true,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: Some(FieldConstraints {
                    min: Some(1.0),
                    max: None,
                    step: None,
                }),
            },
            SchemaField {
                path: "resources.max_memory_mib".into(),
                label: "Max Memory (MiB)".into(),
                description: Some("Maximum memory in mebibytes. Empty = no limit.".into()),
                input_type: "number",
                default: None,
                required: false,
                nullable: true,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: Some(FieldConstraints {
                    min: Some(1.0),
                    max: None,
                    step: None,
                }),
            },
            SchemaField {
                path: "resources.max_processes".into(),
                label: "Max Processes".into(),
                description: Some("Maximum concurrent processes. Empty = no limit.".into()),
                input_type: "number",
                default: None,
                required: false,
                nullable: true,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: Some(FieldConstraints {
                    min: Some(1.0),
                    max: None,
                    step: None,
                }),
            },
        ],
        subsections: vec![],
        collection: None,
        optional_section: false,
    }
}

fn user_credential_refs_section() -> SchemaSection {
    SchemaSection {
        id: "credential_refs".into(),
        label: "Credential References".into(),
        description: Some("Credential key-value pairs available to the agent".into()),
        fields: vec![
            SchemaField {
                path: "_key".into(),
                label: "Credential Name".into(),
                description: Some("Identifier for this credential".into()),
                input_type: "text",
                default: None,
                required: true,
                nullable: false,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: None,
            },
            SchemaField {
                path: "_value".into(),
                label: "Credential Value".into(),
                description: Some("The credential secret value".into()),
                input_type: "secret",
                default: None,
                required: true,
                nullable: false,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: None,
            },
        ],
        subsections: vec![],
        collection: Some(CollectionMeta {
            kind: "map",
            add_label: "Add Credential".into(),
            key_field: None, // key_field is embedded in fields above
        }),
        optional_section: false,
    }
}

fn user_behavior_section() -> SchemaSection {
    SchemaSection {
        id: "behavior".into(),
        label: "Behavior Overrides".into(),
        description: Some("Override default sandbox and tool access settings for this user".into()),
        fields: vec![
            SchemaField {
                path: "behavior.sandbox_tier".into(),
                label: "Sandbox Tier Override".into(),
                description: Some("Override the default sandbox tier for this user".into()),
                input_type: "select",
                default: None,
                required: false,
                nullable: true,
                allow_custom: false,
                dynamic_source: None,
                enum_options: Some(vec![
                    EnumOption {
                        value: "micro_vm".into(),
                        label: "Micro VM".into(),
                    },
                    EnumOption {
                        value: "container".into(),
                        label: "Container".into(),
                    },
                    EnumOption {
                        value: "process".into(),
                        label: "Process".into(),
                    },
                ]),
                constraints: None,
            },
            SchemaField {
                path: "behavior.shell_enabled".into(),
                label: "Shell Access".into(),
                description: Some("Whether this user's agent can use the shell tool".into()),
                input_type: "boolean",
                default: None,
                required: false,
                nullable: true,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: None,
            },
            SchemaField {
                path: "behavior.browser_enabled".into(),
                label: "Browser Access".into(),
                description: Some("Whether this user's agent can use browser tools".into()),
                input_type: "boolean",
                default: None,
                required: false,
                nullable: true,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: None,
            },
        ],
        subsections: vec![],
        collection: None,
        optional_section: false,
    }
}

fn user_telegram_section() -> SchemaSection {
    SchemaSection {
        id: "channels.telegram".into(),
        label: "Telegram Channel".into(),
        description: Some("Telegram bot channel adapter settings".into()),
        optional_section: true,
        fields: vec![
            SchemaField {
                path: "channels.telegram.enabled".into(),
                label: "Enabled".into(),
                description: Some("Activate the Telegram channel adapter".into()),
                input_type: "boolean",
                default: Some(Value::Bool(false)),
                required: false,
                nullable: false,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: None,
            },
            SchemaField {
                path: "channels.telegram.bot_token_env".into(),
                label: "Bot Token Env".into(),
                description: Some(
                    "Environment variable name holding the Telegram bot token".into(),
                ),
                input_type: "text",
                default: None,
                required: false,
                nullable: true,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: None,
            },
            SchemaField {
                path: "channels.telegram.polling_timeout_secs".into(),
                label: "Polling Timeout (seconds)".into(),
                description: Some("Long-polling timeout for Telegram updates".into()),
                input_type: "number",
                default: Some(Value::Number(30.into())),
                required: false,
                nullable: false,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: Some(FieldConstraints {
                    min: Some(1.0),
                    max: None,
                    step: None,
                }),
            },
            SchemaField {
                path: "channels.telegram.max_message_length".into(),
                label: "Max Message Length".into(),
                description: Some("Maximum characters before splitting a Telegram message".into()),
                input_type: "number",
                default: Some(Value::Number(4096.into())),
                required: false,
                nullable: false,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: Some(FieldConstraints {
                    min: Some(1.0),
                    max: None,
                    step: None,
                }),
            },
            SchemaField {
                path: "channels.telegram.senders".into(),
                label: "Senders".into(),
                description: Some(
                    "Authorized sender bindings (platform IDs and display names)".into(),
                ),
                input_type: "tag_list",
                default: None,
                required: false,
                nullable: false,
                allow_custom: false,
                dynamic_source: None,
                enum_options: None,
                constraints: None,
            },
        ],
        subsections: vec![],
        collection: None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt;

    fn test_state() -> Arc<WebState> {
        let config = types::RunnerGlobalConfig::default();
        let config_path = std::path::PathBuf::from("/tmp/test-runner.toml");
        Arc::new(WebState::new(
            config,
            config_path,
            "127.0.0.1:9400".to_owned(),
        ))
    }

    #[test]
    fn agent_schema_has_all_sections() {
        let schema = build_agent_schema();
        let section_ids: Vec<&str> = schema.sections.iter().map(|s| s.id.as_str()).collect();
        assert!(section_ids.contains(&"general"));
        assert!(section_ids.contains(&"selection"));
        assert!(section_ids.contains(&"runtime"));
        assert!(section_ids.contains(&"runtime.context_budget"));
        assert!(section_ids.contains(&"runtime.summarization"));
        assert!(section_ids.contains(&"memory"));
        assert!(section_ids.contains(&"memory.retrieval"));
        assert!(section_ids.contains(&"providers"));
        assert!(section_ids.contains(&"reliability"));
        assert!(section_ids.contains(&"catalog"));
        assert!(section_ids.contains(&"tools.web_search"));
        assert!(section_ids.contains(&"tools.shell"));
        assert!(section_ids.contains(&"tools.attachment_save"));
        assert!(section_ids.contains(&"scheduler"));
        assert!(section_ids.contains(&"gateway"));
        assert!(section_ids.contains(&"agents"));
    }

    #[test]
    fn runner_schema_has_all_sections() {
        let schema = build_runner_schema();
        let section_ids: Vec<&str> = schema.sections.iter().map(|s| s.id.as_str()).collect();
        assert!(section_ids.contains(&"general"));
        assert!(section_ids.contains(&"guest_images"));
        assert!(section_ids.contains(&"web"));
    }

    #[test]
    fn user_schema_has_all_sections() {
        let schema = build_user_schema();
        let section_ids: Vec<&str> = schema.sections.iter().map(|s| s.id.as_str()).collect();
        assert!(section_ids.contains(&"general"));
        assert!(section_ids.contains(&"mounts"));
        assert!(section_ids.contains(&"resources"));
        assert!(section_ids.contains(&"credential_refs"));
        assert!(section_ids.contains(&"behavior"));
        assert!(section_ids.contains(&"channels.telegram"));
    }

    #[test]
    fn dynamic_sources_include_tool_names() {
        let state = test_state();
        let sources = build_dynamic_sources(&state);
        assert!(!sources.tool_names.is_empty());
        // Verify canonical tools are present
        assert!(sources.tool_names.contains(&"file_read".to_owned()));
        assert!(sources.tool_names.contains(&"shell_exec".to_owned()));
        assert!(sources.tool_names.contains(&"web_search".to_owned()));
        assert!(sources.tool_names.contains(&"delegate_to_agent".to_owned()));
    }

    #[test]
    fn dynamic_sources_include_registered_providers() {
        let state = test_state();
        let sources = build_dynamic_sources(&state);
        // Default config includes at least openai + anthropic
        assert!(
            sources.registered_providers.contains(&"openai".to_owned())
                || !sources.registered_providers.is_empty(),
            "should have some registered providers"
        );
    }

    #[test]
    fn dynamic_sources_include_all_enum_sets() {
        let state = test_state();
        let sources = build_dynamic_sources(&state);
        assert!(!sources.provider_types.is_empty());
        assert!(!sources.sandbox_tiers.is_empty());
        assert!(!sources.auth_modes.is_empty());
        assert!(!sources.embedding_backends.is_empty());
        assert!(!sources.model2vec_models.is_empty());
        assert!(!sources.web_search_providers.is_empty());
        assert!(!sources.input_modalities.is_empty());
        assert!(!sources.safesearch_levels.is_empty());
        assert!(!sources.timezone_suggestions.is_empty());
    }

    #[test]
    fn provider_types_match_rust_enum() {
        let state = test_state();
        let sources = build_dynamic_sources(&state);
        let values: Vec<&str> = sources
            .provider_types
            .iter()
            .map(|o| o.value.as_str())
            .collect();
        assert!(values.contains(&"openai"));
        assert!(values.contains(&"anthropic"));
        assert!(values.contains(&"gemini"));
        assert!(values.contains(&"openai_responses"));
        assert_eq!(values.len(), 4, "should match the 4 ProviderType variants");
    }

    #[test]
    fn sandbox_tiers_match_rust_enum() {
        let state = test_state();
        let sources = build_dynamic_sources(&state);
        let values: Vec<&str> = sources
            .sandbox_tiers
            .iter()
            .map(|o| o.value.as_str())
            .collect();
        assert!(values.contains(&"micro_vm"));
        assert!(values.contains(&"container"));
        assert!(values.contains(&"process"));
        assert_eq!(values.len(), 3, "should match the 3 SandboxTier variants");
    }

    #[test]
    fn optional_sections_marked_correctly() {
        let schema = build_agent_schema();
        let web_search = schema
            .sections
            .iter()
            .find(|s| s.id == "tools.web_search")
            .unwrap();
        assert!(web_search.optional_section);

        let shell = schema
            .sections
            .iter()
            .find(|s| s.id == "tools.shell")
            .unwrap();
        assert!(shell.optional_section);

        let runtime = schema.sections.iter().find(|s| s.id == "runtime").unwrap();
        assert!(!runtime.optional_section);
    }

    #[test]
    fn collection_sections_have_metadata() {
        let schema = build_agent_schema();
        let providers = schema
            .sections
            .iter()
            .find(|s| s.id == "providers")
            .unwrap();
        let collection = providers.collection.as_ref().unwrap();
        assert_eq!(collection.kind, "map");
        assert!(collection.key_field.is_some());

        let agents = schema.sections.iter().find(|s| s.id == "agents").unwrap();
        let collection = agents.collection.as_ref().unwrap();
        assert_eq!(collection.kind, "map");
    }

    #[test]
    fn nullable_fields_present_for_optional_types() {
        let schema = build_agent_schema();
        let runtime = schema.sections.iter().find(|s| s.id == "runtime").unwrap();
        let max_cost = runtime
            .fields
            .iter()
            .find(|f| f.path == "runtime.max_cost")
            .unwrap();
        assert!(
            max_cost.nullable,
            "max_cost should be nullable (Option<f64>)"
        );

        let memory = schema.sections.iter().find(|s| s.id == "memory").unwrap();
        let remote_url = memory
            .fields
            .iter()
            .find(|f| f.path == "memory.remote_url")
            .unwrap();
        assert!(
            remote_url.nullable,
            "remote_url should be nullable (Option<String>)"
        );
    }

    #[test]
    fn schema_serializes_to_json_without_error() {
        let state = test_state();
        let sources = build_dynamic_sources(&state);
        let response = ConfigSchemaResponse {
            agent: build_agent_schema(),
            runner: build_runner_schema(),
            user: build_user_schema(),
            dynamic_sources: sources,
        };
        let json = serde_json::to_string(&response).expect("schema should serialize to JSON");
        assert!(!json.is_empty());
        // Verify it parses back as valid JSON
        let _parsed: serde_json::Value =
            serde_json::from_str(&json).expect("serialized schema should be valid JSON");
    }

    #[tokio::test]
    async fn schema_endpoint_returns_all_config_types() {
        let state = test_state();
        let app = crate::web::build_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/meta/schema")
                    .header("host", "127.0.0.1:9400")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["data"]["agent"]["sections"].is_array());
        assert!(json["data"]["runner"]["sections"].is_array());
        assert!(json["data"]["user"]["sections"].is_array());
        assert!(json["data"]["dynamic_sources"]["tool_names"].is_array());
        assert!(json["data"]["dynamic_sources"]["registered_providers"].is_array());
    }
}
