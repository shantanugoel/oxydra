![Oxydra](./static/oxydra-1.png)

# Oxydra

Oxydra is a Rust-based AI agent orchestrator, that strives to run always-evolving and self learning capable agents with strong isolation, provider flexibility, tool execution, persistent memory, and multi-session/multi-agent/multi-user concurrency.

> ⚠️ **Maturity notice:** Oxydra is in alpha stage right now.

## Table of Contents

- [Intro & Features](#intro--features)
- [Quick Start](#quick-start)
- [Manual Install & Configuration](#manual-install--configuration)
- [Customizing Your Oxydra](#customizing-your-oxydra)
- [Use Latest From Repository (and Contribute)](#use-latest-from-repository-and-contribute)
- [Comparison: Oxydra vs. ZeroClaw vs. IronClaw vs. MicroClaw](#comparison-oxydra-vs-zeroclaw-vs-ironclaw-vs-microclaw)

## Intro & Features

Oxydra is designed for people who want an agent runtime they can self-host, inspect, and evolve — not a black box.

### Why it is useful

- **Provider-agnostic LLM layer**: OpenAI, Anthropic, Gemini, and OpenAI Responses API, with streaming, retries, and model-catalog-based validation.
- **Multi-agent orchestration**: Define specialist subagents with distinct prompts/tools/models and delegate tasks between them.
- **Async by default**: Multiple sessions/chats can run in parallel without blocking each other.
- **Multi-user**: It is trivial to use oxydra in a multi user environment, and each user is isolated from other users
- **Tooling with safety rails**: `#[tool]` macro, auto schema generation, safety tiers, and workspace path hardening (`/shared`, `/tmp`, `/vault`).
- **Structured runtime loop**: Planning, tool dispatch, retries, context budget management, and bounded turn/cost controls.
- **Persistent memory**: libSQL-backed hybrid retrieval (vector + FTS), summarization, memory CRUD tools, and session scratchpad.
- **Durable scheduler**: One-off and periodic jobs with run history and notification routing.
- **Skill system and browser automation**: Markdown-based workflow skills injected into the agent's system prompt. The built-in **Browser Automation** skill drives headless Chrome via Pinchtab's REST API through the existing sandboxed shell — navigate pages, click/fill forms, take screenshots, download files — all without a dedicated tool. Users can author custom skills or override built-ins.
- **Isolation model**: Process, container, and microVM tiers with WASM sandboxing as defense-in-depth.
- **Gateway + channels**: WebSocket gateway with per-user multi-session support, plus Telegram channel adapter.
- **Web configurator**: Browser-based dashboard for configuration editing, daemon control, log viewing, and guided onboarding — no extra tooling required.
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

## Quick Start

The fastest path to a running Oxydra instance. You'll need [Docker](https://docs.docker.com/get-started/get-docker/) installed and running for the default `container` isolation tier. If Docker is unavailable, see [Manual Install](#manual-install--configuration) for the process-mode fallback.

### 1) Choose a release tag

Pick a version from the [GitHub releases page](https://github.com/shantanugoel/oxydra/releases) and export it:

```bash
export OXYDRA_TAG=v0.1.6   # replace with the release you want
```

### 2) Install with one command

```bash
curl -fsSL https://raw.githubusercontent.com/shantanugoel/oxydra/main/scripts/install-release.sh | bash -s -- --tag "$OXYDRA_TAG" --base-dir "$PWD"
```

This installs the `runner`, `oxydra-vm`, `shell-daemon`, and `oxydra-tui` binaries to `~/.local/bin` and copies starter config templates to `.oxydra/`. If `~/.local/bin` is not in `PATH`:

```bash
export PATH="$HOME/.local/bin:$PATH"
```

### 3) Configure with the web configurator

Start the web configurator:

```bash
runner --config .oxydra/runner.toml web
```

Open **http://127.0.0.1:9400** in your browser and navigate to **Agent Config**. The **Core Setup** section (highlighted in orange below) is the only part you need to configure before the agent can run — everything else on the page is optional.

![Web Configurator — Agent Config Core Setup](./static/web-configurator-minimal.png)

Core Setup has two areas to fill in:

- **Model Selection** — pick your **Provider** and **Model** from the dropdowns
- **Providers** — add a matching provider entry and set its `api_key_env` to the environment variable name that holds your API key (e.g. `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, or `GEMINI_API_KEY`)

Once saved, stop the web configurator with `Ctrl+C`.

### 4) Export your provider API key

The runner reads provider credentials from environment variables. Before starting the daemon, export the key whose name you set in the Providers section above:

```bash
export OPENAI_API_KEY=your-key-here
# or: export ANTHROPIC_API_KEY=...
# or: export GEMINI_API_KEY=...
```

### 5) Start the daemon and connect

Run the daemon (terminal 1):

```bash
runner --config .oxydra/runner.toml --user alice start
```

Connect with the TUI (terminal 2):

```bash
runner --tui --config .oxydra/runner.toml --user alice
```

That's it — Oxydra is running. For install variants, manual TOML configuration, Docker setup on Linux, Telegram, TUI commands, process-mode fallback, and troubleshooting, see [Manual Install & Configuration](#manual-install--configuration).

---

## Manual Install & Configuration

Use this path for more control over the install process, direct TOML editing instead of the web configurator, or to set up optional features like Telegram.

### 0) Choose a release tag

Pick a version from the [GitHub releases page](https://github.com/shantanugoel/oxydra/releases) and export it once:

```bash
export OXYDRA_TAG=v0.1.6   # replace with the release you want
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
- On upgrades, verifies release checksum (`SHA256SUMS`), backs up existing binaries/config, and updates existing `runner.toml` `[guest_images]` tags to the installed release tag (without replacing other settings)
- Leaves existing config files unchanged outside the targeted image-tag update (use `--overwrite-config` to replace templates)

Useful variants:

```bash
# Preview upgrade actions without changing anything
curl -fsSL https://raw.githubusercontent.com/shantanugoel/oxydra/main/scripts/install-release.sh | bash -s -- --tag "$OXYDRA_TAG" --dry-run

# Install latest release (no pin)
curl -fsSL https://raw.githubusercontent.com/shantanugoel/oxydra/main/scripts/install-release.sh | bash

# Install to /usr/local/bin (uses sudo)
curl -fsSL https://raw.githubusercontent.com/shantanugoel/oxydra/main/scripts/install-release.sh | bash -s -- --tag "$OXYDRA_TAG" --system

# Install into a different project directory
curl -fsSL https://raw.githubusercontent.com/shantanugoel/oxydra/main/scripts/install-release.sh | bash -s -- --tag "$OXYDRA_TAG" --base-dir /path/to/project

# Install binaries only (skip config scaffolding)
curl -fsSL https://raw.githubusercontent.com/shantanugoel/oxydra/main/scripts/install-release.sh | bash -s -- --tag "$OXYDRA_TAG" --skip-config

# Non-interactive upgrade (auto-confirm prompts)
curl -fsSL https://raw.githubusercontent.com/shantanugoel/oxydra/main/scripts/install-release.sh | bash -s -- --tag "$OXYDRA_TAG" --yes

# Skip Docker pre-pull after install
curl -fsSL https://raw.githubusercontent.com/shantanugoel/oxydra/main/scripts/install-release.sh | bash -s -- --tag "$OXYDRA_TAG" --no-pull
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

### 2) Review and configure

If you used Option A, the installer already created:

- `.oxydra/agent.toml`
- `.oxydra/runner.toml`
- `.oxydra/users/alice.toml`

#### Configure `runner.toml`

Verify `.oxydra/runner.toml` guest image tags match the release you installed:

```toml
default_tier = "container"

[guest_images]
oxydra_vm = "ghcr.io/shantanugoel/oxydra-vm:${OXYDRA_TAG}"
shell_vm  = "ghcr.io/shantanugoel/shell-vm:${OXYDRA_TAG}"
```

If you plan to use `micro_vm` instead of `container`:

- macOS: install and run Docker Desktop
- Linux: install `firecracker`, set `default_tier = "micro_vm"`, and configure `guest_images.firecracker_oxydra_vm_config` (plus `guest_images.firecracker_shell_vm_config` if you want shell/browser sidecar)

If you want a user id other than `alice`, update `[users.alice]` in `.oxydra/runner.toml` and rename `.oxydra/users/alice.toml` accordingly.

#### Configure `agent.toml`

Edit `.oxydra/agent.toml`:

- Set `[selection].provider` and `[selection].model`
- Add the matching `[providers.registry.<name>]` entry with the correct `api_key_env`:
  - OpenAI example: `api_key_env = "OPENAI_API_KEY"`
  - Anthropic example: `api_key_env = "ANTHROPIC_API_KEY"`
  - Gemini example: `api_key_env = "GEMINI_API_KEY"`

#### Optional: use the web configurator instead

If you prefer a browser UI to editing TOML directly, the web configurator provides a guided interface for the same settings:

```bash
runner --config .oxydra/runner.toml web
```

Open **http://127.0.0.1:9400** and navigate to **Agent Config**. See [Quick Start — step 3](#3-configure-with-the-web-configurator) for details on the Core Setup section. Once saved, stop the web configurator with `Ctrl+C`.

### 3) Ensure Docker is ready (Linux)

```bash
# Start Docker daemon and enable it on boot
sudo systemctl enable --now docker

# Add your user to the docker group so you don't need sudo
sudo usermod -aG docker $USER
newgrp docker   # apply in the current shell without logging out
```

The guest images are public on ghcr.io and pull without authentication. If you ever hit a `manifest unknown` 404, double-check that the tag in `runner.toml` includes the `v` prefix (e.g. `v0.1.6`, not `0.1.2`).

### 4) Set your provider API key

```bash
export OPENAI_API_KEY=your-key-here
# or: export ANTHROPIC_API_KEY=...
# or: export GEMINI_API_KEY=...
```

### 5) Start and connect

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

### 6) If Docker is unavailable

Use process mode (lower safety, no shell/browser tools):

```bash
runner --config .oxydra/runner.toml --user alice --insecure start
runner --tui --config .oxydra/runner.toml --user alice
```

Even in `--insecure` mode, WASM tool policies still enforce path boundaries and web SSRF checks.

### 7) TUI session commands

| Command | Meaning |
|---|---|
| `/new` | Create a fresh session |
| `/new <name>` | Create a named session |
| `/sessions` | List sessions |
| `/switch <id>` | Switch by session id (prefix supported) |
| `/cancel` | Cancel active turn in current session |
| `/cancelall` | Cancel active turns across sessions |
| `/status` | Show current session/runtime state |

### 8) (Optional) Enable Telegram

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

### 9) (Optional) Web Configurator for ongoing management

The web configurator provides a browser-based dashboard for managing Oxydra without editing TOML files directly — useful for day-to-day config changes after your initial setup.

```bash
# Start the web configurator
runner --config .oxydra/runner.toml web

# Custom bind address
runner --config .oxydra/runner.toml web --bind 0.0.0.0:8080
```

Then open `http://127.0.0.1:9400` in your browser. The dashboard offers:
- **Config editors** for runner, agent, and user settings (with validation and backups)
- **Control panel** to start/stop/restart daemons
- **Log viewer** with filtering and auto-refresh
- **Status dashboard** showing registered users and daemon health

The web server binds to localhost only by default. To enable token auth for remote access, add to `runner.toml`:

```toml
[web]
auth_mode = "token"
auth_token_env = "OXYDRA_WEB_TOKEN"
```

### Troubleshooting

| Symptom | Fix |
|---|---|
| Need to check logs | `oxydra-runner help logs` will show you the options that you have for logging across runner and containers in one place |
| `oxydra-tui was not found in PATH` | Ensure install dir is in `PATH` or run the binary directly |
| Docker unreachable / `client error (Connect)` | Start Docker (`sudo systemctl start docker`); for Colima set `DOCKER_HOST=unix://$HOME/.colima/default/docker.sock` |
| `Permission denied` accessing Docker socket | Add your user to the docker group: `sudo usermod -aG docker $USER` then run `newgrp docker` or log out and back in |
| `pull_image` fails with `manifest unknown` or 404 | Check the tag in `runner.toml` includes the `v` prefix (e.g. `v0.1.6` not `0.1.2`); see [published images](https://github.com/shantanugoel/oxydra/pkgs/container/oxydra-vm) for available tags |
| Telegram bot does not respond | Verify `bot_token_env` points to an exported token and your Telegram user ID is listed in `[[channels.telegram.senders]]` |
| `micro_vm` start fails on macOS | Ensure Docker Desktop is installed and running |
| `micro_vm` start fails on Linux (`firecracker` or config error) | Install `firecracker` and set `guest_images.firecracker_oxydra_vm_config` (and `guest_images.firecracker_shell_vm_config` for sidecar) |
| `unknown model for catalog provider` | Use `runner catalog show` to inspect known models, or set `catalog.skip_catalog_validation = true` |
| 401/Unauthorized from provider | Check API key env var and `api_key_env` name in `agent.toml` |

---

## Customizing Your Oxydra

After initial setup, the most impactful customizations are listed below. Everything is controlled through `agent.toml` and `users/alice.toml` (or the web configurator). Restart the runner for changes to take effect.

### Enabling Browser Automation

Oxydra can control a headless Chrome browser (via [Pinchtab](https://github.com/pinchtab/pinchtab)) from within its sandboxed container. When enabled, the agent can navigate web pages, interact with forms, take screenshots, read page content, download files, and deliver results directly to you — all guided by an injected skill document and driven through ordinary shell commands.

**Requires:** `container` isolation tier. Browser is not available in `process` (`--insecure`) mode.

Uncomment in `.oxydra/users/alice.toml`:

```toml
[behavior]
browser_enabled = true
```

Or toggle it in the web configurator under **User Config → Behavior → Browser Access**.

**How it works:** The Browser Automation skill is a markdown document embedded in the Oxydra binary. When browser is enabled and Pinchtab starts successfully, the skill is automatically injected into the agent's system prompt with the Pinchtab API URL pre-filled. The shell sandbox is also automatically extended to allow `curl`, `jq`, `sleep`, and shell operators (`&&`, `|`, `$()`) — no manual shell config changes needed. The agent then drives the browser entirely through `curl` calls to Pinchtab's REST API, keeping all browser activity inside the sandboxed container.

### Custom Specialist Agents

Specialist agents let you configure separate personas, tool scopes, and model choices for different tasks. The main agent can delegate work to specialists using the `delegate_to_agent` tool, or you can address a specialist directly from the TUI using `/new <agent-name>`.

Define specialists in `.oxydra/agent.toml`:

```toml
[agents.researcher]
system_prompt = "You are a research specialist focused on evidence gathering."
tools         = ["web_search", "web_fetch", "file_read"]
max_turns     = 12
max_cost      = 0.50

[agents.researcher.selection]
provider = "anthropic"
model    = "claude-3-5-haiku-latest"

[agents.coder]
system_prompt = "You are a coding specialist for implementation and debugging."
tools         = ["file_read", "file_edit", "file_write", "shell_exec"]
# No [agents.coder.selection]: inherits the caller's current provider/model.
```

The `tools` list restricts which tools a specialist can use. Omit it entirely to inherit the default tool set. See [`examples/config/agent.toml`](examples/config/agent.toml) for more examples including multimodal and image-generation agents.

### Turn Limits and Cost Budgets

The default configuration allows up to **25 turns** per interactive session with no cost cap. Tighten or loosen these in `.oxydra/agent.toml`:

```toml
[runtime]
max_turns         = 15     # Reduce for more focused tasks (default: 25)
turn_timeout_secs = 60     # Per-LLM-call timeout in seconds
# max_cost        = 0.50   # Optional cost cap (provider-reported units). Uncomment to enable.
```

For scheduled tasks, set stricter per-run budgets under `[scheduler]`:

```toml
[scheduler]
enabled  = true
max_turns = 8     # Per-run turn limit for scheduled tasks
max_cost  = 0.25  # Per-run cost cap
```

The gateway also has session-level knobs for multi-session environments:

```toml
[gateway]
max_sessions_per_user         = 50   # How many sessions a user can have open
max_concurrent_turns_per_user = 10   # How many turns can run in parallel
session_idle_ttl_hours        = 48   # When idle sessions are archived
```

### Shell Command Allowlist

By default the agent has access to ~30 common shell commands (find, grep, git, python3, etc.). Extend or restrict the list in `.oxydra/agent.toml`:

```toml
[tools.shell]
allow            = ["npm", "make", "docker", "rg"]   # Add to the default allowlist
deny             = ["rm"]                             # Remove from the defaults
allow_operators  = true                               # Enable &&, ||, |, $() chaining
env_keys         = ["NPM_TOKEN", "GH_TOKEN"]          # Forward specific env vars into the shell
```

To replace the default list entirely and have full control over allowed commands:

```toml
[tools.shell]
replace_defaults = true
allow            = ["git", "python3", "pip", "npm"]
```

### Skills: Custom Workflows and Overrides

Skills are markdown files that teach the agent domain-specific workflows by extending its system prompt. Oxydra ships with a built-in **Browser Automation** skill; you can author your own or override the built-ins.

#### Writing a custom skill

A skill is a folder containing a `SKILL.md` file with YAML frontmatter followed by markdown content:

```markdown
---
name: my-git-workflow
description: Git conventions and PR format for this project
activation: always
priority: 80
---

## Git Workflow

Always create a branch before making changes: `git checkout -b feat/<name>`

Commit messages must follow Conventional Commits: `feat:`, `fix:`, `docs:`, etc.
PRs should reference the issue with `Closes #<issue-number>`.
```

**Frontmatter fields:**

| Field | Required | Default | Description |
|---|---|---|---|
| `name` | Yes | — | Unique identifier (kebab-case). Used for deduplication across locations. |
| `description` | Yes | — | One-line summary shown in diagnostic logs. |
| `activation` | No | `auto` | `auto` — inject when all conditions are met; `always` — always inject; `manual` — never auto-inject. |
| `requires` | No | `[]` | Tool names that must be **ready** for this skill to activate (e.g. `["shell_exec"]`). |
| `env` | No | `[]` | Environment variable names that must be set. Their values are available as `{{VAR}}` placeholders in the skill body (for non-sensitive values like URLs). |
| `priority` | No | `100` | Ordering when multiple skills are active; lower numbers appear earlier in the prompt. |

Skills are capped at approximately **3,000 tokens** (~12 KB) to avoid prompt bloat. For large reference material (full API docs, parameter tables), keep your `SKILL.md` concise and place supplementary files in a `references/` subfolder — then mention in the skill body that the agent can `cat` those files on demand.

#### Where to place skills

| Location | Scope | Path |
|---|---|---|
| Workspace skill | Project-specific (highest priority) | `.oxydra/skills/<SkillName>/SKILL.md` |
| User skill | Shared across all projects | `~/.config/oxydra/skills/<SkillName>/SKILL.md` |

The **same `name` field** determines which skill wins when multiple tiers define the same skill: workspace overrides user, user overrides built-ins.

#### Overriding or disabling a built-in skill

To override the built-in browser automation skill with a customized version, create a `SKILL.md` with the **exact same `name`** at workspace or user level:

```markdown
---
name: browser-automation     # Must match the built-in name exactly
description: My custom browser skill
activation: auto
requires:
  - shell_exec
env:
  - PINCHTAB_URL
priority: 50
---

## Browser Automation (Custom)

...your customized instructions...
```

To effectively **disable** a built-in skill, place a version with `activation: manual` and the matching `name` at workspace or user level — `manual` skills are never auto-injected.

---

## Use Latest From Repository (and Contribute)

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
./scripts/test-install-release.sh
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

All four are Rust-based AI agent runtimes that emerged from the "\*Claw" ecosystem, although Oxydra strives to be unto its own and not a `*claw`. Each takes a distinct philosophy.

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

Oxydra's distinguishing technical advantages are in security architecture and engineering rigour while still maintaining as much, if not more, flexibility as others and keeping self-learning evolution as a key goal: no other project in this group combines Firecracker-level VM isolation + WASM capability profiles on every tier + a vault with 2-step atomic semantics + entropy-based output scrubbing + a compiler-enforced 5-layer defense model — all without requiring an external database or cloud account.
