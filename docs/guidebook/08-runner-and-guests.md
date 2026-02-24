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

### Session Management

The `SessionManager` maintains a map of active shell sessions. Each session is an isolated shell process with its own working directory, environment, and lifecycle. Sessions can be spawned, used for multiple commands, streamed, and killed independently.

### Shell Session Backends

Two backends implement the `ShellSession` trait:

| Backend | Use Case | Mechanism |
|---------|----------|-----------|
| `VsockShellSession` | Production (Container/MicroVM) | Connects to shell-daemon over vsock/Unix socket |
| `LocalProcessShellSession` | Development/Testing | Spawns host processes directly |

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
