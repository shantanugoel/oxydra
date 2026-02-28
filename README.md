![Oxydra](./static/oxydra-1.png)

# Oxydra

A high-performance AI agent orchestrator written in Rust. Oxydra provides a modular runtime for building, isolating, and orchestrating AI agents with provider-agnostic LLM integration, tool execution, persistent memory, and multi-agent coordination.

## Features

- **Provider-agnostic LLM integration** — OpenAI, Anthropic, Google Gemini, and OpenAI Responses API with SSE streaming, automatic retries, and a dynamic model catalog (models.dev-aligned with Oxydra capability overlays)
- **Multi Agent Orchestration** - Supports creating custom subagents with their own purpose and model selections that the orchestrator can delegate special tasks to
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

## ⚠️ Maturity Notice

Not ready for wide use, yet! Unless you love dipping your toes in pre-pre-alpha software.

---

## Which Run Mode Should I Use?

| Mode | Docker Required? | Isolation Level | Best For |
|------|:-:|---|---|
| **Container** (recommended) | ✅ | Full — agent runs in a Docker container | Normal use, production-like safety |
| **Process** (`--insecure`) | ❌ | Degraded — agent runs on host directly | Quick testing, no Docker available. Shell tool is not available in this mode |
| **MicroVM** (experimental) | ✅ | Strongest — Firecracker VM | Advanced users testing VM isolation. This mode needs additional installations like docker-desktop (macOS) or firecracker (linux) and is not tested well yet |

> **Recommendation:** Use the **container** tier unless you have a specific reason not to. Process mode disables shell/browser tools and runs without sandboxing.

---

## Quick Start

These instructions are for using this repository directly. For installing/using published releases, scroll to the bottom.

### Prerequisites

Before you begin, make sure you have:

- **Rust stable** (`rustup toolchain install stable`)
- **WASM target** (`rustup target add wasm32-wasip1`)
- **Docker** (if using container or MicroVM tier)
- An API key for at least one LLM provider (OpenAI, Anthropic, or Gemini)

**Preflight check:**

```bash
# Verify Rust toolchain
rustc --version

# Verify Docker is reachable (skip if using process tier)
docker info > /dev/null 2>&1 && echo "Docker OK" || echo "Docker NOT reachable"

# Verify API key is set (pick your provider)
[ -n "$OPENAI_API_KEY" ] && echo "OpenAI key set" || echo "OPENAI_API_KEY not set"
```

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

### 2. Configure provider and runner

Set the provider + model in `.oxydra/agent.toml`:

```toml
[selection]
provider = "openai"
model = "gpt-4o-mini"

[providers.registry.openai]
provider_type = "openai"
api_key_env = "OPENAI_API_KEY"
```

Set the runner tier and guest images in `.oxydra/runner.toml`:

```toml
default_tier = "container"

[guest_images]
oxydra_vm = "ghcr.io/shantanugoel/shell-vm:v0.1.0"
shell_vm  = "ghcr.io/shantanugoel/oxydra-vm:v0.1.0"

[users.alice]
config_path = "users/alice.toml"
```

> **Tip:** Read the inline comments in `examples/config/agent.toml`, `examples/config/runner.toml`, and `examples/config/runner-user.toml` for all available configuration knobs.

### 3. Build

```bash
cargo build --workspace
```

### 4. Build guest Docker images

The container and MicroVM tiers run agent code inside Docker containers. You must build the guest images before using these tiers. **Skip this step if using process tier.**

#### Option A — Local cross-compilation (recommended, faster)

