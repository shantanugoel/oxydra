# Oxydra

A high-performance AI agent orchestrator written in Rust. Oxydra provides a modular runtime for building, isolating, and orchestrating AI agents with provider-agnostic LLM integration, tool execution, persistent memory, and multi-agent coordination.

## Features

- **Provider-agnostic LLM integration** — OpenAI, Anthropic, Google Gemini, and OpenAI Responses API with SSE streaming, automatic retries, and a pinned model catalog
- **Tool system** — `#[tool]` proc-macro for defining tools with automatic JSON Schema generation and safety tiers
- **Agent runtime** — Turn-loop state machine with tool dispatch, self-correction, context budget management, and cost limits
- **Persistent memory** — Hybrid retrieval (vector + FTS5) over libSQL with conversation summarization
- **Isolation tiers** — MicroVM, container, and process-level sandboxing via the runner/guest model
- **Gateway and channels** — WebSocket-based gateway with pluggable channel adapters (TUI included)
- **Configuration** — Layered config (files, env vars, CLI overrides) with deterministic precedence and validation

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
cargo run -p runner -- --config .oxydra/runner.toml --user alice --insecure --daemon
```

The process-tier runner exits immediately unless you pass `--daemon`. Keep it running in one
terminal, then connect the TUI in another.

**Container tier** (requires guest images from step 3):

```bash
cargo run -p runner -- --config .oxydra/runner.toml --user alice
```

The runner spawns two containers per user: one for the agent runtime (`oxydra-vm`) and one for the shell/browser sidecar (`shell-vm`). Both use host networking and bind-mount the user workspace.

Connect the TUI to a running session:

```bash
cargo run -p runner -- --tui --config .oxydra/runner.toml --user alice
```

If you see `oxydra-tui was not found in PATH`, build or install it:

```bash
cargo install --path crates/tui
# or
cargo build -p tui
target/debug/oxydra-tui --gateway-endpoint ws://127.0.0.1:PORT/ws --user alice
```

See `cargo run -p runner -- --help` for all CLI options.

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
- **`users/<id>.toml`** — Per-user overrides: resource limits, sandbox tier, mount paths, credential references

See `examples/config/` for complete annotated configuration files:

- [`agent.toml`](examples/config/agent.toml) — Agent runtime, provider, memory, and reliability settings
- [`runner.toml`](examples/config/runner.toml) — Runner global settings (workspace root, sandbox tier, guest images, users)
- [`runner-user.toml`](examples/config/runner-user.toml) — Per-user overrides (mounts, resources, credentials, behavior)

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
  channels/       Channel trait and registry
  gateway/        WebSocket gateway server
  tui/            Terminal UI adapter
docs/guidebook/   Architecture and implementation documentation
examples/config/  Example configuration files
```

## Documentation

See [`docs/guidebook/`](docs/guidebook/README.md) for detailed architecture and implementation documentation.

## License

See [LICENSE](LICENSE) for details.
