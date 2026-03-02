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
    /// Visual group identifier for organizing sections under common headings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Display label for the group (only needed on the first section of a group).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_label: Option<String>,
    /// Description for the group (only needed on the first section of a group).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_description: Option<String>,
    /// Custom label when the optional section toggle is ON (default: "Enabled").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub toggle_on_label: Option<String>,
    /// Custom label when the optional section toggle is OFF (default: "Disabled").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub toggle_off_label: Option<String>,
    /// When true the section is always expanded and cannot be collapsed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub always_expanded: bool,
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
    /// Placeholder text for text/number inputs (shown when empty).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    /// Whether this field is essential for basic system operation.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub setup_required: bool,
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
// Schema builder helpers
// ---------------------------------------------------------------------------

/// Create a `SchemaSection` with all optional fields defaulted.
fn sec(id: &str, label: &str) -> SchemaSection {
    SchemaSection {
        id: id.into(),
        label: label.into(),
        description: None,
        fields: vec![],
        subsections: vec![],
        collection: None,
        optional_section: false,
        group: None,
        group_label: None,
        group_description: None,
        toggle_on_label: None,
        toggle_off_label: None,
        always_expanded: false,
    }
}

/// Create a `SchemaField` with all optional fields defaulted.
fn fld(path: &str, label: &str, input_type: &'static str) -> SchemaField {
    SchemaField {
        path: path.into(),
        label: label.into(),
        description: None,
        input_type,
        default: None,
        required: false,
        nullable: false,
        allow_custom: false,
        dynamic_source: None,
        enum_options: None,
        constraints: None,
        placeholder: None,
        setup_required: false,
    }
}

fn opt(value: &str, label: &str) -> EnumOption {
    EnumOption {
        value: value.into(),
        label: label.into(),
    }
}

fn range(min: Option<f64>, max: Option<f64>, step: Option<f64>) -> Option<FieldConstraints> {
    Some(FieldConstraints { min, max, step })
}

// ---------------------------------------------------------------------------
// Agent schema
// ---------------------------------------------------------------------------

fn build_agent_schema() -> ConfigSchema {
    ConfigSchema {
        sections: vec![
            // ── Group: Core Setup ────────────────────────────────────
            agent_general_section(),
            agent_selection_section(),
            agent_providers_section(),
            agent_catalog_section(),
            // ── Group: Custom Agent Definitions ─────────────────────
            agent_agents_section(),
            // ── Group: Agent Behavior ────────────────────────────────
            agent_runtime_section(),
            agent_context_budget_section(),
            agent_summarization_section(),
            // ── Group: Memory ────────────────────────────────────────
            agent_memory_section(),
            agent_memory_retrieval_section(),
            // ── Group: Tools ─────────────────────────────────────────
            agent_tools_web_search_section(),
            agent_tools_shell_section(),
            agent_tools_attachment_save_section(),
            // ── Group: Infrastructure ────────────────────────────────
            agent_reliability_section(),
            agent_scheduler_section(),
            agent_gateway_section(),
        ],
    }
}

fn agent_general_section() -> SchemaSection {
    SchemaSection {
        description: None,
        group: Some("core_setup".into()),
        group_label: Some("Core Setup".into()),
        group_description: Some(
            "Essential provider and model configuration — set these up first to get started".into(),
        ),
        fields: vec![SchemaField {
            description: Some("Agent configuration schema version".into()),
            default: Some(Value::String("1.0.0".into())),
            ..fld("config_version", "Config Version", "readonly")
        }],
        ..sec("general", "General")
    }
}

fn agent_selection_section() -> SchemaSection {
    SchemaSection {
        description: Some("Choose the LLM provider and model for agent conversations".into()),
        group: Some("core_setup".into()),
        fields: vec![
            SchemaField {
                description: Some(
                    "The provider registry name to use for LLM API calls. \
                     Must match a provider configured in the Providers section below."
                        .into(),
                ),
                default: Some(Value::String("openai".into())),
                required: true,
                allow_custom: true,
                dynamic_source: Some("registered_providers".into()),
                setup_required: true,
                ..fld("selection.provider", "Provider", "select_dynamic")
            },
            SchemaField {
                description: Some(
                    "The model ID to use. Choose from the catalog \
                     or enter a custom model ID."
                        .into(),
                ),
                default: Some(Value::String("gpt-4o-mini".into())),
                required: true,
                allow_custom: true,
                setup_required: true,
                ..fld("selection.model", "Model", "model_picker")
            },
        ],
        ..sec("selection", "Model Selection")
    }
}

fn agent_providers_section() -> SchemaSection {
    let provider_types = vec![
        opt("openai", "OpenAI"),
        opt("anthropic", "Anthropic"),
        opt("gemini", "Gemini"),
        opt("openai_responses", "OpenAI Responses"),
    ];

    SchemaSection {
        description: Some(
            "LLM provider configurations. Each entry maps a registry name to API connection \
             details. You need at least one provider configured with valid credentials."
                .into(),
        ),
        group: Some("core_setup".into()),
        fields: vec![
            SchemaField {
                description: Some("LLM API protocol implementation".into()),
                required: true,
                default: Some(Value::String("openai".into())),
                enum_options: Some(provider_types),
                setup_required: true,
                ..fld("provider_type", "Provider Type", "select")
            },
            SchemaField {
                description: Some(
                    "Custom API base URL. Leave empty to use the provider's default endpoint."
                        .into(),
                ),
                nullable: true,
                placeholder: Some("Leave empty for provider default".into()),
                ..fld("base_url", "Base URL", "text")
            },
            SchemaField {
                description: Some(
                    "Direct API key. Prefer using an environment variable instead for security."
                        .into(),
                ),
                nullable: true,
                ..fld("api_key", "API Key", "secret")
            },
            SchemaField {
                description: Some(
                    "Environment variable name containing the API key \
                     (e.g. OPENAI_API_KEY). Recommended over inline API key."
                        .into(),
                ),
                nullable: true,
                placeholder: Some("e.g. OPENAI_API_KEY".into()),
                setup_required: true,
                ..fld("api_key_env", "API Key Env Variable", "text")
            },
            SchemaField {
                description: Some("Additional HTTP headers sent with every API request".into()),
                nullable: true,
                ..fld("extra_headers", "Extra Headers", "key_value_map")
            },
            SchemaField {
                description: Some(
                    "Model catalog namespace for model validation. \
                     Auto-detected from provider type (e.g. openai → 'openai', \
                     gemini → 'google'). Only set if you need a custom mapping."
                        .into(),
                ),
                nullable: true,
                placeholder: Some("Auto-detected from provider type".into()),
                ..fld("catalog_provider", "Catalog Provider", "text")
            },
        ],
        subsections: vec![SchemaSection {
            description: Some(
                "Capability overrides for models not found in the catalog. \
                 These are used as fallbacks only when skip_catalog_validation is enabled \
                 and the selected model is not in the catalog. For catalog-known models, \
                 capabilities are determined automatically."
                    .into(),
            ),
            fields: vec![
                SchemaField {
                    description: Some(
                        "Fallback: whether unknown models on this provider support \
                         file/image attachments"
                            .into(),
                    ),
                    nullable: true,
                    ..fld("attachment", "Supports Attachments", "boolean")
                },
                SchemaField {
                    description: Some(
                        "Fallback: accepted input types for unknown models on this provider".into(),
                    ),
                    nullable: true,
                    dynamic_source: Some("input_modalities".into()),
                    ..fld("input_modalities", "Input Modalities", "multi_select")
                },
                SchemaField {
                    description: Some(
                        "Fallback: whether unknown models on this provider support \
                         reasoning/thinking"
                            .into(),
                    ),
                    nullable: true,
                    ..fld("reasoning", "Supports Reasoning", "boolean")
                },
                SchemaField {
                    description: Some(
                        "Fallback: maximum input token limit for unknown models".into(),
                    ),
                    nullable: true,
                    constraints: range(Some(1.0), None, None),
                    ..fld("max_input_tokens", "Max Input Tokens", "number")
                },
                SchemaField {
                    description: Some(
                        "Fallback: maximum output token limit for unknown models".into(),
                    ),
                    nullable: true,
                    constraints: range(Some(1.0), None, None),
                    ..fld("max_output_tokens", "Max Output Tokens", "number")
                },
                SchemaField {
                    description: Some(
                        "Fallback: maximum context window size for unknown models".into(),
                    ),
                    nullable: true,
                    constraints: range(Some(1.0), None, None),
                    ..fld("max_context_tokens", "Max Context Tokens", "number")
                },
            ],
            ..sec("providers.advanced", "Advanced Overrides")
        }],
        collection: Some(CollectionMeta {
            kind: "map",
            add_label: "Add Provider".into(),
            key_field: Some(SchemaField {
                description: Some("Unique name for this provider configuration".into()),
                required: true,
                ..fld("_key", "Provider Name", "text")
            }),
        }),
        ..sec("providers", "Providers")
    }
}

