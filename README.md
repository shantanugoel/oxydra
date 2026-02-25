# Oxydra

A high-performance AI agent orchestrator written in Rust. Oxydra provides a modular runtime for building, isolating, and orchestrating AI agents with provider-agnostic LLM integration, tool execution, persistent memory, and multi-agent coordination.

## Features

- **Provider-agnostic LLM integration** — OpenAI, Anthropic, Google Gemini, and OpenAI Responses API with SSE streaming, automatic retries, and a pinned model catalog
- **Tool system** — `#[tool]` proc-macro for defining tools with automatic JSON Schema generation and safety tiers
- **Agent runtime** — Turn-loop state machine with tool dispatch, self-correction, context budget management, and cost limits
- **Persistent memory** — Hybrid retrieval (vector + FTS5) over libSQL with conversation summarization and LLM-callable memory tools (search, save, update, delete)
- **Scheduler** — Durable one-off and periodic task scheduling with cron/interval cadences, conditional notification routing, and automatic execution through the same agent runtime policy envelope
- **Isolation tiers** — MicroVM, container, and process-level sandboxing via the runner/guest model
- **Multi-session gateway** — Protocol v2 WebSocket gateway with per-user multi-session support, session persistence, concurrent turn management, and pluggable channel adapters
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

### 2. Build

```bash
cargo build --workspace
```

### 3. Build guest Docker images

The container and MicroVM isolation tiers run `oxydra-vm` and `shell-daemon` inside Docker containers. You must build the guest images before using these tiers.

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

### Runner configuration

The runner requires two config files:

- **`runner.toml`** — Global settings: workspace root, default isolation tier, guest images, user mappings
- **`users/<id>.toml`** — Per-user overrides: resource limits, sandbox tier, mount paths, credential references, and external channel configuration

See `examples/config/` for complete annotated configuration files:

- [`agent.toml`](examples/config/agent.toml) — Agent runtime, provider, memory, and reliability settings
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

The same `/new`, `/sessions`, `/switch`, `/cancel`, and `/status` commands work in Telegram as in the TUI.

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
