![Oxydra](./static/oxydra-1.png)

# Oxydra

Oxydra is a Rust-based AI agent orchestrator for running capable agents with strong isolation, provider flexibility, tool execution, persistent memory, and multi-session concurrency.

> ⚠️ **Maturity notice:** Oxydra is still pre-pre-alpha and not yet ready for broad production use.

## Part 1 — Product Intro & Features

Oxydra is designed for people who want an agent runtime they can self-host, inspect, and evolve — not a black box.

### Why it is useful

- **Provider-agnostic LLM layer**: OpenAI, Anthropic, Gemini, and OpenAI Responses API, with streaming, retries, and model-catalog-based validation.
- **Multi-agent orchestration**: Define specialist subagents with distinct prompts/tools/models and delegate tasks between them.
- **Async by default**: Multiple sessions/chats can run in parallel without blocking each other.
- **Tooling with safety rails**: `#[tool]` macro, auto schema generation, safety tiers, and workspace path hardening (`/shared`, `/tmp`, `/vault`).
- **Structured runtime loop**: Planning, tool dispatch, retries, context budget management, and bounded turn/cost controls.
- **Persistent memory**: libSQL-backed hybrid retrieval (vector + FTS), summarization, memory CRUD tools, and session scratchpad.
- **Durable scheduler**: One-off and periodic jobs with run history and notification routing.
- **Isolation model**: Process, container, and microVM tiers with WASM sandboxing as defense-in-depth.
- **Gateway + channels**: WebSocket gateway with per-user multi-session support, plus Telegram channel adapter.
- **Deterministic configuration**: Layered config (defaults/files/env/CLI) with explicit validation.

### Isolation tiers at a glance

| Tier | Docker Required | Safety | Typical Use |
|---|---:|---|---|
| `container` (recommended) | ✅ | Strong | Daily usage with shell/browser tools |
| `process` (`--insecure`) | ❌ | Degraded | Fast local testing when Docker is unavailable |
| `micro_vm` (experimental) | ✅ | Strongest | Advanced isolation testing (extra host setup required) |

`process` mode disables shell/browser tools and runs directly on the host.

`micro_vm` prerequisites:

- macOS: requires Docker Desktop running (used for the sandbox VM runtime)
- Linux: requires the `firecracker` binary plus Firecracker config paths in `runner.toml`

---

## Part 2 — Quick Start (Install From Releases)

This is the **recommended path** if you just want Oxydra running quickly.

### 0) Choose a release tag