fn agent_catalog_section() -> SchemaSection {
    SchemaSection {
        description: Some("Model catalog validation and refresh settings".into()),
        group: Some("core_setup".into()),
        fields: vec![
            SchemaField {
                description: Some(
                    "Allow models not found in the catalog. Enable for custom/proxy \
                     providers where models aren't listed in the public catalog."
                        .into(),
                ),
                default: Some(Value::Bool(false)),
                ..fld(
                    "catalog.skip_catalog_validation",
                    "Skip Catalog Validation",
                    "boolean",
                )
            },
            SchemaField {
                description: Some(
                    "URL for fetching the model catalog snapshot. \
                     Leave empty to use the default models.dev catalog."
                        .into(),
                ),
                nullable: true,
                placeholder: Some("Leave empty for default catalog".into()),
                ..fld("catalog.pinned_url", "Pinned Catalog URL", "text")
            },
        ],
        ..sec("catalog", "Catalog")
    }
}

fn agent_runtime_section() -> SchemaSection {
    SchemaSection {
        description: Some("Configure execution timeouts, turn limits, and cost caps".into()),
        group: Some("agent_behavior".into()),
        group_label: Some("Agent Behavior".into()),
        group_description: Some(
            "Control how the agent processes conversations, manages context, and defines \
             named agent variants"
                .into(),
        ),
        fields: vec![
            SchemaField {
                description: Some("Maximum time for a single LLM turn before timeout".into()),
                default: Some(Value::Number(60.into())),
                constraints: range(Some(1.0), None, None),
                ..fld(
                    "runtime.turn_timeout_secs",
                    "Turn Timeout (seconds)",
                    "number",
                )
            },
            SchemaField {
                description: Some("Maximum agentic loop iterations per session".into()),
                default: Some(Value::Number(8.into())),
                constraints: range(Some(1.0), None, None),
                ..fld("runtime.max_turns", "Max Turns", "number")
            },
            SchemaField {
                description: Some("Maximum USD cost per session. Empty = no limit.".into()),
                nullable: true,
                constraints: range(Some(0.0), None, None),
                placeholder: Some("No limit".into()),
                ..fld("runtime.max_cost", "Max Cost ($)", "number")
            },
        ],
        ..sec("runtime", "Runtime Limits")
    }
}

fn agent_context_budget_section() -> SchemaSection {
    SchemaSection {
        description: Some("Control when and how context window summarization triggers".into()),
        group: Some("agent_behavior".into()),
        fields: vec![
            SchemaField {
                description: Some(
                    "Context usage ratio that triggers summarization (e.g., 0.85 = 85% full)"
                        .into(),
                ),
                default: Some(serde_json::json!(0.85)),
                constraints: range(Some(0.0), Some(1.0), Some(0.05)),
                ..fld(
                    "runtime.context_budget.trigger_ratio",
                    "Trigger Ratio",
                    "number",
                )
            },
            SchemaField {
                description: Some(
                    "Tokens reserved as safety margin when computing context budget".into(),
                ),
                default: Some(Value::Number(1024.into())),
                constraints: range(Some(1.0), None, None),
                ..fld(
                    "runtime.context_budget.safety_buffer_tokens",
                    "Safety Buffer (tokens)",
                    "number",
                )
            },
            SchemaField {
                description: Some(
                    "Context window size fallback when model catalog has no data".into(),
                ),
                default: Some(Value::Number(128_000.into())),
                constraints: range(Some(1.0), None, None),
                ..fld(
                    "runtime.context_budget.fallback_max_context_tokens",
                    "Fallback Max Context",
                    "number",
                )
            },
        ],
        ..sec("runtime.context_budget", "Context Budget")
    }
}

fn agent_summarization_section() -> SchemaSection {
    SchemaSection {
        description: Some("Configure context rolling summary behavior".into()),
        group: Some("agent_behavior".into()),
        fields: vec![
            SchemaField {
                description: Some(
                    "How much to compress context during summarization (0.0-1.0)".into(),
                ),
                default: Some(serde_json::json!(0.5)),
                constraints: range(Some(0.0), Some(1.0), Some(0.05)),
                ..fld(
                    "runtime.summarization.target_ratio",
                    "Target Ratio",
                    "number",
                )
            },
            SchemaField {
                description: Some(
                    "Minimum conversation turns before summarization can trigger".into(),
                ),
                default: Some(Value::Number(6.into())),
                constraints: range(Some(1.0), None, None),
                ..fld("runtime.summarization.min_turns", "Min Turns", "number")
            },
        ],
        ..sec("runtime.summarization", "Summarization")
    }
}

fn agent_agents_section() -> SchemaSection {
    SchemaSection {
        description: Some(
            "Named agent definitions with optional provider/model overrides and tool \
             restrictions. Use these to create specialized agent variants."
                .into(),
        ),
        group: Some("custom_agent_definitions".into()),
        group_label: Some("Custom Agent Definitions".into()),
        group_description: Some(
            "Define named agent variants with their own model, prompt, and tool settings".into(),
        ),
        always_expanded: true,
        fields: vec![
            SchemaField {
                description: Some(
                    "Custom system prompt text. Overrides default agent behavior.".into(),
                ),
                nullable: true,
                ..fld("system_prompt", "System Prompt", "multiline")
            },
            SchemaField {
                description: Some(
                    "Path to a file containing the system prompt. Alternative to inline.".into(),
                ),
                nullable: true,
                ..fld("system_prompt_file", "System Prompt File", "text")
            },
            SchemaField {
                description: Some("Use a different provider for this agent".into()),
                nullable: true,
                allow_custom: true,
                dynamic_source: Some("registered_providers".into()),
                ..fld("selection.provider", "Provider Override", "select_dynamic")
            },
            SchemaField {
                description: Some("Use a different model for this agent".into()),
                nullable: true,
                allow_custom: true,
                ..fld("selection.model", "Model Override", "model_picker")
            },
            SchemaField {
                description: Some(
                    "Restrict this agent to specific tools. Empty = all tools.".into(),
                ),
                nullable: true,
                dynamic_source: Some("tool_names".into()),
                ..fld("tools", "Allowed Tools", "multi_select")
            },
            SchemaField {
                description: Some("Override the global max turns for this agent".into()),
                nullable: true,
                constraints: range(Some(1.0), None, None),
                ..fld("max_turns", "Max Turns Override", "number")
            },
            SchemaField {
                description: Some("Override the global max cost for this agent".into()),
                nullable: true,
                constraints: range(Some(0.0), None, None),
                ..fld("max_cost", "Max Cost Override ($)", "number")
            },
        ],
        collection: Some(CollectionMeta {
            kind: "map",
            add_label: "Add Agent".into(),
            key_field: Some(SchemaField {
                description: Some("Unique identifier for this agent definition".into()),
                required: true,
                ..fld("_key", "Agent Name", "text")
            }),
        }),
        ..sec("agents", "Custom Agent Definitions")
    }
}

