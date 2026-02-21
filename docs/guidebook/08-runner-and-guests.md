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
│    ├── shared/   (persistent data)                   │
│    ├── tmp/      (temporary + gateway-endpoint)      │
│    └── vault/    (sensitive credentials)             │
└─────────────────────────────────────────────────────┘
```

## Runner Startup Flow

When you run `oxydra-runner`:

1. **CLI parsing** — reads `--config`, `--user`, `--insecure`, `--tui` flags
2. **Global config loading** — reads `runner.toml` (workspace root, user registrations, default tier, guest images)
3. **User resolution** — if `--user` is omitted and exactly one user is configured, auto-selects; otherwise requires explicit `--user`
4. **Workspace provisioning** — creates `<workspace_root>/<user_id>/{shared,tmp,vault}` if they don't exist
5. **Backend selection** — based on `SandboxTier` (or `Process` if `--insecure`):
   - `MicroVm` → provisions via platform-specific VM backend
   - `Container` → provisions via Docker/OCI API (`bollard`)
   - `Process` → spawns as local child process
6. **Guest launch** — starts `oxydra-vm` binary with bootstrap data

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

- **Process tier:** Written to the guest's `stdin` as a 4-byte length prefix (big-endian u32) followed by JSON bytes
- **Container/MicroVM tier:** Similar mechanism via container stdin or socket

### Consumption

The `oxydra-vm` binary reads the envelope from stdin (when `--bootstrap-stdin` is set) and passes it to `tui::bootstrap_vm_runtime`:

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

The `oxydra-vm` binary is the guest-side entry point:

```
1. Initialize tracing subscriber
2. Read bootstrap envelope from stdin
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

**File:** `shell-daemon/src/lib.rs`

The shell daemon runs inside `shell-vm` and provides an RPC interface for shell command and browser session execution.

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

### Linux (Container Tier)

Uses `bollard` (Docker API) to:
- Pull/build guest images
- Create containers with workspace bind mounts
- Configure networking and resource limits
- Start container pairs (oxydra-vm + shell-vm)

### macOS (Container Tier)

Interacts with `docker-sandboxd` via a Unix socket to provision Docker-managed VMs, which host the guest containers. This adds an extra layer of indirection on macOS where native VM support differs from Linux.

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
