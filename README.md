![Oxydra](./static/oxydra-1.png)

# Oxydra

A high-performance AI agent orchestrator written in Rust. Oxydra provides a modular runtime for building, isolating, and orchestrating AI agents with provider-agnostic LLM integration, tool execution, persistent memory, and multi-agent coordination.

## Features

- **Provider-agnostic LLM integration** — OpenAI, Anthropic, Google Gemini, and OpenAI Responses API with SSE streaming, automatic retries, and a pinned model catalog
- **Multi-modal input** — Users can send images, audio, video, PDFs, and documents from supported channels (Telegram); media is validated against model and provider capabilities then forwarded as inline attachments to the LLM
- **Tool system** — `#[tool]` proc-macro for defining tools with automatic JSON Schema generation and safety tiers, plus path-hardened virtual workspace handling (`/shared`, `/tmp`, `/vault`) across file, vault, and media tools
- **Agent runtime** — Turn-loop state machine with tool dispatch, self-correction, context budget management, cost limits, and a built-in autonomy protocol (plan → recall → execute → reflect → learn) with structured retry escalation
- **Persistent memory** — Hybrid retrieval (native libSQL vector index + FTS5) over libSQL with model2vec semantic embeddings (configurable potion-8m/32m), conversation summarization, LLM-callable memory tools (search, save, update, delete) with tag-based filtering and backend-owned timestamps, and a session-scoped working-memory scratchpad for multi-step task coordination
- **Scheduler** — Durable one-off and periodic task scheduling with cron/interval cadences, origin-aware notification routing (back to the creating channel), full run output storage, run history tools, failure notifications, and automatic execution through the same agent runtime policy envelope
- **Isolation tiers** — MicroVM, container, and process-level sandboxing via the runner/guest model (MicroVM is currently experimental; container is the recommended tier today)
- **Multi-session gateway** — Protocol v2 WebSocket gateway with per-user multi-session support, session persistence, bounded FIFO top-level turn queueing (default 10 concurrent turns/user), session caps (default 50), idle TTL cleanup (default 48h), and pluggable channel adapters
- **Session lifecycle** — Create, list, and switch between named sessions from the TUI (`/new`, `/sessions`, `/switch`) or via the `--session` CLI flag
- **External channels (Telegram)** — In-process Telegram bot adapter with pre-configured sender authorization, forum topic threading, and live edit-message streaming responses
- **Configuration** — Layered config (files, env vars, CLI overrides) with deterministic precedence and validation

## CAUTION - WIP

Not ready for use by anyone except me, yet! Unless you love dipping your toes in pre-pre-alpha software.

## Quick start

### 1. Create configuration

```bash
mkdir -p .oxydra/users
cp examples/config/agent.toml .oxydra/agent.toml
cp examples/config/runner.toml .oxydra/runner.toml
cp examples/config/runner-user.toml .oxydra/users/alice.toml
```

Set your provider API key:

```bash
export OPENAI_API_KEY=your-key
# or
export ANTHROPIC_API_KEY=your-key
# or
export GEMINI_API_KEY=your-key
```

### 1b. Apply bare-minimum runner settings (recommended)

MicroVM support is currently experimental; for most users, set the container tier and image references explicitly in `.oxydra/runner.toml`, and keep the `--user` value aligned with your configured user entry:

```toml
default_tier = "container"

[guest_images]
oxydra_vm = "ghcr.io/shantanugoel/oxydra-vm:<tag>"
shell_vm  = "ghcr.io/shantanugoel/shell-vm:<tag>"

[users.alice] # replace "alice" if you want a different user ID
config_path = "users/alice.toml"
```

Set the provider + model in `.oxydra/agent.toml`, and bind credentials via `api_key_env`:

```toml
[selection]
provider = "openai"
model = "gpt-4o-mini"

[providers.registry.openai]
provider_type = "openai"
api_key_env = "OPENAI_API_KEY"
```

Read the inline comments in `examples/config/agent.toml`, `examples/config/runner.toml`, and `examples/config/runner-user.toml` to review other recommended knobs before production use.