fn agent_memory_section() -> SchemaSection {
    SchemaSection {
        description: Some("Persistent conversational memory settings".into()),
        group: Some("memory".into()),
        group_label: Some("Memory".into()),
        group_description: Some("Persistent conversational memory and retrieval settings".into()),
        fields: vec![
            SchemaField {
                description: Some("Enable persistent conversational memory".into()),
                default: Some(Value::Bool(false)),
                ..fld("memory.enabled", "Enable Memory", "boolean")
            },
            SchemaField {
                description: Some(
                    "URL for remote memory storage. Leave empty for local storage.".into(),
                ),
                nullable: true,
                placeholder: Some("Leave empty for local storage".into()),
                ..fld("memory.remote_url", "Remote URL", "text")
            },
            SchemaField {
                description: Some("Authentication token for remote memory server".into()),
                nullable: true,
                ..fld("memory.auth_token", "Auth Token", "secret")
            },
            SchemaField {
                description: Some("Backend for generating vector embeddings".into()),
                default: Some(Value::String("model2vec".into())),
                enum_options: Some(vec![
                    opt("model2vec", "Model2Vec"),
                    opt("deterministic", "Deterministic"),
                ]),
                ..fld("memory.embedding_backend", "Embedding Backend", "select")
            },
            SchemaField {
                description: Some("Model size for Model2Vec embeddings".into()),
                default: Some(Value::String("potion_32m".into())),
                enum_options: Some(vec![
                    opt("potion_8m", "Potion 8M"),
                    opt("potion_32m", "Potion 32M"),
                ]),
                ..fld("memory.model2vec_model", "Model2Vec Model", "select")
            },
        ],
        ..sec("memory", "Memory")
    }
}

fn agent_memory_retrieval_section() -> SchemaSection {
    SchemaSection {
        description: Some("Configure how memory entries are searched and ranked".into()),
        group: Some("memory".into()),
        fields: vec![
            SchemaField {
                description: Some("Number of most relevant memory entries to retrieve".into()),
                default: Some(Value::Number(8.into())),
                constraints: range(Some(1.0), None, None),
                ..fld("memory.retrieval.top_k", "Top K Results", "number")
            },
            SchemaField {
                description: Some(
                    "Weight for vector similarity search (0.0-1.0). \
                     Vector + FTS must sum to 1.0."
                        .into(),
                ),
                default: Some(serde_json::json!(0.7)),
                constraints: range(Some(0.0), Some(1.0), Some(0.1)),
                ..fld("memory.retrieval.vector_weight", "Vector Weight", "number")
            },
            SchemaField {
                description: Some(
                    "Weight for full-text search (0.0-1.0). \
                     Vector + FTS must sum to 1.0."
                        .into(),
                ),
                default: Some(serde_json::json!(0.3)),
                constraints: range(Some(0.0), Some(1.0), Some(0.1)),
                ..fld("memory.retrieval.fts_weight", "Full-Text Weight", "number")
            },
        ],
        ..sec("memory.retrieval", "Memory Retrieval")
    }
}

fn agent_tools_web_search_section() -> SchemaSection {
    SchemaSection {
        description: Some(
            "Web search provider configuration. The web search tool is always available — \
             these settings customize which search backend and parameters to use."
                .into(),
        ),
        group: Some("tools".into()),
        group_label: Some("Tools".into()),
        group_description: Some(
            "Tool-specific configuration. Tools are always available — these settings \
             customize their behavior."
                .into(),
        ),
        optional_section: true,
        toggle_on_label: Some("Custom".into()),
        toggle_off_label: Some("Defaults".into()),
        fields: vec![
            SchemaField {
                description: Some("Web search backend to use".into()),
                nullable: true,
                enum_options: Some(vec![
                    opt("duckduckgo", "DuckDuckGo"),
                    opt("google", "Google"),
                    opt("searxng", "SearxNG"),
                ]),
                ..fld("tools.web_search.provider", "Search Provider", "select")
            },
            SchemaField {
                description: Some("Custom base URL for the search provider".into()),
                nullable: true,
                ..fld("tools.web_search.base_url", "Base URL", "text")
            },
            SchemaField {
                description: Some("Comma-separated fallback base URLs".into()),
                nullable: true,
                ..fld("tools.web_search.base_urls", "Fallback URLs", "text")
            },
            SchemaField {
                description: Some("Env var name for Google API key".into()),
                nullable: true,
                placeholder: Some("e.g. GOOGLE_API_KEY".into()),
                ..fld("tools.web_search.api_key_env", "API Key Env", "text")
            },
            SchemaField {
                description: Some("Env var name for Google Custom Search engine ID".into()),
                nullable: true,
                placeholder: Some("e.g. GOOGLE_CSE_ID".into()),
                ..fld("tools.web_search.engine_id_env", "Engine ID Env", "text")
            },
            SchemaField {
                description: Some("Additional parameters as `key=value&key2=value2`".into()),
                nullable: true,
                ..fld(
                    "tools.web_search.query_params",
                    "Extra Query Params",
                    "text",
                )
            },
            SchemaField {
                description: Some("Comma-separated SearxNG search engines".into()),
                nullable: true,
                ..fld("tools.web_search.engines", "SearxNG Engines", "text")
            },
            SchemaField {
                description: Some("Comma-separated SearxNG categories".into()),
                nullable: true,
                ..fld("tools.web_search.categories", "SearxNG Categories", "text")
            },
            SchemaField {
                description: Some("SearxNG safe search level".into()),
                nullable: true,
                enum_options: Some(vec![
                    opt("0", "Off"),
                    opt("1", "Moderate"),
                    opt("2", "Strict"),
                ]),
                ..fld("tools.web_search.safesearch", "Safe Search Level", "select")
            },
            SchemaField {
                description: Some("Hosts allowed to resolve to private/loopback addresses".into()),
                nullable: true,
                ..fld(
                    "tools.web_search.egress_allowlist",
                    "Egress Allowlist",
                    "tag_list",
                )
            },
        ],
        ..sec("tools.web_search", "Tools — Web Search")
    }
}

fn agent_tools_shell_section() -> SchemaSection {
    SchemaSection {
        description: Some(
            "Shell command execution security policy. The shell tool is always available — \
             these settings customize the command allowlist and behavior."
                .into(),
        ),
        group: Some("tools".into()),
        optional_section: true,
        toggle_on_label: Some("Custom".into()),
        toggle_off_label: Some("Defaults".into()),
        fields: vec![
            SchemaField {
                description: Some(
                    "Additional commands/patterns to add to the allowlist. \
                     Supports glob patterns (e.g. npm*, cargo-*)."
                        .into(),
                ),
                nullable: true,
                ..fld("tools.shell.allow", "Allow Commands", "tag_list")
            },
            SchemaField {
                description: Some(
                    "Commands/patterns to remove from the allowlist (deny takes priority)".into(),
                ),
                nullable: true,
                ..fld("tools.shell.deny", "Deny Commands", "tag_list")
            },
            SchemaField {
                description: Some(
                    "If enabled, the allow list replaces the built-in list entirely \
                     instead of extending it"
                        .into(),
                ),
                nullable: true,
                default: Some(Value::Bool(false)),
                ..fld(
                    "tools.shell.replace_defaults",
                    "Replace Default Allowlist",
                    "boolean",
                )
            },
            SchemaField {
                description: Some("Allow shell control operators (&&, ||, |, etc.)".into()),
                nullable: true,
                default: Some(Value::Bool(false)),
                ..fld(
                    "tools.shell.allow_operators",
                    "Allow Shell Operators",
                    "boolean",
                )
            },
            SchemaField {
                description: Some(
                    "Env var names to forward into the shell container. \
                     Each entry is a bare env var name (e.g. MY_TOKEN)."
                        .into(),
                ),
                nullable: true,
                ..fld("tools.shell.env_keys", "Environment Variables", "tag_list")
            },
        ],
        ..sec("tools.shell", "Tools — Shell")
    }
}

