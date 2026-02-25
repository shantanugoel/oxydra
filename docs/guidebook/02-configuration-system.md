# Chapter 2: Configuration System

## Overview

Oxydra uses a deterministic, layered configuration system built on the `figment` crate. Every configuration value has a single, auditable source, and the system fails fast on invalid configuration at startup.

## Layered Precedence

Configuration sources are applied in this order, with later sources overriding earlier ones:

```
1. Built-in defaults (hardcoded in Rust structs)
2. System config    (/etc/oxydra/agent.toml, /etc/oxydra/providers.toml)
3. User config      (~/.config/oxydra/agent.toml, ~/.config/oxydra/providers.toml)
4. Workspace config (.oxydra/agent.toml, .oxydra/providers.toml)
5. Environment vars (OXYDRA__RUNTIME__MAX_TURNS, etc.)
6. CLI flags        (--model, etc.)
```

The implementation in `tui/src/bootstrap.rs` uses `figment` to merge these layers:

```rust
// Pseudocode of actual loading order
Figment::new()
    .merge(Serialized::defaults(AgentConfig::default()))
    .merge(Toml::file("/etc/oxydra/agent.toml"))
    .merge(Toml::file("~/.config/oxydra/agent.toml"))
    .merge(Toml::file(".oxydra/agent.toml"))
    .merge(Env::prefixed("OXYDRA__").split("__"))
    .merge(cli_overrides)
```

Environment variables use double-underscore nesting: `OXYDRA__RUNTIME__MAX_TURNS=10` maps to `runtime.max_turns = 10`.

Figment also supports profiled config files. If a TOML file contains a `[default]` or `[global]` key, it's treated as profiled and the active profile is selected (defaulting to `default`).

## Configuration Structs

All config types live in `types::config` and derive `Serialize`/`Deserialize`.

### `AgentConfig` (Root)

```rust
pub struct AgentConfig {
    pub config_version: String,        // Must match major version 1
    pub runtime: RuntimeConfig,
    pub memory: MemoryConfig,
    pub selection: ProviderSelection,  // Active provider name + model
    pub providers: ProviderConfigs,    // Named provider registry
    pub reliability: ReliabilityConfig,
    pub catalog: CatalogConfig,        // Catalog resolution and validation
    pub tools: ToolsConfig,
    pub scheduler: SchedulerConfig,    // Scheduled task system (disabled by default)
}
```

### `RuntimeConfig`

Controls the agent loop's operational limits:

| Field | Default | Purpose |
|-------|---------|---------|
| `max_turns` | 8 | Maximum tool-calling iterations per session turn |
| `turn_timeout_secs` | 60 | Per-turn timeout in seconds |
| `max_cost` | (none) | Optional cost budget per session |
| `context_budget` | (nested) | Context window management (trigger ratio, safety buffer, fallback max tokens) |
| `summarization` | (nested) | Rolling summary trigger/target ratios |

### `MemoryConfig`

Controls persistence and retrieval:

| Field | Default | Purpose |
|-------|---------|---------|
| `enabled` | false | Whether memory persistence is active |
| `remote_url` | (none) | Optional Turso remote URL |
| `auth_token` | (none) | Required if `remote_url` is set |
| `retrieval.top_k` | 8 | Number of retrieval results per query |
| `retrieval.vector_weight` | 0.7 | Weight for vector similarity (must sum to 1.0 with fts_weight) |
| `retrieval.fts_weight` | 0.3 | Weight for full-text search scoring |

### `ProviderSelection`

```rust
pub struct ProviderSelection {
    pub provider: ProviderId,  // Name of a registry entry (e.g., "openai", "my-proxy")
    pub model: ModelId,        // e.g., "gpt-4o", "claude-3-5-sonnet-latest"
}
```

The `provider` field references a named entry in the provider registry, not a hardcoded provider type. This allows multiple instances of the same provider type (e.g., two OpenAI-compatible endpoints) with different names.

