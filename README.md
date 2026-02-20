# Oxydra Configuration

This repository now includes a Phase 7 config loader in `crates/cli` with deterministic layering, startup validation, and provider switching.

## Quick start

1. Copy the example file:
   - `examples/config/agent.toml` -> `./.oxydra/agent.toml`
2. Set credentials:
   - OpenAI: `OPENAI_API_KEY=...`
   - Anthropic: `ANTHROPIC_API_KEY=...`
3. Optionally override fields with `OXYDRA__...` environment variables.

## Config precedence (lowest -> highest)

1. Built-in defaults (`types::AgentConfig::default()`)
2. `/etc/oxydra/agent.toml` + `/etc/oxydra/providers.toml`
3. `~/.config/oxydra/agent.toml` + `~/.config/oxydra/providers.toml`
4. `./.oxydra/agent.toml` + `./.oxydra/providers.toml`
5. Environment variables (`OXYDRA__...`)
6. CLI overrides (`CliOverrides`)

## Supported schema (`types::AgentConfig`)

- `config_version` (must be major version `1`)
- `selection.provider` (`openai` or `anthropic`)
- `selection.model` (must exist in pinned model catalog)
- `runtime.turn_timeout_secs` (> 0)
- `runtime.max_turns` (> 0)
- `runtime.max_cost` (optional)
- `providers.openai.api_key` / `providers.openai.base_url`
- `providers.anthropic.api_key` / `providers.anthropic.base_url`
- `reliability.max_attempts` (> 0)
- `reliability.backoff_base_ms` and `reliability.backoff_max_ms` (`base <= max`, both > 0)
- `reliability.jitter` (currently stored in config; retry wrapper currently uses bounded exponential backoff)

## Environment override format

Use `OXYDRA__` prefix and `__` as path separators:

- `OXYDRA__SELECTION__PROVIDER=anthropic`
- `OXYDRA__SELECTION__MODEL=claude-3-5-haiku-latest`
- `OXYDRA__RUNTIME__MAX_TURNS=12`
- `OXYDRA__PROVIDERS__OPENAI__BASE_URL=https://openrouter.ai/api`

## Credentials resolution

Provider API keys resolve as:

1. Explicit `api_key` in config
2. Provider-specific env var (`OPENAI_API_KEY` / `ANTHROPIC_API_KEY`)
3. Generic fallback `API_KEY`

## Profile-based TOML

`agent.toml` can be flat (example file) or profile-based with top-level profile tables:

```toml
[default.selection]
provider = "openai"
model = "gpt-4o-mini"

[prod.selection]
provider = "anthropic"
model = "claude-3-5-sonnet-latest"
```

When a profile is selected, values are resolved from that profile with fallback to `default`.