Pick a version from the [GitHub releases page](https://github.com/shantanugoel/oxydra/releases) and export it once:

```bash
export OXYDRA_TAG=v0.3.0   # replace with the release you want
```

### 1) Install binaries and bootstrap config templates

#### Option A (recommended): one-command installer

```bash
curl -fsSL https://raw.githubusercontent.com/shantanugoel/oxydra/main/scripts/install-release.sh | bash -s -- --tag "$OXYDRA_TAG" --base-dir "$PWD"
```

Defaults:

- Installs to `~/.local/bin`
- Installs `runner`, `oxydra-vm`, `shell-daemon`, `oxydra-tui`
- Copies example configs to `<base-dir>/.oxydra/agent.toml`, `<base-dir>/.oxydra/runner.toml`, and `<base-dir>/.oxydra/users/alice.toml`
- Leaves existing config files unchanged (use `--overwrite-config` to replace)

Useful variants:

```bash
# Install latest release (no pin)
curl -fsSL https://raw.githubusercontent.com/shantanugoel/oxydra/main/scripts/install-release.sh | bash

# Install to /usr/local/bin (uses sudo)
curl -fsSL https://raw.githubusercontent.com/shantanugoel/oxydra/main/scripts/install-release.sh | bash -s -- --tag "$OXYDRA_TAG" --system

# Install into a different project directory
curl -fsSL https://raw.githubusercontent.com/shantanugoel/oxydra/main/scripts/install-release.sh | bash -s -- --tag "$OXYDRA_TAG" --base-dir /path/to/project

# Install binaries only (skip config scaffolding)
curl -fsSL https://raw.githubusercontent.com/shantanugoel/oxydra/main/scripts/install-release.sh | bash -s -- --tag "$OXYDRA_TAG" --skip-config
```

#### Option B: manual install

```bash
# Download the correct artifact for your platform:
#   oxydra-<tag>-macos-arm64.tar.gz
#   oxydra-<tag>-linux-amd64.tar.gz
#   oxydra-<tag>-linux-arm64.tar.gz

PLATFORM=linux-amd64   # change for your platform

curl -fL -o "oxydra-${OXYDRA_TAG}-${PLATFORM}.tar.gz" \
  "https://github.com/shantanugoel/oxydra/releases/download/${OXYDRA_TAG}/oxydra-${OXYDRA_TAG}-${PLATFORM}.tar.gz"

tar -xzf "oxydra-${OXYDRA_TAG}-${PLATFORM}.tar.gz"

mkdir -p ~/.local/bin
install -m 0755 runner ~/.local/bin/runner
install -m 0755 oxydra-vm ~/.local/bin/oxydra-vm
install -m 0755 shell-daemon ~/.local/bin/shell-daemon
install -m 0755 oxydra-tui ~/.local/bin/oxydra-tui
```

Option B installs binaries only. If you want automatic config scaffolding, use Option A.

If `~/.local/bin` is not in `PATH`:

```bash
export PATH="$HOME/.local/bin:$PATH"
```

### 2) Review and update the copied config templates

If you used Option A above, the installer already created:

- `.oxydra/agent.toml`
- `.oxydra/runner.toml`
- `.oxydra/users/alice.toml`

Update `.oxydra/runner.toml` to match the release you installed:

```toml
default_tier = "container"

[guest_images]
oxydra_vm = "ghcr.io/shantanugoel/oxydra-vm:${OXYDRA_TAG}"
shell_vm  = "ghcr.io/shantanugoel/shell-vm:${OXYDRA_TAG}"
```

If you plan to use `micro_vm` instead of `container`:

- macOS: install and run Docker Desktop
- Linux: install `firecracker`, set `default_tier = "micro_vm"`, and configure `guest_images.firecracker_oxydra_vm_config` (plus `guest_images.firecracker_shell_vm_config` if you want shell/browser sidecar)

Update `.oxydra/agent.toml`:

- choose `[selection].provider` and `[selection].model`
- keep the matching `[providers.registry.<name>]` entry with the right `api_key_env`
- OpenAI example: `api_key_env = "OPENAI_API_KEY"`
- Anthropic example: `api_key_env = "ANTHROPIC_API_KEY"`
- Gemini example: `api_key_env = "GEMINI_API_KEY"`

If you want a user id other than `alice`, update `[users.alice]` in `.oxydra/runner.toml` and rename `.oxydra/users/alice.toml` accordingly.

### 3) Set your provider API key

```bash
export OPENAI_API_KEY=your-key-here
# or: export ANTHROPIC_API_KEY=...
# or: export GEMINI_API_KEY=...
```

### 4) Start and connect

Run the daemon in terminal 1:

```bash
runner --config .oxydra/runner.toml --user alice start
```

Connect TUI in terminal 2:

```bash
runner --tui --config .oxydra/runner.toml --user alice
```

Lifecycle commands:

```bash
runner --config .oxydra/runner.toml --user alice status
runner --config .oxydra/runner.toml --user alice stop
runner --config .oxydra/runner.toml --user alice restart
```

### 5) If Docker is unavailable

Use process mode (lower safety, no shell/browser tools):

```bash
runner --config .oxydra/runner.toml --user alice --insecure start
runner --tui --config .oxydra/runner.toml --user alice
```

### 6) TUI session commands

| Command | Meaning |
|---|---|
| `/new` | Create a fresh session |
| `/new <name>` | Create a named session |
| `/sessions` | List sessions |
| `/switch <id>` | Switch by session id (prefix supported) |
| `/cancel` | Cancel active turn in current session |
| `/cancelall` | Cancel active turns across sessions |
| `/status` | Show current session/runtime state |

### 7) (Optional) Enable Telegram

1. Create a bot with [@BotFather](https://t.me/BotFather) and copy the bot token.
2. Find your Telegram user ID (for example with [@userinfobot](https://t.me/userinfobot)).
3. Export the bot token before starting the runner:

```bash
export ALICE_TELEGRAM_BOT_TOKEN=your-bot-token
```

4. Edit `.oxydra/users/alice.toml` and add/uncomment:

```toml
[channels.telegram]
enabled = true
bot_token_env = "ALICE_TELEGRAM_BOT_TOKEN"

[[channels.telegram.senders]]
platform_ids = ["12345678"]
```

5. Restart the runner and message your bot in Telegram.

Only IDs listed in `[[channels.telegram.senders]]` are allowed to interact with your agent.
Telegram supports the same session commands (`/new`, `/sessions`, `/switch`, `/cancel`, `/cancelall`, `/status`).

### Quick troubleshooting

| Symptom | Fix |
|---|---|
| `oxydra-tui was not found in PATH` | Ensure install dir is in `PATH` or run the binary directly |
| Docker unreachable | Start Docker; for Colima set `DOCKER_HOST=unix://$HOME/.colima/default/docker.sock` |
| Telegram bot does not respond | Verify `bot_token_env` points to an exported token and your Telegram user ID is listed in `[[channels.telegram.senders]]` |
| `micro_vm` start fails on macOS | Ensure Docker Desktop is installed and running |
| `micro_vm` start fails on Linux (`firecracker` or config error) | Install `firecracker` and set `guest_images.firecracker_oxydra_vm_config` (and `guest_images.firecracker_shell_vm_config` for sidecar) |
| `unknown model for catalog provider` | Use `runner catalog show` to inspect known models, or set `catalog.skip_catalog_validation = true` |
| 401/Unauthorized from provider | Check API key env var and `api_key_env` name in `agent.toml` |

---

## Part 3 — Use Latest From Repository (and Contribute)

Use this path if you want unreleased changes, local development, or contributions.

### 1) Prerequisites

- Rust stable (`rustup toolchain install stable`)
- WASM target (`rustup target add wasm32-wasip1`)
- Docker (required for `container` and `micro_vm` tiers)
- `micro_vm` on macOS: Docker Desktop
- `micro_vm` on Linux: `firecracker` binary + Firecracker VM config files referenced in `.oxydra/runner.toml`
- Provider API key (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, or `GEMINI_API_KEY`)

If you want to build guest images locally via cross-compilation:

- `cargo-zigbuild` (`cargo install cargo-zigbuild`)
- `zig` (`brew install zig` on macOS)

### 2) Clone and bootstrap config

```bash
git clone https://github.com/shantanugoel/oxydra.git
cd oxydra

mkdir -p .oxydra/users
cp examples/config/agent.toml .oxydra/agent.toml
cp examples/config/runner.toml .oxydra/runner.toml
cp examples/config/runner-user.toml .oxydra/users/alice.toml
```

Edit `.oxydra/agent.toml` and `.oxydra/runner.toml` for your provider, tier, and image refs.
For most setups, set `default_tier = "container"` in `.oxydra/runner.toml`.

### 3) Build workspace

```bash
cargo build --workspace
```

### 4) Build guest images (for container/micro_vm)

Option A: cross-compile locally (supports both `amd64` and `arm64`):

```bash
./scripts/build-guest-images.sh arm64
# or
./scripts/build-guest-images.sh amd64
```

Option B: in-Docker build (currently builds `linux/arm64` images):

```bash
./scripts/build-guest-images-in-docker.sh
```

If you used a custom tag, set matching refs in `.oxydra/runner.toml`:

```toml
[guest_images]
oxydra_vm = "oxydra-vm:<tag>"
shell_vm  = "shell-vm:<tag>"
```

### 5) Run from source

```bash
cargo run -p runner -- --config .oxydra/runner.toml --user alice start

# One-time setup for a fresh clone (skip if oxydra-tui is already in PATH)
cargo install --path crates/tui

cargo run -p runner -- --tui --config .oxydra/runner.toml --user alice
```

Process-tier fallback:

```bash
cargo run -p runner -- --config .oxydra/runner.toml --user alice --insecure start
```

### 6) Quality checks before opening a PR

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

### 7) Build release artifacts (maintainers)

```bash
# macOS arm64 host: builds macOS + linux-amd64 + linux-arm64 artifacts
./scripts/build-release-assets.sh --tag local

# Linux host example
./scripts/build-release-assets.sh --platforms linux-amd64,linux-arm64 --tag local
```

### Documentation and code map

- Architecture guidebook: [`docs/guidebook/README.md`](docs/guidebook/README.md)
- Example configs: [`examples/config/`](examples/config)
- Workspace layout:

```text
crates/
  types/          Core type definitions, config, model catalog
  provider/       LLM provider implementations
  tools/          Tool trait and core tools
  tools-macros/   #[tool] procedural macro
  runtime/        Agent turn-loop runtime
  memory/         Persistent memory and retrieval
  sandbox/        WASM sandboxing and policies
  runner/         Runner lifecycle and guest orchestration
  shell-daemon/   Shell daemon protocol
  channels/       External channel adapters (Telegram)
  gateway/        WebSocket gateway server
  tui/            Terminal UI client
```

License: see [LICENSE](LICENSE).
