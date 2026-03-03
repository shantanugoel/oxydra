# Chapter 8: Runner and Guest Lifecycle

## Overview

The runner (`oxydra-runner`) is the host-side entry point for Oxydra. Its single responsibility is to spawn and wire isolated execution environments, then stand aside. The runner does not execute user code, does not access user data contents, and does not participate in agent turns.

## Architecture

```
┌─────────────────────────────────────────────────────┐
│                    Host Machine                      │
│                                                      │
│  ┌──────────────┐                                    │
│  │ oxydra-runner │                                   │
│  │  (main.rs)   │                                    │
│  └───────┬──────┘                                    │
│          │ spawn per-user pair                       │
│    ┌─────┴──────────────────────────┐                │
│    │                                │                │
│    ▼                                ▼                │
│  ┌──────────────┐           ┌──────────────┐        │
│  │  oxydra-vm   │  vsock/   │  shell-vm    │        │
│  │              │◄─────────►│              │        │
│  │ Gateway      │  unix     │ Shell Daemon │        │
│  │ Runtime      │  socket   │ Browser Pool │        │
│  │ Memory       │           │              │        │
│  │ Tools        │           │              │        │
│  └──────────────┘           └──────────────┘        │
│                                                      │
│  Workspace: <root>/<user_id>/                        │
│    ├── shared/     (persistent data)                 │
│    ├── tmp/        (temporary + gateway-endpoint)    │
│    ├── vault/      (sensitive credentials)           │
│    ├── ipc/        (sockets, bootstrap files,        │
│    │                runner-control.sock)              │
│    ├── logs/       (guest process logs)              │
│    └── internal/   (oxydra-vm only: DB, config)      │
└─────────────────────────────────────────────────────┘
```

## Runner Startup Flow

When you run `oxydra-runner`:

1. **CLI parsing** — reads `--config`, `--user`, `--insecure`, `--tui` flags and the subcommand (`start`, `stop`, `status`, `restart`, `catalog`)
2. **Global config loading** — reads `runner.toml` (workspace root, user registrations, default tier, guest images)
3. **User resolution** — if `--user` is omitted and exactly one user is configured, auto-selects; otherwise requires explicit `--user`
4. **Workspace provisioning** — creates `<workspace_root>/<user_id>/{shared,tmp,vault,ipc,logs,internal}` if they don't exist
5. **Backend selection** — based on `SandboxTier` (or `Process` if `--insecure`):
   - `MicroVm` → provisions via platform-specific VM backend
   - `Container` → provisions via Docker/OCI API (`bollard`)
   - `Process` → spawns as local child process
6. **Guest launch** — starts `oxydra-vm` binary with bootstrap data
7. **Daemon loop** — binds a Unix control socket at `<workspace>/ipc/runner-control.sock` and serves lifecycle requests until shutdown or Ctrl+C

## CLI Lifecycle Commands

The runner CLI provides four top-level subcommands for managing daemon sessions:

| Command | Description |
|---------|-------------|
| `runner start` | Launch guest sandbox and enter daemon mode |
| `runner stop` | Send shutdown to a running daemon via its control socket |
| `runner status` | Query health and capability info from a running daemon |
| `runner restart` | Stop an existing daemon (if running), then start a new one |

All commands accept `--config`, `--user`, and other global flags. The `start` and `restart` commands also accept `--insecure`, `--env`, and `--env-file`.

```bash
# Start a session
runner --user alice start

# Check status
runner --user alice status

# Stop
runner --user alice stop

# Restart with extra env vars
runner --user alice -e API_KEY=xxx restart
```

### Control Socket Protocol

Each daemon binds a Unix domain socket at `<workspace>/ipc/runner-control.sock`. Lifecycle commands connect to this socket, send a length-prefixed JSON frame containing a `RunnerControl` request, and read back a `RunnerControlResponse`.

- **`stop`** sends `RunnerControl::ShutdownUser` — the daemon shuts down the guest, responds with `ShutdownStatus`, removes the socket file, and exits.
- **`status`** sends `RunnerControl::HealthCheck` — the daemon responds with `HealthStatus` containing user id, sandbox tier, tool availability, runtime PID, and degraded reasons.
- **`restart`** sends a shutdown, waits for the socket file to be removed (with a 10-second timeout and forced cleanup), then starts a new daemon.

