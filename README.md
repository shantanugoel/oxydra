# Oxydra

Current repository status: foundational runtime/tooling, runner isolation surfaces, and phase-12 channel/gateway/TUI path are implemented and tested at crate level.

## What is implemented right now

- Typed core contracts in `types` (provider/tool/runtime/memory/channel/runner envelopes).
- Provider/runtime/tool/memory stack with config validation and deterministic config precedence.
- Runner isolation orchestration model (`micro_vm`, `container`, `process`) and connect-only TUI probe flow.
- Channel trait in `types`, channel registry in `channels`, gateway daemon implementation in `gateway`, and TUI adapter/websocket client in `tui`.

## Quick start (phase-12 developer flow)

### 1) Create config files

```bash
mkdir -p .oxydra/users
cp examples/config/agent.toml .oxydra/agent.toml
```

Create `.oxydra/runner.toml`:

```toml
workspace_root = ".oxydra/workspaces"
default_tier = "process"

[guest_images]
oxydra_vm = "oxydra-vm:latest"
shell_vm = "shell-vm:latest"

[users.alice]
config_path = ".oxydra/users/alice.toml"
```

Create `.oxydra/users/alice.toml`:

```toml
[behavior]
sandbox_tier = "process"
shell_enabled = false
browser_enabled = false
```

Set provider credentials (example):

```bash
export OPENAI_API_KEY=your-key
```

### 2) Run phase-12 verification tests

```bash
cargo fmt --all --check && \
cargo clippy -p types -p channels -p gateway -p tui -p runner --all-targets --all-features -- -D warnings && \
cargo test -p types -p channels -p gateway -p tui -p runner -- --test-threads=1
```

### 3) Run runner start/connect commands

Build bundled runner binaries once (includes `runner` and `oxydra-vm`):

```bash
cargo build -p runner --bins
```

Start flow (uses `Runner::start_user`):

```bash
cargo run -p runner -- --config .oxydra/runner.toml --user alice --insecure
```

Connect-only TUI probe flow (uses `Runner::connect_tui`):

```bash
cargo run -p runner -- --tui --config .oxydra/runner.toml --user alice
```

Notes:
- `runner` CLI now uses clap for argument parsing and `--help` output.
- Process-tier start mode launches a bundled `oxydra-vm` executable (or a path supplied through `OXYDRA_VM_PROCESS_EXECUTABLE`).
- `oxydra-vm` writes `<workspace_root>/tmp/gateway-endpoint`, which enables runner `--tui` connect-only probing.
- Container/microVM tiers still require runnable guest images (`oxydra-vm`/`shell-vm`) in the configured environment.
- `--tui` is connect-only by design; it probes a running gateway endpoint for the user.
- If no running guest marker exists, runner exits with a clear error (`NoRunningGuest`).
- `cargo run -p runner -- --help` prints all runner CLI options.

## Configuration guide

### Config files to create

| File | Required | Purpose |
|---|---|---|
| `.oxydra/runner.toml` | **Required** for `runner` CLI | Global/operator runner config (workspace root, users, default tier, guest images). |
| `.oxydra/users/<user-id>.toml` | **Required** when referenced from `runner.toml` | Per-user overrides (mounts/resources/behavior/credential references). |
| `.oxydra/agent.toml` | Optional (recommended) | Runtime/provider/memory/reliability config loaded by vm bootstrap path. |
| `.oxydra/providers.toml` | Optional | Split provider credentials/endpoints out of `agent.toml` if preferred. |

### Agent config precedence (lowest -> highest)

1. Built-in defaults (`AgentConfig::default`)
2. `/etc/oxydra/agent.toml` + `/etc/oxydra/providers.toml`
3. `~/.config/oxydra/agent.toml` + `~/.config/oxydra/providers.toml`
4. `./.oxydra/agent.toml` + `./.oxydra/providers.toml`
5. Environment (`OXYDRA__...`)
6. CLI override struct (`CliOverrides`)