fn agent_tools_attachment_save_section() -> SchemaSection {
    SchemaSection {
        description: Some(
            "Override the default file save operation timeout. The attachment save tool \
             always works — toggle this on only if you need a custom timeout value. \
             Default timeout is 60 seconds."
                .into(),
        ),
        group: Some("tools".into()),
        optional_section: true,
        toggle_on_label: Some("Override".into()),
        toggle_off_label: Some("Default (60s)".into()),
        fields: vec![SchemaField {
            description: Some(
                "Timeout for file save operations. Only takes effect when override is enabled."
                    .into(),
            ),
            default: Some(Value::Number(60.into())),
            constraints: range(Some(1.0), None, None),
            ..fld(
                "tools.attachment_save.timeout_secs",
                "Save Timeout (seconds)",
                "number",
            )
        }],
        ..sec("tools.attachment_save", "Tools — Attachment Save")
    }
}

fn agent_reliability_section() -> SchemaSection {
    SchemaSection {
        description: Some("Retry and backoff settings for failed API calls".into()),
        group: Some("infrastructure".into()),
        group_label: Some("Infrastructure".into()),
        group_description: Some(
            "Reliability, scheduling, and gateway settings for production operation".into(),
        ),
        fields: vec![
            SchemaField {
                description: Some("Number of retry attempts for failed API calls".into()),
                default: Some(Value::Number(3.into())),
                constraints: range(Some(1.0), None, None),
                ..fld("reliability.max_attempts", "Max Retry Attempts", "number")
            },
            SchemaField {
                description: Some("Base delay in milliseconds for exponential backoff".into()),
                default: Some(Value::Number(250.into())),
                constraints: range(Some(1.0), None, None),
                ..fld("reliability.backoff_base_ms", "Backoff Base (ms)", "number")
            },
            SchemaField {
                description: Some("Maximum backoff delay in milliseconds. Must be ≥ base.".into()),
                default: Some(Value::Number(2000.into())),
                constraints: range(Some(1.0), None, None),
                ..fld("reliability.backoff_max_ms", "Backoff Max (ms)", "number")
            },
            SchemaField {
                description: Some("Add random jitter to backoff to prevent thundering herd".into()),
                default: Some(Value::Bool(false)),
                ..fld("reliability.jitter", "Enable Jitter", "boolean")
            },
        ],
        ..sec("reliability", "Reliability")
    }
}

fn agent_scheduler_section() -> SchemaSection {
    SchemaSection {
        description: Some("Scheduled and recurring task execution settings".into()),
        group: Some("infrastructure".into()),
        fields: vec![
            SchemaField {
                description: Some("Enable scheduled/recurring task execution".into()),
                default: Some(Value::Bool(false)),
                ..fld("scheduler.enabled", "Enable Scheduler", "boolean")
            },
            SchemaField {
                description: Some("How often the executor checks for due schedules".into()),
                default: Some(Value::Number(15.into())),
                constraints: range(Some(1.0), None, None),
                ..fld(
                    "scheduler.poll_interval_secs",
                    "Poll Interval (seconds)",
                    "number",
                )
            },
            SchemaField {
                description: Some("Maximum simultaneous scheduled executions".into()),
                default: Some(Value::Number(2.into())),
                constraints: range(Some(1.0), None, None),
                ..fld("scheduler.max_concurrent", "Max Concurrent", "number")
            },
            SchemaField {
                description: Some("Maximum number of schedules a user can create".into()),
                default: Some(Value::Number(50.into())),
                constraints: range(Some(1.0), None, None),
                ..fld(
                    "scheduler.max_schedules_per_user",
                    "Max Schedules Per User",
                    "number",
                )
            },
            SchemaField {
                description: Some("Maximum turns for each scheduled execution".into()),
                default: Some(Value::Number(10.into())),
                constraints: range(Some(1.0), None, None),
                ..fld("scheduler.max_turns", "Max Turns Per Run", "number")
            },
            SchemaField {
                description: Some("Maximum USD cost per scheduled execution".into()),
                default: Some(serde_json::json!(0.50)),
                constraints: range(Some(0.0), None, None),
                ..fld("scheduler.max_cost", "Max Cost Per Run ($)", "number")
            },
            SchemaField {
                description: Some("History entries to keep per schedule".into()),
                default: Some(Value::Number(20.into())),
                constraints: range(Some(1.0), None, None),
                ..fld("scheduler.max_run_history", "Max Run History", "number")
            },
            SchemaField {
                description: Some("Minimum time between runs (anti-abuse)".into()),
                default: Some(Value::Number(60.into())),
                constraints: range(Some(1.0), None, None),
                ..fld(
                    "scheduler.min_interval_secs",
                    "Min Interval (seconds)",
                    "number",
                )
            },
            SchemaField {
                description: Some("Timezone for cron schedule interpretation".into()),
                default: Some(Value::String("Asia/Kolkata".into())),
                dynamic_source: Some("timezone_suggestions".into()),
                ..fld("scheduler.default_timezone", "Default Timezone", "text")
            },
            SchemaField {
                description: Some("Consecutive failures before auto-disabling a schedule".into()),
                default: Some(Value::Number(5.into())),
                constraints: range(Some(1.0), None, None),
                ..fld(
                    "scheduler.auto_disable_after_failures",
                    "Auto-Disable After Failures",
                    "number",
                )
            },
            SchemaField {
                description: Some(
                    "Consecutive failures before notifying the user. 0 = disabled.".into(),
                ),
                default: Some(Value::Number(3.into())),
                constraints: range(Some(0.0), None, None),
                ..fld(
                    "scheduler.notify_after_failures",
                    "Notify After Failures",
                    "number",
                )
            },
        ],
        ..sec("scheduler", "Scheduler")
    }
}

fn agent_gateway_section() -> SchemaSection {
    SchemaSection {
        description: Some("Session and concurrency limits for the agent gateway".into()),
        group: Some("infrastructure".into()),
        fields: vec![
            SchemaField {
                description: Some("Maximum concurrent sessions per user".into()),
                default: Some(Value::Number(50.into())),
                constraints: range(Some(1.0), None, None),
                ..fld("gateway.max_sessions_per_user", "Max Sessions", "number")
            },
            SchemaField {
                description: Some("Maximum simultaneous turns per user".into()),
                default: Some(Value::Number(10.into())),
                constraints: range(Some(1.0), None, None),
                ..fld(
                    "gateway.max_concurrent_turns_per_user",
                    "Max Concurrent Turns",
                    "number",
                )
            },
            SchemaField {
                description: Some("Hours before an idle session is cleaned up".into()),
                default: Some(Value::Number(48.into())),
                constraints: range(Some(1.0), None, None),
                ..fld(
                    "gateway.session_idle_ttl_hours",
                    "Session Idle TTL (hours)",
                    "number",
                )
            },
        ],
        ..sec("gateway", "Gateway")
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
        fields: vec![
            SchemaField {
                description: Some("Runner configuration schema version".into()),
                default: Some(Value::String("1.0.1".into())),
                ..fld("config_version", "Config Version", "readonly")
            },
            SchemaField {
                description: Some(
                    "Directory for user workspace data. Each registered user gets \
                     a subdirectory here for their shared files, temp data, and vault."
                        .into(),
                ),
                default: Some(Value::String(".oxydra/workspaces".into())),
                required: true,
                setup_required: true,
                ..fld("workspace_root", "Workspace Root", "text")
            },
            SchemaField {
                description: Some("Default isolation level for new users".into()),
                default: Some(Value::String("micro_vm".into())),
                enum_options: Some(vec![
                    opt("micro_vm", "Micro VM"),
                    opt("container", "Container"),
                    opt("process", "Process"),
                ]),
                ..fld("default_tier", "Default Sandbox Tier", "select")
            },
        ],
        ..sec("general", "General")
    }
}