### `ProviderConfigs` (Provider Registry)

```rust
pub struct ProviderConfigs {
    pub registry: BTreeMap<String, ProviderRegistryEntry>,
}
```

Each key in `registry` is a user-chosen provider instance name (referenced by `ProviderSelection.provider`). The default registry ships with `"openai"` and `"anthropic"` entries.

### `ProviderRegistryEntry`

```rust
pub struct ProviderRegistryEntry {
    pub provider_type: ProviderType,           // openai, anthropic, gemini, openai_responses
    pub base_url: Option<String>,              // Custom endpoint URL
    pub api_key: Option<String>,               // Explicit API key (highest priority)
    pub api_key_env: Option<String>,           // Custom env var for API key
    pub extra_headers: Option<BTreeMap<String, String>>,  // Additional HTTP headers
    pub catalog_provider: Option<String>,       // Override catalog namespace for model validation
    pub reasoning: Option<bool>,               // Override for unknown model reasoning capability
    pub max_input_tokens: Option<u32>,         // Override max input tokens for unknown models
    pub max_output_tokens: Option<u32>,        // Override max output tokens for unknown models
    pub max_context_tokens: Option<u32>,       // Override max context tokens for unknown models
}
```

### `ProviderType`

Discriminant for the underlying provider implementation:

```rust
pub enum ProviderType {
    Openai,           // OpenAI Chat Completions API (/v1/chat/completions)
    Anthropic,        // Anthropic Messages API (/v1/messages)
    Gemini,           // Google Gemini API (v1beta/models/:generateContent)
    OpenaiResponses,  // OpenAI Responses API (/v1/responses) with stateful chaining
}
```

### `CatalogConfig`

Controls model catalog resolution and validation:

```rust
pub struct CatalogConfig {
    pub skip_catalog_validation: bool,  // Default: false
    pub pinned_url: Option<String>,     // Optional override for pinned snapshot URL
}
```

| Field | Default | Purpose |
|-------|---------|---------|
| `skip_catalog_validation` | `false` | When `true`, unknown models are allowed through with synthetic default capabilities |
| `pinned_url` | (none) | Override the default URL for fetching the pinned catalog snapshot |

When `skip_catalog_validation` is enabled, models not found in any loaded catalog are assigned a synthetic descriptor with sensible defaults (`supports_streaming=true`, `supports_tools=true`, context=128K, output=16K). Per-registry-entry capability overrides can fine-tune these defaults.

**Catalog cache locations:**
- User-level: `~/.config/oxydra/model_catalog.json`
- Workspace-level: `.oxydra/model_catalog.json`

#### Per-Registry-Entry Capability Overrides

Each `ProviderRegistryEntry` supports optional capability fields used when `skip_catalog_validation` is on and the model is not in the catalog:

| Field | Type | Purpose |
|-------|------|---------|
| `reasoning` | `Option<bool>` | Whether the model supports reasoning/thinking |
| `max_input_tokens` | `Option<u32>` | Override max input tokens (default: 128000) |
| `max_output_tokens` | `Option<u32>` | Override max output tokens (default: 16384) |
| `max_context_tokens` | `Option<u32>` | Override max context tokens (default: 128000) |

### `ToolsConfig`

Controls tool-specific behavior:

```rust
pub struct ToolsConfig {
    pub web_search: Option<WebSearchConfig>,
    pub shell: Option<ShellConfig>,
}
```

#### `WebSearchConfig` fields

| Field | Type | Purpose |
|-------|------|---------|
| `provider` | `String` | `"duckduckgo"` (default), `"google"`, or `"searxng"` |
| `base_url` | `String` | Override default base URL for the selected provider |
| `base_urls` | `String` | Comma-separated fallback base URLs |
| `api_key_env` | `String` | Env var name holding Google API key |
| `engine_id_env` | `String` | Env var name holding Google engine/CX ID |
| `query_params` | `String` | Extra query parameters (`key=value&key2=value2`) |
| `engines` | `String` | SearxNG: comma-separated engines |
| `categories` | `String` | SearxNG: categories |
| `safesearch` | `u8` | SearxNG: safesearch level (0–2) |
| `egress_allowlist` | `Vec<String>` | Hosts allowed to resolve to private/loopback IPs (see below) |