If the control socket doesn't exist or the connection is refused, `stop` and `status` return a clear "no running server" error with a hint to use `runner start`.

The legacy `--daemon` flag continues to work for backward compatibility — it enters the same daemon loop as `runner start` after the existing startup sequence.

### Web Configurator (`runner web`)

The `runner web` subcommand starts an HTTP-based web configurator that provides a browser UI and REST API for configuration management, daemon lifecycle control, and log viewing.

```bash
# Start the web configurator (default bind: 127.0.0.1:9400)
runner web

# Custom bind address
runner web --bind 0.0.0.0:8080
```

The web configurator runs as an independent process, separate from per-user daemons. It:

- **Reads/writes config files directly** — works even when no daemon is running
- **Proxies lifecycle commands** to running daemons via the control socket protocol
- **Provides an onboarding wizard** for first-time setup
- **Masks secrets** in all API responses (API keys, auth tokens)
- **Creates backups** before every config write (keeps last 10)
- **Preserves TOML comments** during edits via `toml_edit`

#### Security

- Binds to `127.0.0.1` by default (loopback only)
- Host header validation blocks DNS rebinding attacks
- Content-Type enforcement on all mutation endpoints
- Optional bearer token authentication via `[web]` config

#### REST API

All endpoints use a JSON envelope (`{ data, meta }` or `{ error, meta }`). Key routes:

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/api/v1/meta` | Server version, config path |
| `GET` | `/api/v1/meta/schema` | Field metadata for all config types (labels, descriptions, input types, constraints, enum options, dynamic sources) |
| `GET` | `/api/v1/catalog` | Resolved model catalog (cache → pinned snapshot) with provider/model data |
| `GET` | `/api/v1/catalog/status` | Catalog source info: loaded file, last modified, provider/model counts |
| `POST` | `/api/v1/catalog/refresh` | Trigger catalog fetch from models.dev or pinned URL |
| `GET` | `/api/v1/status` | Aggregated daemon status for all users |
| `GET` | `/api/v1/config/runner` | Runner config (masked) |
| `PATCH` | `/api/v1/config/runner` | JSON merge patch on runner config |
| `POST` | `/api/v1/config/runner/validate` | Validate runner config changes |
| `GET` | `/api/v1/config/agent` | Agent config (masked) |
| `GET` | `/api/v1/config/agent/effective` | Effective agent config after layered merging |
| `PATCH` | `/api/v1/config/agent` | JSON merge patch on agent config |
| `POST` | `/api/v1/config/agent/validate` | Validate agent config changes |
| `GET/POST` | `/api/v1/config/users` | List / create users |
| `GET/PATCH/DELETE` | `/api/v1/config/users/{id}` | Read / update / delete user |
| `POST` | `/api/v1/config/users/{id}/validate` | Validate user config changes |
| `POST` | `/api/v1/control/{id}/start\|stop\|restart` | Daemon lifecycle |
| `GET` | `/api/v1/logs/{id}` | Log retrieval with filtering |
| `GET` | `/api/v1/onboarding/status` | First-run setup detection |

#### Schema Metadata (`/api/v1/meta/schema`)

The schema metadata endpoint is the backend source of truth for the structured form UI. It returns purpose-built metadata (not JSON Schema) organized by config type (`agent`, `runner`, `user`) and section:

- **Field metadata**: label, description, input type, default value, constraints (min/max/step), nullable flag, required flag, placeholder text
- **Input types**: `text`, `number`, `boolean`, `secret`, `select`, `select_dynamic`, `model_picker`, `multiline`, `multi_select`, `tag_list`, `key_value_map`, `readonly`
- **Dynamic sources**: runtime-resolved lists including registered providers (from effective layered config), canonical tool names (from the tool registry), timezone suggestions, and enum option sets for all typed enums (`ProviderType`, `SandboxTier`, `WebAuthMode`, `MemoryEmbeddingBackend`, `Model2vecModel`)
- **Collection metadata**: identifies map-type and array-type dynamic collections (providers, agents, credential_refs, senders)
- **Section organization**: group headers, optional section toggles, subsections (e.g., "Advanced Overrides" inside providers)

This hybrid approach combines auto-derived metadata (defaults from Rust types, enum variants, tool names from the registry) with manually-authored UX metadata (labels, descriptions, input widget choices, visibility behavior).

#### Model Catalog (`/api/v1/catalog`)

The catalog endpoint returns the resolved model catalog for the model picker UI. It uses the same three-tier resolution as the runtime: cached catalog → workspace cache → compiled-in pinned snapshot. Each provider entry includes its models with UI-relevant fields: id, name, family, capability flags (reasoning, tool_call, attachment), input modalities, cost, and token limits.

#### Structured Form UI

The web configurator renders config editors as structured, section-based forms instead of generic text inputs:

- **Every field** has a human-readable label, description/help text, and the appropriate input widget (dropdowns for enums, model picker for model selection, tag chips for lists, multi-select for tool allowlists, masked inputs for secrets)
- **Dynamic collections** (providers, agents, credential refs, Telegram senders) use collapsible card-based collection editors with add/remove support
- **Model selection** uses a searchable catalog-aware picker grouped by provider with capability badges
- **Optional config sections** (tools.web_search, tools.shell, etc.) have enable/disable toggles that correctly map to merge-patch creation/removal semantics
- **Catalog browsing** shows current catalog status (source, provider/model counts) with a refresh button
- **Empty state**: when a config file doesn't exist, a banner indicates defaults are shown and the file will be created on first save
- **Mobile navigation** uses a hamburger menu with a slide-out sidebar overlay
- **Accessibility**: all form fields are properly labeled with `aria-describedby` linking to help text, keyboard navigation through all sections and inputs, `aria-expanded` on collapsible sections, and ARIA live regions for error messages

The embedded SPA is served at `/` and uses Alpine.js with hash-based routing. No build toolchain is required — all frontend assets are compiled into the binary via `rust-embed`.

## Bootstrap Envelope

The runner communicates startup configuration to the guest via a length-prefixed JSON envelope, avoiding environment variables for sensitive data.

### Structure

```rust
pub struct RunnerBootstrapEnvelope {
    pub user_id: String,
    pub sandbox_tier: SandboxTier,
    pub workspace_root: PathBuf,
    pub sidecar_endpoint: Option<String>,  // shell-vm socket path
    pub runtime_policy: RunnerRuntimePolicy,
    pub startup_status: StartupStatusReport,
}
```

### Transport

- **Process tier:** Written to the guest's `stdin` as a 4-byte length prefix (big-endian u32) followed by JSON bytes. The guest reads with `--bootstrap-stdin`.
- **Container/MicroVM tier:** The runner writes the bootstrap envelope as a plain JSON file to `<workspace>/tmp/bootstrap/runner_bootstrap.json`, which is bind-mounted into the container at `/run/oxydra/bootstrap`. The guest reads it with `--bootstrap-file /run/oxydra/bootstrap`. The file reader wraps the raw JSON in a 4-byte length prefix internally so `bootstrap_vm_runtime` receives the same frame format regardless of transport.

The `--bootstrap-file` flag also accepts the `OXYDRA_BOOTSTRAP_FILE` environment variable as a fallback, which the container launcher sets automatically.

### Consumption

The `oxydra-vm` binary reads the envelope from stdin (`--bootstrap-stdin`) or from a file (`--bootstrap-file <PATH>`) and passes it to `runner::bootstrap_vm_runtime`:

1. Loads `AgentConfig` from config files
2. Initializes the LLM provider based on `selection.provider`
3. Initializes the memory backend (libSQL)
4. Registers tools based on `StartupStatusReport` (determines which tools are available)
5. Starts the Gateway WebSocket server on a random available port
6. Writes the WebSocket endpoint to `<workspace>/tmp/gateway-endpoint`

## --tui Mode (Connect-Only)

When `--tui` is passed, the runner does not spawn a new guest. Instead:

1. Looks for `<workspace>/tmp/gateway-endpoint` marker file
2. Reads the WebSocket URL from the file (e.g., `ws://127.0.0.1:5678/ws`)
3. Performs a WebSocket handshake:
   - Sends `GatewayClientFrame::Hello`
   - Receives `GatewayServerFrame::HelloAck` with `runtime_session_id`
   - Performs health check