### Runner global config (`runner.toml`)

| Key | Required | Default | Notes |
|---|---|---|---|
| `workspace_root` | No | `.oxydra/workspaces` | Root where `<workspace_root>/<user_id>/{shared,tmp,vault}` is materialized. |
| `default_tier` | No | `micro_vm` | `micro_vm`, `container`, or `process`. |
| `guest_images.oxydra_vm` | No | `oxydra-vm:latest` | Guest image reference for runtime vm. |
| `guest_images.shell_vm` | No | `shell-vm:latest` | Guest image reference for shell/browser vm. |
| `users.<id>.config_path` | **Yes** (per user) | — | Per-user config path (absolute or relative to runner config directory). |

### Runner per-user config (`users/<id>.toml`)

| Key | Required | Default | Notes |
|---|---|---|---|
| `mounts.shared/tmp/vault` | No | unset | Optional mount path overrides. |
| `resources.max_vcpus` | No | unset | Must be > 0 when set. |
| `resources.max_memory_mib` | No | unset | Must be > 0 when set. |
| `resources.max_processes` | No | unset | Must be > 0 when set. |
| `credential_refs.<name>` | No | empty map | Key and value must both be non-empty when present. |
| `behavior.sandbox_tier` | No | inherit global `default_tier` | User-specific tier override. |
| `behavior.shell_enabled` | No | unset | Further constrains shell capability. |
| `behavior.browser_enabled` | No | unset | Further constrains browser capability. |

### Agent runtime/provider config (`agent.toml`)

| Key | Required | Default |
|---|---|---|
| `config_version` | No | `1.0.0` |
| `selection.provider` | No | `openai` |
| `selection.model` | No | `gpt-4o-mini` |
| `runtime.turn_timeout_secs` | No | `60` |
| `runtime.max_turns` | No | `8` |
| `runtime.max_cost` | No | unset |
| `runtime.context_budget.trigger_ratio` | No | `0.85` |
| `runtime.context_budget.safety_buffer_tokens` | No | `1024` |
| `runtime.context_budget.fallback_max_context_tokens` | No | `128000` |
| `runtime.summarization.target_ratio` | No | `0.5` |
| `runtime.summarization.min_turns` | No | `6` |
| `memory.enabled` | No | `false` |
| `memory.db_path` | No | `.oxydra/memory.db` |
| `memory.remote_url` | No | unset |
| `memory.auth_token` | Conditionally required | unset |
| `memory.retrieval.top_k` | No | `8` |
| `memory.retrieval.vector_weight` | No | `0.7` |
| `memory.retrieval.fts_weight` | No | `0.3` |
| `providers.openai.base_url` | No | `https://api.openai.com` |
| `providers.openai.api_key` | No | unset |
| `providers.anthropic.base_url` | No | `https://api.anthropic.com` |
| `providers.anthropic.api_key` | No | unset |
| `reliability.max_attempts` | No | `3` |
| `reliability.backoff_base_ms` | No | `250` |
| `reliability.backoff_max_ms` | No | `2000` |
| `reliability.jitter` | No | `false` |

Conditional rule:
- If `memory.enabled=true` and `memory.remote_url` is set, `memory.auth_token` must also be set.

### Environment override format

Use `OXYDRA__` with `__` path separators:

```bash
OXYDRA__SELECTION__PROVIDER=anthropic
OXYDRA__SELECTION__MODEL=claude-3-5-haiku-latest
OXYDRA__RUNTIME__MAX_TURNS=12
OXYDRA__MEMORY__ENABLED=true
```

Credential resolution order:
1. explicit `api_key` in config
2. provider env (`OPENAI_API_KEY` / `ANTHROPIC_API_KEY`)
3. fallback `API_KEY`

## Documentation map

- Canonical architecture/research brief: `oxydra-project-brief.md`
- Chaptered project guide: `docs/README.md` (split by chapter and roadmap)