#### Local service allowlist (`egress_allowlist`)

By default all private and loopback IP ranges are blocked (SSRF protection). Use `egress_allowlist` to reach self-hosted services such as a local SearxNG instance:

```toml
[tools.web_search]
provider = "searxng"
base_url = "http://localhost:8888"
egress_allowlist = ["localhost:8888"]

# Multiple entries:
# egress_allowlist = ["localhost:8888", "192.168.1.50:9000"]
```

Entries may be `hostname`, `hostname:port`, or bare IP addresses. Only the explicitly listed hosts bypass the block — all other private addresses remain blocked. This applies to both `web_search` and `web_fetch`. See [Security Model](07-security-model.md#local-service-allowlist) for details.

### `SchedulerConfig`

Controls the scheduled task system:

```rust
pub struct SchedulerConfig {
    pub enabled: bool,                      // Default: false
    pub poll_interval_secs: u64,            // Default: 15
    pub max_concurrent: usize,              // Default: 2
    pub max_schedules_per_user: usize,      // Default: 50
    pub max_turns: usize,                   // Default: 10 (operator-only, not LLM-controlled)
    pub max_cost: f64,                      // Default: 0.50 (operator-only, not LLM-controlled)
    pub max_run_history: usize,             // Default: 20
    pub min_interval_secs: u64,             // Default: 60
    pub default_timezone: String,           // Default: "Asia/Kolkata"
    pub auto_disable_after_failures: u32,   // Default: 5
}
```

| Field | Default | Purpose |
|-------|---------|---------|
| `enabled` | `false` | Whether scheduling is active |
| `poll_interval_secs` | `15` | How often the executor checks for due schedules |
| `max_concurrent` | `2` | Maximum concurrent scheduled runs |
| `max_schedules_per_user` | `50` | Per-user schedule limit |
| `max_turns` | `10` | Max turns per scheduled run (operator-only) |
| `max_cost` | `0.50` | Max cost per scheduled run (operator-only) |
| `max_run_history` | `20` | Run history entries retained per schedule |
| `min_interval_secs` | `60` | Minimum interval between runs (anti-abuse) |
| `default_timezone` | `"Asia/Kolkata"` | Default timezone for cron schedules |
| `auto_disable_after_failures` | `5` | Auto-disable after N consecutive failures |

**Key design decisions:**
- Scheduling is disabled by default and must be explicitly enabled.
- `max_turns` and `max_cost` are operator-only configuration — they are not exposed in any tool schema. The LLM cannot override execution budgets for scheduled runs.
- The scheduler requires the memory backend to be enabled, as schedule definitions and run history are stored in the same libSQL database.

### `ShellConfig`

Configures the shell command allowlist and operator policy:

```rust
pub struct ShellConfig {
    pub allow: Option<Vec<String>>,       // Commands to add (supports glob patterns)
    pub deny: Option<Vec<String>>,        // Commands to remove (supports glob patterns)
    pub replace_defaults: Option<bool>,   // Replace default allowlist entirely (default: false)
    pub allow_operators: Option<bool>,    // Allow shell operators &&, ||, | etc. (default: false)
    pub env_keys: Option<Vec<String>>,    // Env var names to forward into the shell container
}
```

| Field | Default | Purpose |
|-------|---------|---------|
| `allow` | (none) | Additional commands or glob patterns (e.g., `npm*`, `cargo-*`) to add to the allowlist |
| `deny` | (none) | Commands or glob patterns to remove from the allowlist |
| `replace_defaults` | `false` | If `true`, `allow` replaces the built-in defaults entirely |
| `allow_operators` | `false` | If `true`, shell operators (`&&`, `\|\|`, `\|`, `;`, `>`, `<`, etc.) are permitted |
| `env_keys` | (none) | Environment variable names to forward into the shell-vm container. API keys from the agent config are **not** forwarded to the shell by default — only keys listed here are. Additionally, CLI `--env` / `--env-file` entries with a `SHELL_` prefix are forwarded with the prefix stripped (e.g. `SHELL_NPM_TOKEN` → `NPM_TOKEN`). |

Glob patterns support `*` as prefix, suffix, or both: `npm*` matches `npm`, `npmrc`; `*test*` matches `pytest`, `testing`.

### `ReliabilityConfig`

```rust
pub struct ReliabilityConfig {
    pub max_attempts: u32,    // Default: 3
    pub backoff_base_ms: u64, // Default: 250
    pub backoff_max_ms: u64,  // Default: 2000
    pub jitter: bool,         // Default: false
}
```

## Credential Resolution

API keys follow a 4-tier resolution chain, implemented in `provider/src/lib.rs` via `resolve_api_key_for_entry()`:

```
1. Explicit config      →  providers.registry.<name>.api_key in agent.toml
2. Custom env var       →  providers.registry.<name>.api_key_env (e.g. "CORP_OPENAI_KEY")
3. Provider-type env var →  OPENAI_API_KEY, ANTHROPIC_API_KEY, or GEMINI_API_KEY
4. Generic fallback     →  API_KEY (development only)
```

The resolution is deterministic: the first non-empty value wins. Secrets are never logged or included in trace spans.

## Catalog Provider Namespace

Each registry entry has an `effective_catalog_provider()` that determines which namespace in the model catalog is used for model validation. The defaults are:

| Provider Type | Default Catalog Provider |
|---------------|--------------------------|
| `Openai` | `"openai"` |
| `Anthropic` | `"anthropic"` |
| `Gemini` | `"google"` |
| `OpenaiResponses` | `"openai"` |

This can be overridden with the `catalog_provider` field, which is useful when a proxy serves models from a different provider's catalog.

## Startup Validation

`AgentConfig::validate()` runs at startup and enforces:

1. **Version gating** — `config_version` must have major version `1` (parses semver-like strings)
2. **Provider registry resolution** — `selection.provider` must match a key in `providers.registry`
3. **Model validity** — `selection.model` must not be empty
4. **Catalog validation** — `validate_model_in_catalog()` checks the selected model exists under the resolved registry entry's catalog provider namespace
5. **Runtime limits** — `max_turns > 0`, `turn_timeout_secs > 0`
6. **Memory coherence** — if `remote_url` is set, `auth_token` is required; retrieval weights must sum to `1.0`
7. **Reliability bounds** — `backoff_base <= backoff_max`, both `> 0`

If validation fails, the system exits immediately with a descriptive error rather than entering an undefined state.

## Runner Configuration

Runner configuration is separate from agent configuration and uses its own types in `types::runner`.

### `RunnerGlobalConfig` (Operator-scoped)

```rust
pub struct RunnerGlobalConfig {
    pub workspace_root: PathBuf,
    pub users: BTreeMap<String, UserRegistration>,
    pub default_tier: SandboxTier,
    pub guest_images: GuestImageConfig,  // oxydra-vm and shell-vm image paths/tags
}
```

This file contains only infrastructure-uniform settings that cannot meaningfully vary per user.

### `RunnerUserConfig` (User-scoped)

```rust
pub struct RunnerUserConfig {
    pub mounts: MountOverrides,
    pub resources: ResourceLimits,
    pub credentials: CredentialReferences,
    pub overrides: BehaviorOverrides,
    pub channels: ChannelsConfig,     // External channel configuration (Telegram, etc.)
}
```

Per-user files carry mount paths, resource limits, credential references, behavioral overrides, and external channel configuration used to launch that user's VM pair. The `channels` field configures external channel adapters (Telegram bot tokens, authorized senders) — see Chapter 12 for details.

## Config File Locations

| File | Location | Purpose |
|------|----------|---------|
| `agent.toml` | System/user/workspace | Agent runtime configuration |
| `providers.toml` | System/user/workspace | Provider-specific settings (keys, URLs) |
| `runner.toml` | `.oxydra/runner.toml` | Runner global config (operator only) |
| Per-user config | Path from `runner.toml` user map | User-specific sandbox settings |

## Example Configuration

```toml
# .oxydra/agent.toml
config_version = "1.0"

[runtime]
max_turns = 10
turn_timeout_secs = 120

[runtime.context_budget]
trigger_ratio              = 0.85
safety_buffer_tokens       = 1024
fallback_max_context_tokens = 128000

[runtime.summarization]
target_ratio = 0.5
min_turns    = 6

[memory]
enabled = true
# Local DB is stored at <workspace>/.oxydra/memory.db by convention.
# For remote mode, set remote_url and auth_token instead.

[memory.retrieval]
top_k         = 8
vector_weight = 0.7
fts_weight    = 0.3

[selection]
provider = "anthropic"
model = "claude-3-5-sonnet-latest"

[reliability]
max_attempts    = 3
backoff_base_ms = 250
backoff_max_ms  = 2000
jitter          = false

# --- Provider registry ---
# Each entry is a named provider instance. The [selection].provider
# field references a key from this registry.

[providers.registry.openai]
provider_type = "openai"
# base_url = "https://api.openai.com"
# api_key_env = "OPENAI_API_KEY"      # default for openai type

[providers.registry.anthropic]
provider_type = "anthropic"
# base_url = "https://api.anthropic.com"
# api_key_env = "ANTHROPIC_API_KEY"   # default for anthropic type

# --- Additional provider examples ---

# Google Gemini
# [providers.registry.gemini]
# provider_type = "gemini"
# api_key_env = "GEMINI_API_KEY"

# OpenAI Responses API (stateful session chaining)
# [providers.registry.openai-responses]
# provider_type = "openai_responses"
# api_key_env = "OPENAI_API_KEY"

# Corporate proxy serving OpenAI-compatible models
# [providers.registry.my-proxy]
# provider_type = "openai"
# base_url = "https://llm-proxy.corp.internal/v1"
# api_key_env = "CORP_LLM_KEY"
# catalog_provider = "openai"  # validate models against OpenAI catalog

# --- Shell command allowlist ---

# [tools.shell]
# allow = ["npm", "npx", "curl", "wget", "jq", "make", "docker", "rg"]
# deny = ["rm"]
# replace_defaults = false
# allow_operators = false
# env_keys = ["NPM_TOKEN", "GH_TOKEN"]  # Forward these env vars into the shell container

# --- Scheduler ---
# Enable and configure the scheduler for automated recurring/one-off tasks.
# [scheduler]
# enabled = true
# poll_interval_secs = 15
# max_concurrent = 2
# max_schedules_per_user = 50
# max_turns = 10            # Operator-only budget per scheduled run
# max_cost = 0.50           # Operator-only cost cap per scheduled run
# max_run_history = 20
# min_interval_secs = 60
# default_timezone = "Asia/Kolkata"
# auto_disable_after_failures = 5

# --- Catalog settings ---

[catalog]
# skip_catalog_validation = true  # allow unknown models with synthetic caps

# --- OpenRouter with skip_catalog_validation ---
# [providers.registry.openrouter]
# provider_type = "openai"
# base_url = "https://openrouter.ai/api/v1"
# api_key_env = "OPENROUTER_API_KEY"
# catalog_provider = "openrouter"
# reasoning = true
# max_context_tokens = 200000
# max_output_tokens = 8192
```