4. Hands over to the TUI channel adapter for interactive use

If no running guest exists (marker file missing), the runner exits with a clear error.

## --insecure Mode (Process Tier)

When `--insecure` is passed:

1. `SandboxTier` is forced to `Process` regardless of config
2. `oxydra-vm` is spawned as a direct child process (no container/VM boundary)
3. Shell and browser tools are **disabled** because they lack sandbox isolation
4. `StartupStatusReport` includes `InsecureProcessTier` degradation reason
5. A prominent warning is logged that isolation is degraded and not production-safe
6. Best-effort hardening is attempted:
   - **Linux:** Landlock filesystem restrictions (if kernel supports it)
   - **macOS:** Seatbelt (`sandbox-exec`) profile integration

## Guest Initialization (oxydra-vm)

**File:** `runner/src/bin/oxydra-vm.rs`

The `oxydra-vm` binary is the guest-side entry point. It accepts the following flags:

| Flag | Description |
|------|-------------|
| `--user-id <ID>` | Required. User identifier. |
| `--workspace-root <PATH>` | Required. Root of the user workspace. |
| `--bootstrap-stdin` | Read bootstrap envelope from stdin (Process tier). |
| `--bootstrap-file <PATH>` | Read bootstrap envelope from a JSON file (Container/MicroVM tier). Also settable via `OXYDRA_BOOTSTRAP_FILE` env var. |
| `--gateway-bind <ADDR>` | Gateway listen address (default `127.0.0.1:0`). |