fn runner_guest_images_section() -> SchemaSection {
    SchemaSection {
        description: Some("Container and VM image references".into()),
        fields: vec![
            SchemaField {
                description: Some("Container/VM image for the oxydra runtime".into()),
                default: Some(Value::String("oxydra-vm:latest".into())),
                required: true,
                ..fld("guest_images.oxydra_vm", "Oxydra VM Image", "text")
            },
            SchemaField {
                description: Some("Container/VM image for shell execution".into()),
                default: Some(Value::String("shell-vm:latest".into())),
                required: true,
                ..fld("guest_images.shell_vm", "Shell VM Image", "text")
            },
            SchemaField {
                description: Some(
                    "JSON config file path for Firecracker microvm (Linux only)".into(),
                ),
                nullable: true,
                placeholder: Some("Linux only — leave empty otherwise".into()),
                ..fld(
                    "guest_images.firecracker_oxydra_vm_config",
                    "Firecracker Oxydra Config",
                    "text",
                )
            },
            SchemaField {
                description: Some(
                    "JSON config file path for Firecracker microvm (Linux only)".into(),
                ),
                nullable: true,
                placeholder: Some("Linux only — leave empty otherwise".into()),
                ..fld(
                    "guest_images.firecracker_shell_vm_config",
                    "Firecracker Shell Config",
                    "text",
                )
            },
        ],
        ..sec("guest_images", "Guest Images")
    }
}

fn runner_web_section() -> SchemaSection {
    SchemaSection {
        description: Some("Embedded web UI settings".into()),
        fields: vec![
            SchemaField {
                description: Some("Whether the web UI is available".into()),
                default: Some(Value::Bool(true)),
                ..fld("web.enabled", "Enable Web Configurator", "boolean")
            },
            SchemaField {
                description: Some(
                    "IP address and port for the web server (e.g. 127.0.0.1:9400)".into(),
                ),
                default: Some(Value::String("127.0.0.1:9400".into())),
                required: true,
                ..fld("web.bind", "Bind Address", "text")
            },
            SchemaField {
                description: Some("How to protect the web interface".into()),
                default: Some(Value::String("disabled".into())),
                enum_options: Some(vec![
                    opt("disabled", "Disabled (loopback only)"),
                    opt("token", "Bearer Token"),
                ]),
                ..fld("web.auth_mode", "Authentication Mode", "select")
            },
            SchemaField {
                description: Some("Environment variable containing the bearer token".into()),
                nullable: true,
                placeholder: Some("e.g. OXYDRA_WEB_AUTH_TOKEN".into()),
                ..fld("web.auth_token_env", "Auth Token Env Variable", "text")
            },
            SchemaField {
                description: Some(
                    "Inline bearer token. Prefer using an environment variable instead.".into(),
                ),
                nullable: true,
                ..fld("web.auth_token", "Auth Token (inline)", "secret")
            },
        ],
        ..sec("web", "Web Configurator")
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
        fields: vec![SchemaField {
            description: Some("User configuration schema version".into()),
            default: Some(Value::String("1.0.1".into())),
            ..fld("config_version", "Config Version", "readonly")
        }],
        ..sec("general", "General")
    }
}

fn user_mounts_section() -> SchemaSection {
    SchemaSection {
        description: Some(
            "Override workspace mount paths for this user. Leave empty to use the \
             auto-resolved defaults based on workspace_root. Only override if you \
             need a custom directory layout."
                .into(),
        ),
        group: Some("storage".into()),
        group_label: Some("Storage & Resources".into()),
        group_description: Some("Mount paths and compute resource limits for this user".into()),
        fields: vec![
            SchemaField {
                description: Some(
                    "Override the shared workspace mount path. Default: \
                     <workspace_root>/<user_id>/shared"
                        .into(),
                ),
                nullable: true,
                placeholder: Some("Auto-resolved — only override if needed".into()),
                ..fld("mounts.shared", "Shared Mount", "text")
            },
            SchemaField {
                description: Some(
                    "Override the temp files mount path. Default: \
                     <workspace_root>/<user_id>/tmp"
                        .into(),
                ),
                nullable: true,
                placeholder: Some("Auto-resolved — only override if needed".into()),
                ..fld("mounts.tmp", "Temp Mount", "text")
            },
            SchemaField {
                description: Some(
                    "Override the vault (credential storage) mount path. Default: \
                     <workspace_root>/<user_id>/vault"
                        .into(),
                ),
                nullable: true,
                placeholder: Some("Auto-resolved — only override if needed".into()),
                ..fld("mounts.vault", "Vault Mount", "text")
            },
        ],
        ..sec("mounts", "Mount Paths")
    }
}

fn user_resources_section() -> SchemaSection {
    SchemaSection {
        description: Some(
            "Maximum compute resources for this user. Leave empty for no limit.".into(),
        ),
        group: Some("storage".into()),
        fields: vec![
            SchemaField {
                description: Some("Maximum virtual CPU cores. Empty = no limit.".into()),
                nullable: true,
                constraints: range(Some(1.0), None, None),
                placeholder: Some("No limit".into()),
                ..fld("resources.max_vcpus", "Max vCPUs", "number")
            },
            SchemaField {
                description: Some("Maximum memory in mebibytes. Empty = no limit.".into()),
                nullable: true,
                constraints: range(Some(1.0), None, None),
                placeholder: Some("No limit".into()),
                ..fld("resources.max_memory_mib", "Max Memory (MiB)", "number")
            },
            SchemaField {
                description: Some("Maximum concurrent processes. Empty = no limit.".into()),
                nullable: true,
                constraints: range(Some(1.0), None, None),
                placeholder: Some("No limit".into()),
                ..fld("resources.max_processes", "Max Processes", "number")
            },
        ],
        ..sec("resources", "Resource Limits")
    }
}

fn user_credential_refs_section() -> SchemaSection {
    SchemaSection {
        description: Some("Credential key-value pairs available to the agent".into()),
        group: Some("security".into()),
        group_label: Some("Security & Access".into()),
        group_description: Some("Credential management and access control overrides".into()),
        fields: vec![
            SchemaField {
                description: Some("Identifier for this credential".into()),
                required: true,
                ..fld("_key", "Credential Name", "text")
            },
            SchemaField {
                description: Some("The credential secret value".into()),
                required: true,
                ..fld("_value", "Credential Value", "secret")
            },
        ],
        collection: Some(CollectionMeta {
            kind: "map",
            add_label: "Add Credential".into(),
            key_field: None,
        }),
        ..sec("credential_refs", "Credential References")
    }
}

fn user_behavior_section() -> SchemaSection {
    SchemaSection {
        description: Some(
            "Override default sandbox and tool access settings for this user. \
             Leave unset to use the runner-level defaults."
                .into(),
        ),
        group: Some("security".into()),
        fields: vec![
            SchemaField {
                description: Some(
                    "Override the default sandbox tier for this user. \
                     Leave unset to use the runner default."
                        .into(),
                ),
                nullable: true,
                enum_options: Some(vec![
                    opt("micro_vm", "Micro VM"),
                    opt("container", "Container"),
                    opt("process", "Process"),
                ]),
                ..fld("behavior.sandbox_tier", "Sandbox Tier Override", "select")
            },
            SchemaField {
                description: Some(
                    "Whether this user's agent can use the shell tool. \
                     Leave unset to use the default."
                        .into(),
                ),
                nullable: true,
                ..fld("behavior.shell_enabled", "Shell Access", "boolean")
            },
            SchemaField {
                description: Some(
                    "Whether this user's agent can use browser tools. \
                     Leave unset to use the default."
                        .into(),
                ),
                nullable: true,
                ..fld("behavior.browser_enabled", "Browser Access", "boolean")
            },
        ],
        ..sec("behavior", "Behavior Overrides")
    }
}