### 2. Build

```bash
cargo build --workspace
```

### 3. Build guest Docker images

The container and MicroVM isolation tiers run `oxydra-vm` and `shell-daemon` inside Docker containers. MicroVM support is currently experimental; the recommended path is the container tier. You must build the guest images before using these tiers.

#### Option A — Local cross-compilation (recommended, faster)

This cross-compiles the guest binaries on your host machine and packages them into lightweight Docker images. It requires [`cargo-zigbuild`](https://github.com/rust-cross/cargo-zigbuild) and [`zig`](https://ziglang.org/download/):

```bash
# Install prerequisites (once)
cargo install cargo-zigbuild
brew install zig            # macOS — see ziglang.org for other platforms
```

Then build for the target architecture:

```bash
./scripts/build-guest-images.sh arm64          # Apple Silicon / ARM servers
./scripts/build-guest-images.sh amd64          # Intel / AMD servers
./scripts/build-guest-images.sh arm64 v0.2.0   # with a custom tag
```

#### Option B — Full in-Docker build (slower, no local toolchain needed)

Builds everything inside a multi-stage Docker container. No cross-compilation toolchain required, but rebuilds the Rust toolchain each time:

```bash
./scripts/build-guest-images-in-docker.sh          # default tag: latest
./scripts/build-guest-images-in-docker.sh v0.2.0   # custom tag
```

Both options produce two images: `oxydra-vm:latest` (agent runtime) and `shell-vm:latest` (shell/browser sidecar). These names match the defaults in `examples/config/runner.toml`.

To use a custom tag, update `[guest_images]` in your `runner.toml` to reference it.

### 4. Docker socket (macOS)

When using the container isolation tier, the runner connects to Docker via the default socket path. On macOS, if you use a Docker runtime other than Docker Desktop (e.g. Colima, Rancher Desktop, or Lima), you must set `DOCKER_HOST` to point at the correct socket:

```bash
# Colima example
export DOCKER_HOST=unix://$HOME/.colima/default/docker.sock
```

### 5. Run

> **Note:** MicroVM support is currently experimental; use the container tier unless you are specifically testing MicroVM behavior.

**Process tier** (no Docker required, isolation is degraded):

```bash
cargo run -p runner -- --config .oxydra/runner.toml --user alice --insecure start
```

**Container tier** (requires guest images from step 3):

```bash
cargo run -p runner -- --config .oxydra/runner.toml --user alice start
```

Both commands launch the guest sandbox and enter daemon mode, listening on a Unix control socket for lifecycle commands.

Check the status of a running session:

```bash
cargo run -p runner -- --config .oxydra/runner.toml --user alice status
```

Stop a running session:

```bash
cargo run -p runner -- --config .oxydra/runner.toml --user alice stop
```

Restart (stop + start) in one command:

```bash
cargo run -p runner -- --config .oxydra/runner.toml --user alice restart
```

Connect the TUI to a running session:

```bash
cargo run -p runner -- --tui --config .oxydra/runner.toml --user alice
```

Join a specific existing session by ID:

```bash
cargo run -p runner -- --tui --config .oxydra/runner.toml --user alice --session <session-id>
```

If you see `oxydra-tui was not found in PATH`, build or install it:

```bash
cargo install --path crates/tui
# or
cargo build -p tui
target/debug/oxydra-tui --gateway-endpoint ws://127.0.0.1:PORT/ws --user alice
```

See `cargo run -p runner -- --help` for all CLI options.

### Session management (TUI)

Once connected, you can manage multiple sessions from within the TUI:

| Command | Description |
|---------|-------------|
| `/new` | Create a new session (starts fresh conversation) |
| `/new <name>` | Create a new named session |
| `/sessions` | List all your sessions with IDs and last-active times |
| `/switch <id>` | Switch the TUI to an existing session by ID (prefix match) |

Each TUI window manages one session at a time. You can open multiple TUI windows connected to the same gateway to work in multiple sessions simultaneously.
When the per-user concurrent turn limit is reached, new top-level turns queue fairly (FIFO) instead of being dropped, up to the configured per-user queue bound.

## Local build and run guide

This repo includes `scripts/build-release-assets.sh` to build release tarballs and guest Docker images in one command.

### Platform dependencies

| Platform | Build dependencies |
|---|---|
| macOS (Apple Silicon, arm64) | Rust stable (`rustup`), `cargo-zigbuild`, `zig`, Docker |
| Linux PC (amd64) | Rust stable (`rustup`), `cargo-zigbuild`, `zig`, Docker |
| Linux Raspberry Pi (aarch64/arm64) | Rust stable (`rustup`), `cargo-zigbuild`, `zig`, Docker |

Install shared prerequisites:

```bash
rustup toolchain install stable
rustup target add wasm32-wasip1
cargo install cargo-zigbuild
```

Install zig:

```bash
# macOS
brew install zig

# Debian/Ubuntu (example)
sudo apt-get update && sudo apt-get install -y zig
```

### Build locally

Build all requested targets and local Docker images:

```bash
# macOS arm64 host: builds macos-arm64 + linux-amd64 + linux-arm64
./scripts/build-release-assets.sh --tag local

# Linux amd64 host: build linux amd64/arm64 only
./scripts/build-release-assets.sh --platforms linux-amd64,linux-arm64 --tag local

# Push Docker images to GHCR (default registry; after `docker login ghcr.io`)
./scripts/build-release-assets.sh --platforms linux-amd64,linux-arm64 --tag v0.2.0 \
  --push-docker --image-namespace <github-user-or-org>

# Push Docker images to Docker Hub instead (optional override)
./scripts/build-release-assets.sh --platforms linux-amd64,linux-arm64 --tag v0.2.0 \
  --push-docker --registry dockerhub --image-namespace <dockerhub-namespace>
```

For maintainers publishing tag releases (not needed for end users), set these GitHub repository settings:

- `IMAGE_REGISTRY` (Actions variable, optional; defaults to `ghcr`)
- `IMAGE_NAMESPACE` (Actions variable, optional; defaults to `github.repository_owner`)
- `DOCKERHUB_USERNAME` + `DOCKERHUB_TOKEN` (Actions secrets, only needed when `IMAGE_REGISTRY=dockerhub`)

Artifacts are created in `dist/`:

- `oxydra-<tag>-macos-arm64.tar.gz`
- `oxydra-<tag>-linux-amd64.tar.gz`
- `oxydra-<tag>-linux-arm64.tar.gz`
- `SHA256SUMS`

### Run from local build artifacts

```bash
mkdir -p ./dist/run
tar -xzf ./dist/oxydra-local-macos-arm64.tar.gz -C ./dist/run   # or linux-amd64/linux-arm64 tarball
./dist/run/runner --config .oxydra/runner.toml --user alice --insecure start
./dist/run/oxydra-tui --gateway-endpoint ws://127.0.0.1:PORT/ws --user alice
```

### Run with local Docker images

Local image tags are created as:

- `oxydra-vm:<tag>-linux-amd64`
- `oxydra-vm:<tag>-linux-arm64`
- `shell-vm:<tag>-linux-amd64`
- `shell-vm:<tag>-linux-arm64`

Use the right tag in `.oxydra/runner.toml`:

```toml
[guest_images]
oxydra_vm = "oxydra-vm:local-linux-arm64" # use linux-amd64 on x86_64 PCs
shell_vm  = "shell-vm:local-linux-arm64"  # use linux-amd64 on x86_64 PCs
```

Then start normally:

```bash
cargo run -p runner -- --config .oxydra/runner.toml --user alice start
```

## Install from published releases and GHCR

### 1) Install binaries from GitHub Releases

1. Open the latest release at `https://github.com/<owner>/<repo>/releases` (replace with your repository path)
2. Download the tarball for your platform:
   - `oxydra-<tag>-macos-arm64.tar.gz` (macOS Apple Silicon)
   - `oxydra-<tag>-linux-amd64.tar.gz` (Linux PC)
   - `oxydra-<tag>-linux-arm64.tar.gz` (Raspberry Pi 64-bit / ARM64)
3. Download `SHA256SUMS` and verify:

```bash
shasum -a 256 -c SHA256SUMS
```

4. Extract and place binaries on `PATH`:

```bash
tar -xzf oxydra-<tag>-linux-amd64.tar.gz
sudo install -m 0755 runner /usr/local/bin/runner
sudo install -m 0755 oxydra-vm /usr/local/bin/oxydra-vm
sudo install -m 0755 shell-daemon /usr/local/bin/shell-daemon
sudo install -m 0755 oxydra-tui /usr/local/bin/oxydra-tui
```

### 2) Copy minimum config and set the published image refs

```bash
mkdir -p .oxydra/users
cp examples/config/agent.toml .oxydra/agent.toml
cp examples/config/runner.toml .oxydra/runner.toml
cp examples/config/runner-user.toml .oxydra/users/alice.toml
```

Set bare-minimum values in `.oxydra/runner.toml`:

```toml
default_tier = "container"   # recommended; microvm is experimental

[guest_images]
oxydra_vm = "ghcr.io/<owner-or-org>/oxydra-vm:<tag>"
shell_vm  = "ghcr.io/<owner-or-org>/shell-vm:<tag>"

[users.alice] # replace "alice" if you want a different user ID
config_path = "users/alice.toml"
```

Set bare-minimum provider selection in `.oxydra/agent.toml`:

```toml
[selection]
provider = "openrouter"
model = "z-ai/glm-4.5-air:free"

[providers.registry.openrouter]
catalog_provider="openrouter"
provider_type = "openai"
base_url = "https://openrouter.ai/api"
api_key_env = "OPENROUTER_API_KEY"
```

Set your provider API key before start:

```bash
export OPENROUTER_API_KEY=your-key
# or OPENAPI_API_KEY / ANTHROPIC_API_KEY / GEMINI_API_KEY
```

Read the inline comments in the copied config files to review optional overrides (memory, runtime limits, mounts, channels, subagents etc.).

### 3) (Optional) pre-pull published Docker images from GHCR

The release workflow publishes multi-arch manifests for `linux/amd64` and `linux/arm64`:

```bash
docker pull ghcr.io/<owner-or-org>/oxydra-vm:<tag>
docker pull ghcr.io/<owner-or-org>/shell-vm:<tag>
```

Pre-pulling is optional: the runner automatically pulls missing images on startup.

## Configuration

### Config file locations (lowest to highest precedence)

1. Built-in defaults
2. `/etc/oxydra/agent.toml`
3. `~/.config/oxydra/agent.toml`
4. `./.oxydra/agent.toml`
5. Environment variables (`OXYDRA__SELECTION__PROVIDER=anthropic`)
6. CLI overrides

A providers-only file (`providers.toml`) is also loaded at each level if present.

### Environment variables

Use `OXYDRA__` prefix with `__` as path separator:

```bash
OXYDRA__SELECTION__PROVIDER=anthropic
OXYDRA__SELECTION__MODEL=claude-3-5-sonnet-latest
OXYDRA__RUNTIME__MAX_TURNS=12
OXYDRA__MEMORY__ENABLED=true
```

API key resolution order: explicit config value > custom `api_key_env` > provider-specific env var (`OPENAI_API_KEY` / `ANTHROPIC_API_KEY` / `GEMINI_API_KEY`) > fallback `API_KEY`.

### Memory configuration

Enable persistent memory in your `agent.toml`:

```toml
[memory]
enabled = true
# embedding_backend = "model2vec"   # default; or "deterministic" for hash-based
# model2vec_model = "potion_32m"    # default; or "potion_8m" for lighter footprint

[memory.retrieval]
top_k = 8
vector_weight = 0.7
fts_weight = 0.3
```

**Embedding backends:**

| Backend | Config Value | Description |
|---------|-------------|-------------|
| Model2vec | `model2vec` (default) | Semantic embeddings via Potion static models. Paraphrases and related concepts cluster naturally. |
| Deterministic | `deterministic` | Blake3 hash-based vectors. No semantic similarity, but zero dependencies and instant startup. |

**Model2vec model sizes:**

| Model | Config Value | Size | Use Case |
|-------|-------------|------|----------|
| Potion-32M | `potion_32m` (default) | ~32 MB | Higher quality semantic retrieval |
| Potion-8M | `potion_8m` | ~8 MB | Constrained environments |

When memory is enabled, the agent gains access to:
- **Memory tools** — `memory_save`, `memory_search` (with tag filtering), `memory_update`, `memory_delete` for cross-session knowledge
- **Scratchpad tools** — `scratchpad_write`, `scratchpad_read`, `scratchpad_clear` for session-scoped working memory during long tasks
- **Autonomy protocol** — The agent follows a structured plan→recall→execute→reflect→learn loop and uses a retry protocol (up to 3 distinct approaches before escalating to the user)

### Runner configuration

The runner requires two config files:

- **`runner.toml`** — Global settings: workspace root, default isolation tier, guest images, user mappings
- **`users/<id>.toml`** — Per-user overrides: resource limits, sandbox tier, mount paths, credential references, and external channel configuration

See `examples/config/` for complete annotated configuration files:

- [`agent.toml`](examples/config/agent.toml) — Agent runtime, provider, memory, reliability, and gateway session/turn limits
- [`runner.toml`](examples/config/runner.toml) — Runner global settings (workspace root, sandbox tier, guest images, users)
- [`runner-user.toml`](examples/config/runner-user.toml) — Per-user overrides (mounts, resources, credentials, behavior, external channels)

### External channels (Telegram)

Oxydra can receive messages via Telegram and respond through a bot. The adapter runs inside the VM alongside the gateway — no separate process or webhook server required.

**Setup:**

1. Create a Telegram bot via [@BotFather](https://t.me/BotFather) and copy the bot token
2. Set the token as an environment variable before starting the runner:
   ```bash
   export ALICE_TELEGRAM_BOT_TOKEN=your-bot-token-here
   ```
3. Find your Telegram user ID (e.g. using [@userinfobot](https://t.me/userinfobot))
4. Add the channel config to your per-user config file (e.g. `.oxydra/users/alice.toml`):

```toml
[channels.telegram]
enabled = true
bot_token_env = "ALICE_TELEGRAM_BOT_TOKEN"

[[channels.telegram.senders]]
platform_ids = ["12345678"]   # Your Telegram user ID
```

Only senders explicitly listed in `[[channels.telegram.senders]]` can interact with the agent. Unknown senders are silently dropped and audit-logged to `<workspace>/.oxydra/sender_audit.log`.

**Forum topics as separate sessions:**

In Telegram groups with forum mode enabled, each topic maps to an independent session — so you can have a "Research" topic and a "Coding" topic running simultaneously without blocking each other.

**Session commands via Telegram:**

The same `/new`, `/sessions`, `/switch`, `/cancel`, `/cancelall`, and `/status` commands work in Telegram as in the TUI.

See [`examples/config/runner-user.toml`](examples/config/runner-user.toml) for the full annotated channel configuration reference.

## Workspace layout

```
crates/
  types/          Core type definitions, config, model catalog
  provider/       LLM provider implementations (OpenAI, Anthropic, Gemini, OpenAI Responses)
  tools/          Tool trait and core tool implementations
  tools-macros/   #[tool] procedural macro
  runtime/        Agent turn-loop runtime
  memory/         Persistent memory with hybrid retrieval
  sandbox/        WASM sandbox and security policies
  runner/         Runner lifecycle, guest orchestration, catalog commands
  shell-daemon/   Shell daemon protocol for guest environments
  channels/       Channel adapters (Telegram) and auth pipeline
  gateway/        WebSocket gateway server (multi-session, protocol v2)
  tui/            Terminal UI adapter
docs/guidebook/   Architecture and implementation documentation
examples/config/  Example configuration files
```

## Documentation

See [`docs/guidebook/`](docs/guidebook/README.md) for detailed architecture and implementation documentation.

## License

See [LICENSE](LICENSE) for details.