Startup sequence:

```
1. Initialize tracing subscriber
2. Read bootstrap envelope (stdin via --bootstrap-stdin, or file via --bootstrap-file, or None)
3. Load AgentConfig (figment layered config)
4. Validate config (version, provider, limits)
5. Build Provider (OpenAI or Anthropic + ReliableProvider wrapper)
6. Build Memory (LibsqlMemory with embedded libSQL)
7. Build ToolRegistry (based on startup status)
8. Create AgentRuntime
9. Start GatewayServer (Axum WebSocket on 127.0.0.1:random_port)
10. Write gateway endpoint to tmp/gateway-endpoint
11. Wait for connections
```

## Shell Daemon

**Files:** `shell-daemon/src/lib.rs` (library), `shell-daemon/src/bin/shell-daemon.rs` (binary)

The shell daemon runs inside `shell-vm` and provides an RPC interface for shell command and browser session execution. The standalone `shell-daemon` binary is a thin wrapper that accepts a `--socket <PATH>` argument, removes any stale socket file, binds a `UnixListener`, and delegates to `ShellDaemonServer::serve_unix_listener()`.

In container/MicroVM tiers, the runner launches the `shell-vm` container with:
```
ENTRYPOINT ["/usr/local/bin/shell-daemon"]
CMD ["--socket", "/ipc/shell-daemon.sock"]
```

The ShellVm container uses virtual mount paths instead of host paths:
- Host workspace `shared/` → mounted at `/shared` (also the working directory)
- Host workspace `tmp/` → mounted at `/tmp`
- Host workspace IPC directory → mounted at `/ipc`

This means commands like `pwd` return `/shared` rather than the host path, preventing host filesystem details from leaking to the LLM. The shell-daemon socket at `/ipc/shell-daemon.sock` maps to the same physical file as the host-side path via the bind mount, so the `oxydra-vm` container can reach it without any network configuration.

### Protocol

All communication uses length-prefixed JSON frames over vsock or Unix sockets.

```rust
pub enum ShellDaemonRequest {
    SpawnSession { session_id: String, working_dir: Option<PathBuf> },
    ExecCommand { session_id: String, command: String, timeout_ms: Option<u64> },
    StreamOutput { session_id: String },
    KillSession { session_id: String },
}

pub enum ShellDaemonResponse {
    SessionSpawned { session_id: String },
    CommandOutput { stdout: String, stderr: String, exit_code: i32 },
    StreamOutputChunk { data: String, stream_type: StreamType },
    SessionKilled { session_id: String },
    Error { message: String },
}
```

