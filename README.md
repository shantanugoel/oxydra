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

### WASM Security Across All Tiers

WASM-based tool sandboxing is active in `container`, `micro_vm`, and `process` (`--insecure`) modes.

- File/media tools run with capability-scoped mounts (`/shared`, `/tmp`, `/vault`) and per-tool read/write policies.
- Path traversal is blocked via canonicalization + boundary checks before execution, so paths outside allowed roots are denied.
- Web tools (`web_fetch`, `web_search`) run without filesystem mounts.
- Web tools only accept `http/https` URLs and resolve hostnames before requests.
- Web tools block loopback/private/link-local/cloud-metadata IP ranges by default to reduce SSRF risk.
- Vault exfiltration risk is reduced with two-step `vault_copyto` semantics: read from `/vault`, then write to `/shared`/`/tmp` in a separate operation.

In `process` mode, host-level isolation is weaker than container/VM isolation, but the same WASM capability policies and security checks still apply.

---

## Part 2 — Quick Start (Install From Releases)

This is the **recommended path** if you just want Oxydra running quickly.

### 0) Choose a release tag

Pick a version from the [GitHub releases page](https://github.com/shantanugoel/oxydra/releases) and export it once:

```bash
export OXYDRA_TAG=v0.1.2   # replace with the release you want
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

Even in `--insecure` mode, WASM tool policies still enforce path boundaries and web SSRF checks.

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

---

## Comparison: Oxydra vs. ZeroClaw vs. IronClaw vs. MicroClaw

All four are Rust-based AI agent runtimes that emerged from the "\*Claw" ecosystem. Each takes a distinct philosophy.

### At a Glance

| | **Oxydra** | **ZeroClaw** | **IronClaw** | **MicroClaw** |
|---|---|---|---|---|
| **Author** | Independent | zeroclaw-labs | NEAR AI | microclaw org |
| **Language** | Rust | Rust | Rust | Rust |
| **Version** | 0.1.2 | ~0.1.x | 0.12.0 | ~0.1.x |
| **Maturity** | Pre-alpha | Early | Most mature | Early |
| **Primary focus** | Secure isolated agent orchestrator that evovles and learns | Ultra-lightweight personal bot | Secure always-on agent | Multi-channel chat bot |

---

### Security & Sandboxing

| Dimension | **Oxydra** | **ZeroClaw** | **IronClaw** | **MicroClaw** |
|---|---|---|---|---|
| **Isolation tiers** | **3 tiers**: Firecracker micro_vm (strongest) → Docker container → host process | 1: Docker (optional), process-level security | 2: Per-tool WASM + Docker job sandbox | 1: Docker (on/off switch) |
| **Firecracker VM isolation** | **Yes** (Linux; Docker VM on macOS) | No | No | No |
| **WASM sandbox** | **Yes — on ALL tiers** (wasmtime v41 WASI preopens, hardware-enforced capabilities) | Planned, not shipped | Yes — per tool | No |
| **WASM capability profiles** | **5 distinct profiles**: FileReadOnly, FileReadWrite, Web, VaultReadStep, VaultWriteStep | N/A | Yes, capability declarations | N/A |
| **Vault with 2-step semantics** | **Yes** — read and write are separate atomic ops linked by operation_id; vault never simultaneously readable+writable | No | Credential injection at boundary, not 2-step | No |
| **Output scrubbing** | **Yes** — path redaction + keyword scrubbing + **entropy-based detection** (Shannon ≥3.8 bits/char) | No | Leak scan pre/post execution | No |
| **Depth of defense model** | **5 layers**: runner isolation → WASM capability → security policy → output scrubbing → runtime guards | ~3 layers: allowlists, env hygiene, Docker | ~4 layers: WASM, allowlist, leak scan, credential injection | ~2 layers: Docker mode + security profiles |
| **SSRF protection** | **Yes** — IP blocklist + resolve-before-request | Partial (endpoint allowlisting) | Yes (endpoint allowlisting) | No |
| **Per-user workspace isolation** | **Yes** — dedicated guest VM pairs, 4 separate mount points per user | No | No | No |
| **TEE deployment** | Not yet | No | **Yes** (NEAR AI Cloud, verifiable attestation) | No |
| **External dependency for security** | None (embedded) | None | PostgreSQL required | Docker required |

---

### Architecture & Engineering Quality

| Dimension | **Oxydra** | **ZeroClaw** | **IronClaw** | **MicroClaw** |
|---|---|---|---|---|
| **Crate structure** | **12 crates, strict 3-layer hierarchy** enforced by compiler | Single-binary monolith | Rust workspace | Single-binary |
| **Dependency enforcement** | **`deny.toml`** license compliance (OSI allowlist), no duplicate crates, supply chain controls | Not documented | Not documented | Not documented |
| **Code quality policy** | **Zero clippy warnings** (denied in CI), 100% test coverage for critical paths | Not documented | Not documented | Not documented |
| **Config system** | **6-layer** precedence: built-ins → system → user → workspace → env vars → CLI flags | YAML flat config | TOML + NEAR AI account required | YAML flat config |
| **Type safety** | Typed identifiers (`ModelId`, `ProviderId`), build-time model catalog validation | Not documented | Not documented | Not documented |
| **Tool dispatch strategy** | **Parallel ReadOnly batch** + sequential SideEffecting; order preserved | Sequential | Parallel (priority scheduler) | Sequential |
| **Documentation** | **15-chapter architectural guidebook** (~120KB) + README + inline docs | README + wiki | README + CLAUDE.md | README + docs site |

---

### Tools & Capabilities

| Dimension | **Oxydra** | **ZeroClaw** | **IronClaw** | **MicroClaw** |
|---|---|---|---|---|
| **Built-in tools** | **23 tools**: file (6), web (2), vault, shell, memory (4), scratchpad (3), scheduler (4), delegation, media | Shell, file, web, memory | ~890 WASM skill registry | Bash, file (3), web, memory |
| **Tool macro** | **`#[tool]` proc macro** generates `FunctionDecl` from signatures | Not documented | WASM capability declarations | Not documented |
| **Scheduler** | **Yes** — cron/interval/once, queryable run history, pause/resume | Not documented | Yes (heartbeat/cron routines) | Yes (cron + one-shot) |
| **Multi-agent delegation** | **Yes** — typed specialist agents with per-agent tools, providers, turns | No | Yes (dynamic tool generation) | Via MCP federation |
| **Skill ecosystem** | Self learning + External planned | No external registry | **~890 curated WASM skills** | MCP federation + skills directory |
| **MCP support** | Planned | No | Yes (unsandboxed) | Yes |

---

### LLM & Channel Support

| Dimension | **Oxydra** | **ZeroClaw** | **IronClaw** | **MicroClaw** |
|---|---|---|---|---|
| **LLM providers** | OpenAI (Legacy and Responses), Anthropic, Gemini + OpenAI-compatible proxies | **22+ providers** | 6 backends | OpenAI, Anthropic, Google, Ollama, others |
| **Channel count** | 2 live (Telegram and TUI); framework to easily add more | **15+ channels** (WhatsApp, Signal, iMessage, Matrix, Nostr, QQ…) | 3 (Telegram, Slack, HTTP webhook) | 6 (Telegram, Discord, Slack, Feishu, IRC, Web) |

---

### Memory & Storage

| Dimension | **Oxydra** | **ZeroClaw** | **IronClaw** | **MicroClaw** |
|---|---|---|---|---|
| **Memory retrieval** | **Hybrid: vector + FTS5 BM25** (configurable weights) | Hybrid: 70% cosine + 30% BM25 | Hybrid: full-text + vector (RRF) | SQLite + optional semantic embeddings |
| **Embedding backend** | **model2vec-rs (Potion) or blake3 deterministic** — both embedded, zero external model API required | OpenAI `text-embedding-3-small` (requires API) | pgvector | OpenAI or Ollama (external) |
| **Storage engine** | **Embedded libSQL** (Turso optional for remote) | Embedded SQLite | **PostgreSQL + pgvector** (external, mandatory) | Embedded SQLite |
| **External DB required** | **No** | No | **Yes** | No |
| **Session management** | Full turn-level persistence, stale-session archival at 48h | Session resume | PostgreSQL-backed | SQLite + session resume |

---

### Deployment & Footprint

| Dimension | **Oxydra** | **ZeroClaw** | **IronClaw** | **MicroClaw** |
|---|---|---|---|---|
| **Deployment artifacts** | Runner + guest VM binaries + shell-daemon + TUI + Docker images | **Single ~16MB static binary** | Binary + PostgreSQL + pgvector | Single binary + YAML |
| **RAM at runtime** | Higher (VM pair per user) | **~5MB** | Higher (Postgres stack) | Moderate |
| **Platforms** | Linux (amd64, arm64), macOS (arm64) | Linux, macOS, ARM, x86, RISC-V | Linux primarily | Linux, macOS |
| **Setup complexity** | Medium (runner + guest images) | Low | **High** (NEAR AI account + PostgreSQL + pgvector) | Medium |

---

### Summary: Where Each Wins

| Project | Strongest at |
|---|---|
| **Oxydra** | Defense depth (5 layers, 3 isolation tiers, WASM on all tiers, entropy scrubbing, vault semantics), self learning, architectural rigor (compiler-enforced boundaries, deny.toml, zero clippy), no external DB required, configurable turn budget, context compaction |
| **ZeroClaw** | Ultra-low footprint (~5MB RAM), broadest channel coverage (15+), most LLM providers (22+), fastest startup |
| **IronClaw** | Largest skill ecosystem (~890 curated), TEE deployment, most mature (v0.12.0), NEAR AI cloud integration |
| **MicroClaw** | Simplest multi-channel chat automation, wide agentic iteration budget (100 turns), context compaction |

Oxydra's distinguishing technical advantages are in security architecture and engineering rigour while still maintaining as much, if not more, flexibility as others: no other project in this group combines Firecracker-level VM isolation + WASM capability profiles on every tier + a vault with 2-step atomic semantics + entropy-based output scrubbing + a compiler-enforced 5-layer defense model — all without requiring an external database or cloud account.