Requires [`cargo-zigbuild`](https://github.com/rust-cross/cargo-zigbuild) and [`zig`](https://ziglang.org/download/):

```bash
# Install prerequisites (once)
cargo install cargo-zigbuild
brew install zig            # macOS — see ziglang.org for other platforms
```

Then build for your architecture:

```bash
./scripts/build-guest-images.sh arm64          # Apple Silicon / ARM servers
./scripts/build-guest-images.sh amd64          # Intel / AMD servers
```

#### Option B — Full in-Docker build (slower, no local toolchain needed)

```bash
./scripts/build-guest-images-in-docker.sh          # default tag: latest
```

Both options produce two images: `oxydra-vm:latest` and `shell-vm:latest`.

### 5. Docker socket (macOS)

If you use a Docker runtime other than Docker Desktop (e.g. Colima, Rancher Desktop, Lima), set `DOCKER_HOST`:

```bash
# Colima example
export DOCKER_HOST=unix://$HOME/.colima/default/docker.sock
```

### 6. Run

**Container tier** (recommended — requires guest images from step 4):

```bash
# Start the agent
cargo run -p runner -- --config .oxydra/runner.toml --user alice start

# Connect the TUI
cargo run -p runner -- --tui --config .oxydra/runner.toml --user alice
```

**Process tier** (no Docker required, degraded isolation):

```bash
# Start the agent
cargo run -p runner -- --config .oxydra/runner.toml --user alice --insecure start

# Connect the TUI
cargo run -p runner -- --tui --config .oxydra/runner.toml --user alice
```

**Other runner commands:**

```bash
# Check status
cargo run -p runner -- --config .oxydra/runner.toml --user alice status

# Stop
cargo run -p runner -- --config .oxydra/runner.toml --user alice stop

# Restart (stop + start)
cargo run -p runner -- --config .oxydra/runner.toml --user alice restart
```

**Join a specific session:**

```bash
cargo run -p runner -- --tui --config .oxydra/runner.toml --user alice --session <session-id>
```

See `cargo run -p runner -- --help` for all CLI options.

### Session Management (TUI)

Once connected, manage multiple sessions from within the TUI:

| Command | Description |
|---------|-------------|
| `/new` | Create a new session (starts fresh conversation) |
| `/new <name>` | Create a new named session |
| `/sessions` | List all your sessions with IDs and last-active times |
| `/switch <id>` | Switch to an existing session by ID (prefix match supported) |
| `/cancel` | Cancel the active turn in the current session |
| `/cancelall` | Cancel active turns across all your sessions |

Each TUI window manages one session at a time. You can open multiple TUI windows connected to the same gateway to work in multiple sessions simultaneously.

---

## Troubleshooting

| Symptom | Probable Cause | Fix |
|---------|---------------|-----|
| `oxydra-tui was not found in PATH` | TUI binary not built/installed | `cargo install --path crates/tui` or `cargo build -p tui` and use `target/debug/oxydra-tui` directly |
| `Docker NOT reachable` / connection refused | Docker daemon not running, or wrong socket | Start Docker; on macOS with Colima: `export DOCKER_HOST=unix://$HOME/.colima/default/docker.sock` |
| `unknown model for catalog provider` | Model ID not in catalog | Check `cargo run -p runner -- catalog show` for available models, or set `catalog.skip_catalog_validation = true` in `agent.toml` |
| `Unauthorized` / 401 from provider | API key not set or invalid | Verify your key: `echo $OPENAI_API_KEY` (should not be empty); check `api_key_env` in config matches your env var |
| Guest image not found | Docker images not built | Run step 4 (build guest images) |
| Turn rejected: "too many queued turns" | Per-user turn queue full | Wait for active turns to complete, or increase `gateway.max_sessions_per_user` in config |
| `wasm32-wasip1` build error | Missing WASM target | `rustup target add wasm32-wasip1` |

---

## Configuration

### Config File Locations (lowest to highest precedence)

1. Built-in defaults
2. `/etc/oxydra/agent.toml`
3. `~/.config/oxydra/agent.toml`
4. `./.oxydra/agent.toml`
5. Environment variables (`OXYDRA__SELECTION__PROVIDER=anthropic`)
6. CLI overrides

A providers-only file (`providers.toml`) is also loaded at each level if present.

### Environment Variables

Use `OXYDRA__` prefix with `__` as path separator:

```bash
OXYDRA__SELECTION__PROVIDER=anthropic
OXYDRA__SELECTION__MODEL=claude-3-5-sonnet-latest
OXYDRA__RUNTIME__MAX_TURNS=12
OXYDRA__MEMORY__ENABLED=true
```

API key resolution order: explicit config value > custom `api_key_env` > provider-specific env var (`OPENAI_API_KEY` / `ANTHROPIC_API_KEY` / `GEMINI_API_KEY`) > fallback `API_KEY`.

### Memory Configuration

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

### Runner Configuration

The runner requires two config files:

- **`runner.toml`** — Global settings: workspace root, default isolation tier, guest images, user mappings
- **`users/<id>.toml`** — Per-user overrides: resource limits, sandbox tier, mount paths, credential references, and external channel configuration

See `examples/config/` for complete annotated configuration files:

- [`agent.toml`](examples/config/agent.toml) — Agent runtime, provider, memory, reliability, and gateway session/turn limits
- [`runner.toml`](examples/config/runner.toml) — Runner global settings (workspace root, sandbox tier, guest images, users)
- [`runner-user.toml`](examples/config/runner-user.toml) — Per-user overrides (mounts, resources, credentials, behavior, external channels)

### External Channels (Telegram)

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

---

## Workspace Layout

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

---

## Building and Installing from Source

This section covers building release artifacts and running from built binaries — useful for deploying to servers or distributing without requiring Rust toolchain on the target machine.

### Platform Dependencies

| Platform | Build Dependencies |
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

# Debian/Ubuntu
sudo apt-get update && sudo apt-get install -y zig
```

### Build Release Artifacts

The repo includes `scripts/build-release-assets.sh` to build release tarballs and guest Docker images in one command:

```bash
# macOS arm64 host: builds macos-arm64 + linux-amd64 + linux-arm64
./scripts/build-release-assets.sh --tag local

# Linux amd64 host: build linux amd64/arm64 only
./scripts/build-release-assets.sh --platforms linux-amd64,linux-arm64 --tag local
```

Artifacts are created in `dist/`:
- `oxydra-<tag>-macos-arm64.tar.gz`
- `oxydra-<tag>-linux-amd64.tar.gz`
- `oxydra-<tag>-linux-arm64.tar.gz`

### Run from Built Artifacts

```bash
mkdir -p ./dist/run
tar -xzf ./dist/oxydra-local-macos-arm64.tar.gz -C ./dist/run   # or linux-amd64/linux-arm64

# Start the agent
./dist/run/runner --config .oxydra/runner.toml --user alice start

# Connect the TUI
./dist/run/oxydra-tui --gateway-endpoint ws://127.0.0.1:PORT/ws --user alice
```

### Run with Local Docker Images

Local image tags are created as `oxydra-vm:<tag>-linux-<arch>` and `shell-vm:<tag>-linux-<arch>`. Update your `runner.toml` to reference them:

```toml
[guest_images]
oxydra_vm = "oxydra-vm:local-linux-arm64"   # use linux-amd64 on x86_64 PCs
shell_vm  = "shell-vm:local-linux-arm64"    # use linux-amd64 on x86_64 PCs
```

Then start normally:

```bash
./dist/run/runner --config .oxydra/runner.toml --user alice start
```

---

## Installing from Published Releases

> **Note:** Replace `shantanugoel` with the appropriate GitHub username/org if this project has been forked.

### 1. Download binaries from GitHub Releases

1. Open the [latest release](https://github.com/shantanugoel/oxydra/releases)
2. Download the tarball for your platform:
   - `oxydra-<tag>-macos-arm64.tar.gz` (macOS Apple Silicon)
   - `oxydra-<tag>-linux-amd64.tar.gz` (Linux PC)
   - `oxydra-<tag>-linux-arm64.tar.gz` (Raspberry Pi 64-bit / ARM64)
3. Extract and place binaries on `PATH`:

```bash
tar -xzf oxydra-<tag>-linux-amd64.tar.gz
sudo install -m 0755 runner /usr/local/bin/runner
sudo install -m 0755 oxydra-vm /usr/local/bin/oxydra-vm
sudo install -m 0755 shell-daemon /usr/local/bin/shell-daemon
sudo install -m 0755 oxydra-tui /usr/local/bin/oxydra-tui
```

### 2. Configure

```bash
mkdir -p .oxydra/users
cp examples/config/agent.toml .oxydra/agent.toml
cp examples/config/runner.toml .oxydra/runner.toml
cp examples/config/runner-user.toml .oxydra/users/alice.toml
```

Set bare-minimum values in `.oxydra/runner.toml`:

```toml
default_tier = "container"

[guest_images]
oxydra_vm = "ghcr.io/shantanugoel/oxydra-vm:<tag>"
shell_vm  = "ghcr.io/shantanugoel/shell-vm:<tag>"

[users.alice]
config_path = "users/alice.toml"
```

> **Note:** Replace `<tag>` above with the release version you downloaded (e.g. `v0.3.0`).

Set the provider in `.oxydra/agent.toml`:

```toml
[selection]
provider = "openai"
model = "gpt-4o-mini"

[providers.registry.openai]
provider_type = "openai"
api_key_env = "OPENAI_API_KEY"
```

Set your API key:

```bash
export OPENAI_API_KEY=your-key
```

Read the inline comments in the copied config files for optional overrides (memory, runtime limits, mounts, channels, subagents, etc.).

### 3. (Optional) Pre-pull Docker images from GHCR

```bash
docker pull ghcr.io/shantanugoel/oxydra-vm:<tag>
docker pull ghcr.io/shantanugoel/shell-vm:<tag>
```

Pre-pulling is optional: the runner automatically pulls missing images on startup.

### 4. Run

```bash
# Start the agent
runner --config .oxydra/runner.toml --user alice start

# Connect the TUI
runner --tui --config .oxydra/runner.toml --user alice
```

---

## Documentation

See [`docs/guidebook/`](docs/guidebook/README.md) for detailed architecture and implementation documentation.

---

<details>
<summary><h2>Maintainer Reference</h2></summary>

### Publishing Docker Images to a Registry

```bash
# Push to GHCR (default registry; after `docker login ghcr.io`)
./scripts/build-release-assets.sh --platforms linux-amd64,linux-arm64 --tag v0.3.0 \
  --push-docker --image-namespace shantanugoel

# Push to Docker Hub instead
./scripts/build-release-assets.sh --platforms linux-amd64,linux-arm64 --tag v0.3.0 \
  --push-docker --registry dockerhub --image-namespace <dockerhub-namespace>
```

### GitHub Actions Settings

For tag-triggered CI releases, configure these GitHub repository settings:

| Setting | Type | Purpose |
|---------|------|---------|
| `IMAGE_REGISTRY` | Actions variable (optional) | Defaults to `ghcr` |
| `IMAGE_NAMESPACE` | Actions variable (optional) | Defaults to `github.repository_owner` |
| `DOCKERHUB_USERNAME` | Actions secret | Only needed when `IMAGE_REGISTRY=dockerhub` |
| `DOCKERHUB_TOKEN` | Actions secret | Only needed when `IMAGE_REGISTRY=dockerhub` |

### Building Guest Images with Custom Tags

```bash
./scripts/build-guest-images.sh arm64 v0.3.0        # local cross-compilation
./scripts/build-guest-images-in-docker.sh v0.3.0     # full in-Docker build
```

To use a custom tag, update `[guest_images]` in your `runner.toml` to reference it.

</details>

---

## License

See [LICENSE](LICENSE) for details.