#### Socket Desync Recovery

The protocol is strictly request-response over a single framed stream.  If the
client-side future is cancelled (e.g. by a `turn_timeout`) while waiting for a
server response, the socket may retain a stale response frame.  The next
request would then read the previous command's response, permanently desyncing
the two sides.

`VsockShellSession` detects this by classifying certain errors as
*recoverable desync* (`UnexpectedResponse`, stale `ShellDaemon` errors,
`ConnectionClosed`, transport failures).  On such an error it tears down the
current socket, opens a new connection to the sidecar, spawns a fresh session,
and retries the command.  For `stream_output` failures the command output is
lost, so an explicit "please retry" error is returned to the LLM.

> **Future consideration:** If additional desync-related issues are observed,
> adding request-id correlation to the RPC client could detect and drain stale
> responses without tearing down the connection.

### Session Management

The `SessionManager` maintains a map of active shell sessions. Each session is an isolated shell process with its own working directory, environment, and lifecycle. Sessions can be spawned, used for multiple commands, streamed, and killed independently.

### Shell Session Backends

Two backends implement the `ShellSession` trait:

| Backend | Use Case | Mechanism |
|---------|----------|-----------|
| `VsockShellSession` | Production (Container/MicroVM) | Connects to shell-daemon over vsock/Unix socket |
| `LocalProcessShellSession` | Development/Testing | Spawns host processes directly |

## Browser Sidecar (Pinchtab)