fn user_telegram_section() -> SchemaSection {
    SchemaSection {
        description: Some("Telegram bot channel adapter settings".into()),
        group: Some("channels".into()),
        group_label: Some("Channels".into()),
        group_description: Some("Communication channel settings".into()),
        optional_section: true,
        toggle_on_label: Some("Enabled".into()),
        toggle_off_label: Some("Disabled".into()),
        fields: vec![
            SchemaField {
                description: Some("Activate the Telegram channel adapter".into()),
                default: Some(Value::Bool(false)),
                ..fld("channels.telegram.enabled", "Enabled", "boolean")
            },
            SchemaField {
                description: Some(
                    "Environment variable name holding the Telegram bot token".into(),
                ),
                nullable: true,
                placeholder: Some("e.g. TELEGRAM_BOT_TOKEN".into()),
                ..fld("channels.telegram.bot_token_env", "Bot Token Env", "text")
            },
            SchemaField {
                description: Some("Long-polling timeout for Telegram updates".into()),
                default: Some(Value::Number(30.into())),
                constraints: range(Some(1.0), None, None),
                ..fld(
                    "channels.telegram.polling_timeout_secs",
                    "Polling Timeout (seconds)",
                    "number",
                )
            },
            SchemaField {
                description: Some("Maximum characters before splitting a Telegram message".into()),
                default: Some(Value::Number(4096.into())),
                constraints: range(Some(1.0), None, None),
                ..fld(
                    "channels.telegram.max_message_length",
                    "Max Message Length",
                    "number",
                )
            },
            SchemaField {
                description: Some(
                    "Authorized sender bindings (platform IDs and display names)".into(),
                ),
                ..fld("channels.telegram.senders", "Senders", "tag_list")
            },
        ],
        ..sec("channels.telegram", "Telegram Channel")
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

    // ── Section presence tests ──────────────────────────────────

    #[test]
    fn agent_schema_has_all_sections() {
        let schema = build_agent_schema();
        let section_ids: Vec<&str> = schema.sections.iter().map(|s| s.id.as_str()).collect();
        let expected = [
            "general",
            "selection",
            "runtime",
            "runtime.context_budget",
            "runtime.summarization",
            "memory",
            "memory.retrieval",
            "providers",
            "reliability",
            "catalog",
            "tools.web_search",
            "tools.shell",
            "tools.attachment_save",
            "scheduler",
            "gateway",
            "agents",
        ];
        for id in &expected {
            assert!(
                section_ids.contains(id),
                "agent schema missing section '{id}'"
            );
        }
        assert_eq!(
            section_ids.len(),
            expected.len(),
            "agent schema has unexpected extra sections: {section_ids:?}"
        );
    }

    #[test]
    fn runner_schema_has_all_sections() {
        let schema = build_runner_schema();
        let section_ids: Vec<&str> = schema.sections.iter().map(|s| s.id.as_str()).collect();
        let expected = ["general", "guest_images", "web"];
        for id in &expected {
            assert!(
                section_ids.contains(id),
                "runner schema missing section '{id}'"
            );
        }
        assert_eq!(section_ids.len(), expected.len());
    }

    #[test]
    fn user_schema_has_all_sections() {
        let schema = build_user_schema();
        let section_ids: Vec<&str> = schema.sections.iter().map(|s| s.id.as_str()).collect();
        let expected = [
            "general",
            "mounts",
            "resources",
            "credential_refs",
            "behavior",
            "channels.telegram",
        ];
        for id in &expected {
            assert!(
                section_ids.contains(id),
                "user schema missing section '{id}'"
            );
        }
        assert_eq!(section_ids.len(), expected.len());
    }

    // ── Dynamic sources tests ────────────────────────────────────

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
    fn dynamic_sources_tool_names_match_canonical() {
        let state = test_state();
        let sources = build_dynamic_sources(&state);
        let canonical: Vec<String> = tools::canonical_tool_names()
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            sources.tool_names, canonical,
            "dynamic sources tool_names should exactly match tools::canonical_tool_names()"
        );
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

    // ── Enum parity tests ────────────────────────────────────────

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

        // Verify each value round-trips through serde deserialization.
        for v in &values {
            let json = format!("\"{v}\"");
            let parsed: types::ProviderType = serde_json::from_str(&json)
                .unwrap_or_else(|e| panic!("provider_type '{v}' should deserialize: {e}"));
            let serialized = serde_json::to_string(&parsed).unwrap();
            assert_eq!(
                serialized, json,
                "provider_type '{v}' should round-trip through serde"
            );
        }
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

        // Verify round-trip through serde.
        for v in &values {
            let json = format!("\"{v}\"");
            let parsed: types::SandboxTier = serde_json::from_str(&json)
                .unwrap_or_else(|e| panic!("sandbox_tier '{v}' should deserialize: {e}"));
            let serialized = serde_json::to_string(&parsed).unwrap();
            assert_eq!(serialized, json);
        }
    }

    #[test]
    fn auth_modes_match_rust_enum() {
        let state = test_state();
        let sources = build_dynamic_sources(&state);
        let values: Vec<&str> = sources
            .auth_modes
            .iter()
            .map(|o| o.value.as_str())
            .collect();
        assert!(values.contains(&"disabled"));
        assert!(values.contains(&"token"));
        assert_eq!(values.len(), 2, "should match the 2 WebAuthMode variants");

        for v in &values {
            let json = format!("\"{v}\"");
            let parsed: types::WebAuthMode = serde_json::from_str(&json)
                .unwrap_or_else(|e| panic!("auth_mode '{v}' should deserialize: {e}"));
            let serialized = serde_json::to_string(&parsed).unwrap();
            assert_eq!(serialized, json);
        }
    }

    #[test]
    fn embedding_backends_match_rust_enum() {
        let state = test_state();
        let sources = build_dynamic_sources(&state);
        let values: Vec<&str> = sources
            .embedding_backends
            .iter()
            .map(|o| o.value.as_str())
            .collect();
        assert!(values.contains(&"model2vec"));
        assert!(values.contains(&"deterministic"));
        assert_eq!(
            values.len(),
            2,
            "should match the 2 MemoryEmbeddingBackend variants"
        );

        for v in &values {
            let json = format!("\"{v}\"");
            let parsed: types::MemoryEmbeddingBackend = serde_json::from_str(&json)
                .unwrap_or_else(|e| panic!("embedding_backend '{v}' should deserialize: {e}"));
            let serialized = serde_json::to_string(&parsed).unwrap();
            assert_eq!(serialized, json);
        }
    }

    #[test]
    fn model2vec_models_match_rust_enum() {
        let state = test_state();
        let sources = build_dynamic_sources(&state);
        let values: Vec<&str> = sources
            .model2vec_models
            .iter()
            .map(|o| o.value.as_str())
            .collect();
        assert!(values.contains(&"potion_8m"));
        assert!(values.contains(&"potion_32m"));
        assert_eq!(
            values.len(),
            2,
            "should match the 2 Model2vecModel variants"
        );

        for v in &values {
            let json = format!("\"{v}\"");
            let parsed: types::Model2vecModel = serde_json::from_str(&json)
                .unwrap_or_else(|e| panic!("model2vec_model '{v}' should deserialize: {e}"));
            let serialized = serde_json::to_string(&parsed).unwrap();
            assert_eq!(serialized, json);
        }
    }

    // ── Field-path coverage tests ────────────────────────────────

    /// Collect all field paths from a schema, recursively including subsections.
    fn collect_field_paths(schema: &ConfigSchema) -> Vec<String> {
        let mut paths = Vec::new();
        for section in &schema.sections {
            collect_section_field_paths(section, &mut paths);
        }
        paths
    }

    fn collect_section_field_paths(section: &SchemaSection, paths: &mut Vec<String>) {
        for field in &section.fields {
            paths.push(field.path.clone());
        }
        for sub in &section.subsections {
            collect_section_field_paths(sub, paths);
        }
        // Collection key fields
        if let Some(ref col) = section.collection
            && let Some(ref key_field) = col.key_field
        {
            paths.push(key_field.path.clone());
        }
    }

    #[test]
    fn agent_schema_covers_all_agent_config_field_paths() {
        let paths = collect_field_paths(&build_agent_schema());

        // General
        assert!(paths.contains(&"config_version".to_owned()));
        // Selection
        assert!(paths.contains(&"selection.provider".to_owned()));
        assert!(paths.contains(&"selection.model".to_owned()));
        // Runtime
        assert!(paths.contains(&"runtime.turn_timeout_secs".to_owned()));
        assert!(paths.contains(&"runtime.max_turns".to_owned()));
        assert!(paths.contains(&"runtime.max_cost".to_owned()));
        // Context budget
        assert!(paths.contains(&"runtime.context_budget.trigger_ratio".to_owned()));
        assert!(paths.contains(&"runtime.context_budget.safety_buffer_tokens".to_owned()));
        assert!(paths.contains(&"runtime.context_budget.fallback_max_context_tokens".to_owned()));
        // Summarization
        assert!(paths.contains(&"runtime.summarization.target_ratio".to_owned()));
        assert!(paths.contains(&"runtime.summarization.min_turns".to_owned()));
        // Memory
        assert!(paths.contains(&"memory.enabled".to_owned()));
        assert!(paths.contains(&"memory.remote_url".to_owned()));
        assert!(paths.contains(&"memory.auth_token".to_owned()));
        assert!(paths.contains(&"memory.embedding_backend".to_owned()));
        assert!(paths.contains(&"memory.model2vec_model".to_owned()));
        // Memory retrieval
        assert!(paths.contains(&"memory.retrieval.top_k".to_owned()));
        assert!(paths.contains(&"memory.retrieval.vector_weight".to_owned()));
        assert!(paths.contains(&"memory.retrieval.fts_weight".to_owned()));
        // Providers (entry-level fields)
        assert!(paths.contains(&"provider_type".to_owned()));
        assert!(paths.contains(&"base_url".to_owned()));
        assert!(paths.contains(&"api_key".to_owned()));
        assert!(paths.contains(&"api_key_env".to_owned()));
        assert!(paths.contains(&"extra_headers".to_owned()));
        assert!(paths.contains(&"catalog_provider".to_owned()));
        // Providers advanced overrides
        assert!(paths.contains(&"attachment".to_owned()));
        assert!(paths.contains(&"input_modalities".to_owned()));
        assert!(paths.contains(&"reasoning".to_owned()));
        assert!(paths.contains(&"max_input_tokens".to_owned()));
        assert!(paths.contains(&"max_output_tokens".to_owned()));
        assert!(paths.contains(&"max_context_tokens".to_owned()));
        // Reliability
        assert!(paths.contains(&"reliability.max_attempts".to_owned()));
        assert!(paths.contains(&"reliability.backoff_base_ms".to_owned()));
        assert!(paths.contains(&"reliability.backoff_max_ms".to_owned()));
        assert!(paths.contains(&"reliability.jitter".to_owned()));
        // Catalog
        assert!(paths.contains(&"catalog.skip_catalog_validation".to_owned()));
        assert!(paths.contains(&"catalog.pinned_url".to_owned()));
        // Tools — web search
        assert!(paths.contains(&"tools.web_search.provider".to_owned()));
        assert!(paths.contains(&"tools.web_search.base_url".to_owned()));
        assert!(paths.contains(&"tools.web_search.egress_allowlist".to_owned()));
        // Tools — shell
        assert!(paths.contains(&"tools.shell.allow".to_owned()));
        assert!(paths.contains(&"tools.shell.deny".to_owned()));
        assert!(paths.contains(&"tools.shell.replace_defaults".to_owned()));
        assert!(paths.contains(&"tools.shell.allow_operators".to_owned()));
        assert!(paths.contains(&"tools.shell.env_keys".to_owned()));
        // Tools — attachment_save
        assert!(paths.contains(&"tools.attachment_save.timeout_secs".to_owned()));
        // Scheduler
        assert!(paths.contains(&"scheduler.enabled".to_owned()));
        assert!(paths.contains(&"scheduler.poll_interval_secs".to_owned()));
        assert!(paths.contains(&"scheduler.max_concurrent".to_owned()));
        assert!(paths.contains(&"scheduler.default_timezone".to_owned()));
        assert!(paths.contains(&"scheduler.auto_disable_after_failures".to_owned()));
        assert!(paths.contains(&"scheduler.notify_after_failures".to_owned()));
        // Gateway
        assert!(paths.contains(&"gateway.max_sessions_per_user".to_owned()));
        assert!(paths.contains(&"gateway.max_concurrent_turns_per_user".to_owned()));
        assert!(paths.contains(&"gateway.session_idle_ttl_hours".to_owned()));
        // Agent definitions (entry-level fields)
        assert!(paths.contains(&"system_prompt".to_owned()));
        assert!(paths.contains(&"system_prompt_file".to_owned()));
        assert!(paths.contains(&"tools".to_owned()));
    }

    #[test]
    fn runner_schema_covers_all_runner_config_field_paths() {
        let paths = collect_field_paths(&build_runner_schema());

        assert!(paths.contains(&"config_version".to_owned()));
        assert!(paths.contains(&"workspace_root".to_owned()));
        assert!(paths.contains(&"default_tier".to_owned()));
        assert!(paths.contains(&"guest_images.oxydra_vm".to_owned()));
        assert!(paths.contains(&"guest_images.shell_vm".to_owned()));
        assert!(paths.contains(&"guest_images.firecracker_oxydra_vm_config".to_owned()));
        assert!(paths.contains(&"guest_images.firecracker_shell_vm_config".to_owned()));
        assert!(paths.contains(&"web.enabled".to_owned()));
        assert!(paths.contains(&"web.bind".to_owned()));
        assert!(paths.contains(&"web.auth_mode".to_owned()));
        assert!(paths.contains(&"web.auth_token_env".to_owned()));
        assert!(paths.contains(&"web.auth_token".to_owned()));
    }

    #[test]
    fn user_schema_covers_all_user_config_field_paths() {
        let paths = collect_field_paths(&build_user_schema());

        assert!(paths.contains(&"config_version".to_owned()));
        assert!(paths.contains(&"mounts.shared".to_owned()));
        assert!(paths.contains(&"mounts.tmp".to_owned()));
        assert!(paths.contains(&"mounts.vault".to_owned()));
        assert!(paths.contains(&"resources.max_vcpus".to_owned()));
        assert!(paths.contains(&"resources.max_memory_mib".to_owned()));
        assert!(paths.contains(&"resources.max_processes".to_owned()));
        // Credential refs (key/value)
        assert!(paths.contains(&"_key".to_owned()));
        assert!(paths.contains(&"_value".to_owned()));
        // Behavior
        assert!(paths.contains(&"behavior.sandbox_tier".to_owned()));
        assert!(paths.contains(&"behavior.shell_enabled".to_owned()));
        assert!(paths.contains(&"behavior.browser_enabled".to_owned()));
        // Telegram
        assert!(paths.contains(&"channels.telegram.enabled".to_owned()));
        assert!(paths.contains(&"channels.telegram.bot_token_env".to_owned()));
        assert!(paths.contains(&"channels.telegram.polling_timeout_secs".to_owned()));
        assert!(paths.contains(&"channels.telegram.max_message_length".to_owned()));
        assert!(paths.contains(&"channels.telegram.senders".to_owned()));
    }

    // ── Nullable metadata tests ──────────────────────────────────

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

        let attachment_save = schema
            .sections
            .iter()
            .find(|s| s.id == "tools.attachment_save")
            .unwrap();
        assert!(attachment_save.optional_section);

        let runtime = schema.sections.iter().find(|s| s.id == "runtime").unwrap();
        assert!(!runtime.optional_section);

        // User telegram section is optional
        let user_schema = build_user_schema();
        let telegram = user_schema
            .sections
            .iter()
            .find(|s| s.id == "channels.telegram")
            .unwrap();
        assert!(telegram.optional_section);
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

        // User credential_refs are also a map collection
        let user_schema = build_user_schema();
        let creds = user_schema
            .sections
            .iter()
            .find(|s| s.id == "credential_refs")
            .unwrap();
        let collection = creds.collection.as_ref().unwrap();
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

        let auth_token = memory
            .fields
            .iter()
            .find(|f| f.path == "memory.auth_token")
            .unwrap();
        assert!(
            auth_token.nullable,
            "auth_token should be nullable (Option<String>)"
        );
    }

    #[test]
    fn nullable_fields_in_providers_advanced_overrides() {
        let schema = build_agent_schema();
        let providers = schema
            .sections
            .iter()
            .find(|s| s.id == "providers")
            .unwrap();
        let advanced = providers
            .subsections
            .iter()
            .find(|s| s.id == "providers.advanced")
            .unwrap();

        for field in &advanced.fields {
            assert!(
                field.nullable,
                "provider advanced override field '{}' should be nullable",
                field.path
            );
        }
    }

    #[test]
    fn nullable_fields_in_user_resources() {
        let schema = build_user_schema();
        let resources = schema
            .sections
            .iter()
            .find(|s| s.id == "resources")
            .unwrap();

        for field in &resources.fields {
            assert!(
                field.nullable,
                "user resource field '{}' should be nullable",
                field.path
            );
        }
    }

    #[test]
    fn nullable_fields_in_user_behavior() {
        let schema = build_user_schema();
        let behavior = schema.sections.iter().find(|s| s.id == "behavior").unwrap();

        for field in &behavior.fields {
            assert!(
                field.nullable,
                "user behavior field '{}' should be nullable",
                field.path
            );
        }
    }

    #[test]
    fn nullable_fields_in_runner_guest_images() {
        let schema = build_runner_schema();
        let guest_images = schema
            .sections
            .iter()
            .find(|s| s.id == "guest_images")
            .unwrap();

        // Firecracker config paths should be nullable.
        let fc_oxydra = guest_images
            .fields
            .iter()
            .find(|f| f.path == "guest_images.firecracker_oxydra_vm_config")
            .unwrap();
        assert!(fc_oxydra.nullable);

        let fc_shell = guest_images
            .fields
            .iter()
            .find(|f| f.path == "guest_images.firecracker_shell_vm_config")
            .unwrap();
        assert!(fc_shell.nullable);
    }

    // ── Field metadata quality tests ─────────────────────────────

    #[test]
    fn all_fields_have_labels_and_descriptions() {
        let state = test_state();
        let sources = build_dynamic_sources(&state);
        let response = ConfigSchemaResponse {
            agent: build_agent_schema(),
            runner: build_runner_schema(),
            user: build_user_schema(),
            dynamic_sources: sources,
        };

        fn check_section_fields(section: &SchemaSection, config_type: &str) {
            for field in &section.fields {
                assert!(
                    !field.label.is_empty(),
                    "{config_type} field '{}' has empty label",
                    field.path
                );
                assert!(
                    field.description.is_some(),
                    "{config_type} field '{}' has no description",
                    field.path
                );
            }
            for sub in &section.subsections {
                check_section_fields(sub, config_type);
            }
        }

        for section in &response.agent.sections {
            check_section_fields(section, "agent");
        }
        for section in &response.runner.sections {
            check_section_fields(section, "runner");
        }
        for section in &response.user.sections {
            check_section_fields(section, "user");
        }
    }

    #[test]
    fn select_fields_have_enum_options_or_dynamic_source() {
        fn check_section(section: &SchemaSection) {
            for field in &section.fields {
                if field.input_type == "select" {
                    assert!(
                        field.enum_options.is_some()
                            && !field.enum_options.as_ref().unwrap().is_empty(),
                        "select field '{}' should have non-empty enum_options",
                        field.path
                    );
                }
                if field.input_type == "select_dynamic" {
                    assert!(
                        field.dynamic_source.is_some(),
                        "select_dynamic field '{}' should have a dynamic_source",
                        field.path
                    );
                }
            }
            for sub in &section.subsections {
                check_section(sub);
            }
        }

        for section in &build_agent_schema().sections {
            check_section(section);
        }
        for section in &build_runner_schema().sections {
            check_section(section);
        }
        for section in &build_user_schema().sections {
            check_section(section);
        }
    }

    #[test]
    fn number_fields_with_constraints_have_valid_ranges() {
        fn check_section(section: &SchemaSection) {
            for field in &section.fields {
                if field.input_type == "number"
                    && let Some(ref c) = field.constraints
                    && let (Some(min), Some(max)) = (c.min, c.max)
                {
                    assert!(
                        min <= max,
                        "field '{}' has min ({}) > max ({})",
                        field.path,
                        min,
                        max
                    );
                }
            }
            for sub in &section.subsections {
                check_section(sub);
            }
        }

        for section in &build_agent_schema().sections {
            check_section(section);
        }
        for section in &build_runner_schema().sections {
            check_section(section);
        }
        for section in &build_user_schema().sections {
            check_section(section);
        }
    }

    // ── Group organization tests ─────────────────────────────────

    #[test]
    fn agent_sections_have_group_assignments() {
        let schema = build_agent_schema();
        // All non-general sections should have a group.
        for section in &schema.sections {
            if section.id != "general" {
                assert!(
                    section.group.is_some(),
                    "agent section '{}' should have a group assignment",
                    section.id
                );
            }
        }
    }

    // ── Serialization tests ──────────────────────────────────────

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

    #[test]
    fn schema_omits_empty_optional_fields() {
        // Verify skip_serializing_if works: a standard non-nullable field should
        // not serialize the `nullable` key when it is false.
        let field = fld("test", "Test", "text");
        let json = serde_json::to_value(&field).unwrap();
        assert!(
            json.get("nullable").is_none(),
            "nullable=false should be omitted via skip_serializing_if"
        );
        assert!(
            json.get("required").is_none(),
            "required=false should be omitted via skip_serializing_if"
        );
        assert!(
            json.get("allow_custom").is_none(),
            "allow_custom=false should be omitted via skip_serializing_if"
        );
    }

    // ── HTTP endpoint tests ──────────────────────────────────────

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

    #[tokio::test]
    async fn schema_endpoint_returns_correct_field_metadata() {
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

        // Verify the selection section has model_picker input type
        let agent_sections = json["data"]["agent"]["sections"].as_array().unwrap();
        let selection = agent_sections
            .iter()
            .find(|s| s["id"] == "selection")
            .expect("selection section should exist");
        let model_field = selection["fields"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["path"] == "selection.model")
            .expect("selection.model field should exist");
        assert_eq!(model_field["input_type"], "model_picker");
        assert_eq!(model_field["required"], true);
        assert_eq!(model_field["allow_custom"], true);

        // Verify dynamic sources contain all expected keys
        let ds = &json["data"]["dynamic_sources"];
        assert!(ds["tool_names"].is_array());
        assert!(ds["provider_types"].is_array());
        assert!(ds["sandbox_tiers"].is_array());
        assert!(ds["auth_modes"].is_array());
        assert!(ds["embedding_backends"].is_array());
        assert!(ds["model2vec_models"].is_array());
        assert!(ds["web_search_providers"].is_array());
        assert!(ds["input_modalities"].is_array());
        assert!(ds["safesearch_levels"].is_array());
        assert!(ds["timezone_suggestions"].is_array());
    }
}
