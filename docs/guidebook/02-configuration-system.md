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
    pub selection: ProviderSelection,  // Active provider + model
    pub providers: ProviderConfigs,    // Per-provider settings
    pub reliability: ReliabilityConfig,
}
```

### `RuntimeConfig`

Controls the agent loop's operational limits:

| Field | Default | Purpose |
|-------|---------|---------|
| `max_turns` | 8 | Maximum tool-calling iterations per session turn |
| `turn_timeout_secs` | 60 | Per-turn timeout in seconds |
| `max_cost` | (none) | Optional cost budget per session |
| `stream_buffer_size` | 32 | Bounded mpsc channel capacity for streaming |
| `summarization` | (nested) | Rolling summary trigger/target ratios |

### `MemoryConfig`

Controls persistence and retrieval:

| Field | Default | Purpose |
|-------|---------|---------|
| `enabled` | true | Whether memory persistence is active |
| `db_path` | `"oxydra_memory.db"` | Local libSQL database file path |
| `remote_url` | (none) | Optional Turso remote URL |
| `auth_token` | (none) | Required if `remote_url` is set |
| `retrieval.enabled` | true | Whether hybrid retrieval is active |
| `retrieval.max_results` | 5 | Maximum retrieval results per query |
| `retrieval.vector_weight` | 0.7 | Weight for vector similarity (must sum to 1.0 with fts_weight) |
| `retrieval.fts_weight` | 0.3 | Weight for full-text search scoring |

### `ProviderSelection`

```rust
pub struct ProviderSelection {
    pub provider: String,  // "openai" or "anthropic"
    pub model: String,     // e.g., "gpt-4o", "claude-3-5-sonnet-latest"
}
```

### `ProviderConfigs`

```rust
pub struct ProviderConfigs {
    pub openai: OpenAIConfig,
    pub anthropic: AnthropicConfig,
}
```

Each provider config carries:
- `api_key: Option<String>` — explicit key (highest priority)
- `base_url: Option<String>` — custom endpoint (for proxies, OpenRouter, etc.)

### `ReliabilityConfig`

```rust
pub struct ReliabilityConfig {
    pub max_attempts: u32,    // Default: 3
    pub backoff_base_ms: u64, // Default: 1000
    pub backoff_max_ms: u64,  // Default: 30000
}
```

## Credential Resolution

API keys follow a strict 3-tier resolution chain, implemented in `provider/src/lib.rs`:

```
1. Explicit config  →  providers.openai.api_key in agent.toml
2. Provider env var →  OPENAI_API_KEY or ANTHROPIC_API_KEY
3. Generic fallback →  API_KEY (development only)
```

The resolution is deterministic: the first non-empty value wins. Secrets are never logged or included in trace spans.

## Startup Validation

`AgentConfig::validate()` runs at startup and enforces:

1. **Version gating** — `config_version` must have major version `1` (parses semver-like strings)
2. **Provider validity** — `selection.provider` must be `"openai"` or `"anthropic"`
3. **Model validity** — `selection.model` must not be empty
4. **Runtime limits** — `max_turns > 0`, `turn_timeout_secs > 0`
5. **Memory coherence** — if `remote_url` is set, `auth_token` is required; retrieval weights must sum to `1.0`
6. **Reliability bounds** — `backoff_base <= backoff_max`, both `> 0`

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
}
```

Per-user files carry mount paths, resource limits, credential references, and behavioral overrides used to launch that user's VM pair.

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

[memory]
enabled = true
db_path = "oxydra_memory.db"

[memory.retrieval]
enabled = true
max_results = 5
vector_weight = 0.7
fts_weight = 0.3

[selection]
provider = "anthropic"
model = "claude-3-5-sonnet-latest"

[reliability]
max_attempts = 3
backoff_base_ms = 1000
backoff_max_ms = 30000
```

```toml
# .oxydra/providers.toml
[openai]
# api_key resolved from OPENAI_API_KEY env var

[anthropic]
# api_key resolved from ANTHROPIC_API_KEY env var
```