When `browser_enabled = true` in the user config (`[behavior]` section of `runner-user.toml`), the runner provisions a [Pinchtab](https://github.com/pinchtab/pinchtab) process alongside `shell-daemon` inside the same `shell-vm` container. Pinchtab is a headless Chrome automation server with a REST API. The agent drives the browser via `curl` commands through the existing `shell_exec` tool, guided by the Browser Automation skill injected into the system prompt (see Chapter 14).

Browser provisioning is skipped entirely in `Process` tier — the browser tool requires container isolation.

### Entrypoint Script

The shell-vm container uses a launcher script (`docker/shell-vm-entrypoint.sh`) as its entrypoint instead of running `shell-daemon` directly:

```sh
#!/bin/sh
if [ "${BROWSER_ENABLED}" = "true" ]; then
    # Clean Chrome singleton locks from previous crashes
    find "${BRIDGE_STATE_DIR:-/shared/.pinchtab}/profiles" \
        -name "SingletonLock" -delete 2>/dev/null || true

    /usr/local/bin/pinchtab &
    PINCHTAB_PID=$!

    # Wait for Pinchtab to become healthy (up to 30s)
    HEALTH_URL="http://${BRIDGE_BIND:-127.0.0.1}:${BRIDGE_PORT:-9867}/health"
    RETRIES=0; MAX_RETRIES=30
    while [ $RETRIES -lt $MAX_RETRIES ]; do
        curl -sf -o /dev/null "$HEALTH_URL" 2>/dev/null && break
        kill -0 $PINCHTAB_PID 2>/dev/null || break  # exit early if Pinchtab crashed
        RETRIES=$((RETRIES + 1)); sleep 1
    done
fi
exec /usr/local/bin/shell-daemon "$@"
```

The script cleans up Chrome singleton lock files left by crashes in previous sessions, starts Pinchtab as a background process, waits for the `/health` endpoint to respond (up to 30 seconds), then execs `shell-daemon` — so `shell-daemon` becomes PID 1 and `shell_exec` remains fully functional even if browser fails to start.

### Port Allocation

Multiple users or restarts on the same host need distinct Pinchtab ports. The runner uses **probe-and-reserve** starting from `DEFAULT_PINCHTAB_PORT` (9867):

1. Try binding a TCP listener to the candidate port.
2. If in use, increment and retry (up to `PINCHTAB_PORT_RANGE` = 100 attempts).
3. Record the assigned port; use it for `BRIDGE_PORT` and `PINCHTAB_URL`.

This avoids collisions across concurrent users and restarts without any coordination service.

### Auth Token

The runner generates a 32-byte random hex `BRIDGE_TOKEN` at container provisioning time. It is:

1. Set as `BRIDGE_TOKEN` env var in `shell-vm` — Pinchtab enforces it on every request.
2. Set as `BRIDGE_TOKEN` env var in `oxydra-vm` — the LLM's `curl` commands reference `$BRIDGE_TOKEN`; the shell expands it at runtime. The actual token value never appears in the system prompt.

Since both containers share host networking and Pinchtab binds to `127.0.0.1`, the token is defense-in-depth — only local processes can reach Pinchtab regardless.

### Environment Variables

The runner forwards these variables into the shell-vm container when browser is enabled:

| Variable | Value | Purpose |
|---|---|---|
| `BROWSER_ENABLED` | `"true"` | Tells the entrypoint to start Pinchtab |
| `BRIDGE_PORT` | Allocated port (from 9867) | Pinchtab HTTP port |
| `BRIDGE_BIND` | `"127.0.0.1"` | Loopback-only bind |
| `BRIDGE_TOKEN` | Generated hex token | Auth enforcement |
| `BRIDGE_HEADLESS` | Config-derived | Default headless mode |
| `BRIDGE_STATE_DIR` | `"/shared/.pinchtab"` | Profile / state storage |
| `CHROME_BINARY` | `"/usr/bin/chromium"` | Chrome executable |
| `CHROME_FLAGS` | `"--no-sandbox"` | Required for containerized Chrome |

The runner also forwards `PINCHTAB_URL` (`http://127.0.0.1:<port>`) and `BRIDGE_TOKEN` into the **oxydra-vm** environment, so the skill's `{{PINCHTAB_URL}}` template placeholder is resolved and `$BRIDGE_TOKEN` is available in the shell.

### Health Check

The runner polls `GET /health` on the Pinchtab port after starting the container (30s timeout, 1s polling interval). On success, `browser_available = true` is set in `StartupStatusReport`. On timeout or crash, `browser_available = false` is set and a warning is logged — the shell sidecar remains fully functional. Critically, `PINCHTAB_URL` is only added to the oxydra-vm environment when the health check succeeds; this ensures the Browser Automation skill does not activate when Pinchtab is down.

### Shell Policy Overlay

The browser skill's `curl`-based workflow requires commands and features that are not in the default shell allowlist. When `BROWSER_ENABLED=true`, the runner automatically extends the `ShellConfig`:

```rust
// Applied in runner/src/lib.rs before shell-vm launch:
fn apply_browser_shell_overlay(config: &mut ShellConfig) {
    config.allow.get_or_insert_with(Vec::new)
        .extend(["curl", "jq", "sleep"].map(str::to_owned));
    config.allow_operators = Some(true);  // enables &&, |, $(), etc.
}
```

This adds `curl`, `jq`, `sleep`, and shell operators to the allowlist **without replacing** any user-specified shell config. Users who have custom shell allowlists retain full control; the overlay only adds.

### Docker Image Changes

Both `docker/Dockerfile` and `docker/Dockerfile.prebuilt` include:

```dockerfile
ARG PINCHTAB_VERSION=0.7.6

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl jq chromium \
    && rm -rf /var/lib/apt/lists/*

RUN ARCH=$(dpkg --print-architecture) && \
    curl -fSL "https://github.com/pinchtab/pinchtab/releases/download/v${PINCHTAB_VERSION}/pinchtab-linux-${ARCH}" \
      -o /usr/local/bin/pinchtab && \
    chmod +x /usr/local/bin/pinchtab

COPY docker/shell-vm-entrypoint.sh /usr/local/bin/entrypoint.sh
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
```

`dpkg --print-architecture` resolves to `amd64` or `arm64`, so the correct Pinchtab binary is fetched for the build target. `curl` and `jq` are installed at image build time (not just allowed by the shell policy) because the entrypoint health check script itself needs `curl`.

## Platform-Specific Backends

### Container and MicroVM Launch Mechanics

Both the Container and macOS MicroVM tiers use the same `launch_docker_container_async()` path. For each container the runner:

1. **Removes** any existing container with the same name (force-remove)
2. **Pulls** the image if it isn't present locally
3. **Computes entrypoint and cmd** based on the guest role:
   - `OxydraVm` → `ENTRYPOINT ["/usr/local/bin/oxydra-vm"]` with `CMD ["--user-id", "<id>", "--workspace-root", "<path>", "--bootstrap-file", "/run/oxydra/bootstrap"]`
   - `ShellVm` → `ENTRYPOINT ["/usr/local/bin/shell-daemon"]` with `CMD ["--socket", "/ipc/shell-daemon.sock"]`
4. **Creates** the container with:
   - Host networking (`network_mode: "host"`) — the gateway binds `127.0.0.1:0` inside the container, reachable from the host; the shell-daemon socket is in a bind-mounted directory
   - Bind mounts — `OxydraVm` uses host paths directly; `ShellVm` uses virtual container paths (`/shared`, `/tmp`, `/ipc`) to prevent host path leakage
   - Bootstrap file bind-mounted read-only at `/run/oxydra/bootstrap` (OxydraVm only)
   - Resource limits (CPU, memory, PID)
   - Proxy environment variables for sandboxd-managed VMs
   - Environment variables (see [Environment Variable Forwarding](#environment-variable-forwarding) below)
5. **Starts** the container

Container names follow the pattern `oxydra-{tier}-{user}-{role}` (e.g. `oxydra-container-alice-oxydra-vm`).

### Guest Docker Images

Guest images can be built from `docker/Dockerfile` (full in-Docker build) or from `docker/Dockerfile.prebuilt` after local cross-compilation.

```bash
# Local cross-compilation (recommended)
./scripts/build-guest-images.sh arm64

# Full in-Docker build
./scripts/build-guest-images-in-docker.sh
```

The in-Docker build compiles both `oxydra-vm` and `shell-daemon` binaries in a shared Rust builder. The prebuilt path packages locally compiled binaries into `debian:trixie-slim` images.

Image names are configured in `runner.toml` under `[guest_images]`:
```toml
[guest_images]
oxydra_vm = "oxydra-vm:latest"
shell_vm  = "shell-vm:latest"
```

### Linux (Container Tier)

Uses `bollard` (Docker API) to connect to the local Docker daemon and launch container pairs directly.

### macOS (MicroVM Tier)

Interacts with `docker-sandboxd` via a Unix socket to provision Docker-managed VMs, which host the guest containers. This adds an extra layer of indirection on macOS where native VM support differs from Linux. Guest images are transferred into the sandboxd VM via `docker save | docker --host unix://vm-socket load`.

### Environment Variable Forwarding

The runner collects environment variables from two sources and injects them into the **`oxydra-vm` container only**:

1. **Config-referenced keys** — The runner scans the agent config for `api_key_env` fields in provider registry entries and `api_key_env`/`engine_id_env` in `[tools.web_search]`. For each referenced env var name that is set in the runner's own environment, a `KEY=VALUE` pair is forwarded. This is how provider API keys (e.g. `OPENAI_API_KEY`, `GEMINI_API_KEY`) reach the guest.

2. **CLI-supplied env vars** — The `--env KEY=VALUE` (`-e`) and `--env-file <PATH>` flags inject additional variables. CLI values override file values on conflict.

The **`shell-vm` container does not receive API keys or config-referenced env vars** to prevent credential exposure in shell output. Shell-specific env vars can be forwarded via the `[shell.env_keys]` config section or by using the `SHELL_` prefix convention with `--env`/`--env-file` (see Chapter 2 for details).

Additionally, sandboxd-managed VMs (macOS MicroVM tier) receive HTTP/HTTPS proxy environment variables to route traffic through the sandbox daemon's proxy.

## Startup Status Reporting

The guest reports its capability status at startup:

```rust
pub struct StartupStatusReport {
    pub sidecar_available: bool,
    pub shell_available: bool,
    pub browser_available: bool,
    pub degraded_reasons: Vec<StartupDegradedReason>,
}

pub enum StartupDegradedReason {
    InsecureProcessTier,
    SidecarUnavailable,
    SidecarConnectionFailed(String),
    ShellDaemonUnresponsive,
    BrowserPoolUnavailable,
}
```

This report determines which tools are registered in the `ToolRegistry`. If the sidecar is unavailable, shell and browser tools are disabled rather than failing silently at execution time.

## Gateway Endpoint Discovery

The gateway endpoint marker file (`tmp/gateway-endpoint`) enables discovery by external clients:

1. `oxydra-vm` writes the file after binding the WebSocket server
2. The runner reads it in `--tui` mode to find the connection endpoint
3. Contents: a single line with the WebSocket URL (e.g., `ws://127.0.0.1:5678/ws`)

This file-based discovery avoids hardcoded ports and supports multiple concurrent guests on different ports.

## Retrieving Logs

The `runner logs` command retrieves runtime and sidecar logs from the runner daemon without requiring Docker-specific knowledge or direct file access.

### Usage

```bash
runner --user alice logs                          # runtime stdout+stderr, last 200 lines, text
runner --user alice logs --role sidecar           # sidecar (shell-vm) logs
runner --user alice logs --role all               # both runtime and sidecar
runner --user alice logs --stream stderr          # stderr only
runner --user alice logs --tail 50                # last 50 lines
runner --user alice logs --since 15m              # lines from the last 15 minutes
runner --user alice logs --format json            # JSON output (one entry per line)
runner --user alice logs --since 2026-03-01T10:00:00Z --format json --tail 100
runner --user alice logs --follow                # continuously poll for new log lines
runner --user alice logs -f --role all           # follow all roles (short flag)
```

### Flags

| Flag         | Values                        | Default     | Description                                                        |
|-------------|-------------------------------|-------------|--------------------------------------------------------------------|
| `--role`    | `runtime`, `sidecar`, `all`   | `runtime`   | Which guest role to retrieve logs for                              |
| `--stream`  | `stdout`, `stderr`, `both`    | `both`      | Which output stream to show                                        |
| `--tail`    | 1–1000                        | `200`       | Number of log lines to retrieve (clamped to 1000)                  |
| `--since`   | RFC 3339 or duration (e.g. `15m`, `1h`, `30s`) | — | Only show logs newer than this time |
| `--format`  | `text`, `json`                | `text`      | Output format                                                      |
| `--follow`, `-f` | —                        | off         | Continuously poll for new log lines (Ctrl+C to stop)               |

### Output Format

**Text** (default):
```
2026-03-01T10:55:12Z [docker_api][oxydra-vm][stderr] gateway bind failed: port in use
```

**JSON** (one object per line):
```json
{"timestamp":"2026-03-01T10:55:12Z","source":"docker_api","role":"oxydra-vm","stream":"stderr","message":"gateway bind failed: port in use"}
```

Each entry includes:
- `timestamp` — RFC 3339 timestamp (if available from the log source)
- `source` — `process_file` (process-tier file logs) or `docker_api` (Docker container logs)
- `role` — `oxydra-vm` (runtime) or `shell-vm` (sidecar)
- `stream` — `stdout` or `stderr`
- `message` — the log line content

### Log Sources

| Sandbox Tier | Source     | Mechanism                                                           |
|-------------|-----------|---------------------------------------------------------------------|
| Process     | `process_file` | stdout/stderr pipes pumped to `{role}.{stream}.log` files      |
| Container   | `docker_api`   | bollard log stream with timestamps, written to the same files  |
| MicroVM     | `docker_api`   | macOS: same as container; Linux: process-tier log pump          |

All tiers write logs to the workspace `logs/` directory. The `runner logs` command reads from these files, normalizes entries, and returns a bounded response (max 128KB frame).

### Truncation

When log output exceeds the requested `--tail` limit or the 128KB frame size, the response is truncated and a notice is printed to stderr: `(output truncated; use --tail to adjust)`. The `truncated` field in JSON output is set to `true`.

### Follow Mode

When `--follow` (or `-f`) is passed, the command prints an initial snapshot then polls the daemon every 2 seconds for new entries. Duplicate lines are suppressed using content-based deduplication. The loop exits on Ctrl+C or when the daemon is no longer reachable.

### Warnings

When a requested role has no running guest (e.g. `--role sidecar` with no shell-vm), a warning is printed to stderr. The command succeeds with an empty entry list rather than failing.
