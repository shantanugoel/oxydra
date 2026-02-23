# TUI / Runner / oxydra-vm UX Issues — Root Cause Analysis & Fix Plan

## Table of Contents

1. [Issue 1: Socket Files Exposed in User Workspace `tmp/`](#issue-1-socket-files-exposed-in-user-workspace-tmp)
2. [Issue 2: TUI Does Not Support Mouse Selection / Copy](#issue-2-tui-does-not-support-mouse-selection--copy)
3. [Issue 3: TUI Becomes Unresponsive on Connection Loss (No Ctrl+C)](#issue-3-tui-becomes-unresponsive-on-connection-loss-no-ctrlc)
4. [Issue 4: Gateway WebSocket Port Discovery Is Manual and Fragile](#issue-4-gateway-websocket-port-discovery-is-manual-and-fragile)
5. [Issue 5: Insufficient Logging Across the Stack](#issue-5-insufficient-logging-across-the-stack)
6. [Issue 6: Additional UX Issues Found During Review](#issue-6-additional-ux-issues-found-during-review)

---

## Issue 1: Socket Files Exposed in User Workspace `tmp/`

### Problem

Several socket and control files are placed inside `<workspace>/<user_id>/tmp/`:

| File | Created By | Purpose |
|------|-----------|---------|
| `runner-control.sock` | Runner (`main.rs:138`) | Daemon control socket |
| `shell-daemon.sock` | Shell Daemon / Runner (`lib.rs:1149-1156`) | Sidecar RPC |
| `gateway-endpoint` | oxydra-vm (`oxydra-vm.rs:149`) | Contains `ws://...` URL |
| `bootstrap/runner_bootstrap.json` | Runner (`lib.rs:1790-1817`) | Bootstrap envelope (contains `sidecar_endpoint`, `workspace_root`, etc.) |
| `firecracker-oxydra-vm.json` | Runner backend (`backend.rs:44`) | Firecracker config |
| `oxydra-vm-firecracker.sock` | Runner backend (`backend.rs:51`) | Firecracker API socket |

The `tmp/` directory is bind-mounted into the shell-vm container so `oxydra-vm` and `shell-vm` can communicate. This means the **shell** (and therefore the **LLM** via shell tool execution) has read access to all of these files. A prompt injection attack could:

1. Read `gateway-endpoint` → learn the WebSocket URL
2. Connect to the gateway → impersonate a client and issue turns
3. Read `runner_bootstrap.json` → learn `sidecar_endpoint`, `workspace_root`, `sandbox_tier`
4. Connect to `runner-control.sock` → send `ShutdownUser` to kill the runtime
5. Connect to `shell-daemon.sock` → directly execute shell commands bypassing runtime guardrails

### Root Cause

The `tmp/` directory serves dual purposes:
- **IPC bus** for inter-component communication (sockets, marker files)
- **Scratch space** for the shell session's temporary files

Both roles share the same bind-mount because it's the simplest way to make the shell-daemon socket reachable from both the `oxydra-vm` and `shell-vm` containers.

### Fix Plan

**Approach A: Separate IPC directory (Recommended)**

Add a new `ipc/` directory to the workspace that is:
- Mounted into `oxydra-vm` and `shell-vm` containers for inter-process communication
- **NOT** mounted into the shell session's working environment
- Not part of the security policy's allowed roots (`shared`, `tmp`, `vault`)

Steps:
1. Add `ipc: PathBuf` to `UserWorkspace` struct in `crates/runner/src/lib.rs`
2. Create `<workspace>/<user_id>/ipc/` during `provision_user_workspace()`
3. Move the following files from `tmp/` to `ipc/`:
   - `runner-control.sock`
   - `shell-daemon.sock` / `shell-daemon-vsock.sock`
   - `gateway-endpoint`
   - `bootstrap/runner_bootstrap.json`
   - Firecracker config/socket files
4. Update all references:
   - `main.rs:138` — `runner-control.sock` path
   - `lib.rs:1133-1158` — `pre_compute_sidecar_endpoint()` references
   - `lib.rs:1790-1817` — `write_bootstrap_file()` path
   - `oxydra-vm.rs:165` — `write_gateway_endpoint_marker()` path
   - `lib.rs:250` — `connect_tui()` endpoint path lookup
   - `backend.rs:44,51,108,135` — Firecracker config/socket paths
   - Container entrypoint CMD for shell-daemon `--socket` path
5. Update Docker bind-mount configuration:
   - `ipc/` gets its own bind-mount (accessible to both containers)
   - `tmp/` is only mounted for scratch use in `shell-vm`
6. Update the security policy in `sandbox/src/policy.rs` — `ipc/` should NOT be in the allowed roots for WASM tool runner, ensuring file_read/file_list tools cannot access it
7. Clean up `gateway-endpoint` marker on shutdown (currently not cleaned up)

**Approach B: Unix socket placed in a host-only path (Alternative)**

Place sockets in a path *outside* the workspace entirely (e.g., `$XDG_RUNTIME_DIR/oxydra/<user_id>/`). This fully isolates them but:
- Requires the runner and oxydra-vm to agree on the path without workspace-based discovery
- Doesn't work for Container/MicroVM tiers where the guest can't reach host-only paths
- Complicates multi-user isolation

**Recommendation:** Approach A is the right trade-off. It keeps file-based discovery working, requires no special host paths, and cleanly separates IPC from user-accessible scratch space.

---

## Issue 2: TUI Does Not Support Mouse Selection / Copy

### Problem

The TUI enables mouse capture (`EnableMouseCapture` in `app.rs:109`) which takes over the terminal's mouse handling. This prevents the user from using native terminal mouse selection and copy.

### Root Cause

In `crates/tui/src/app.rs`, `TerminalGuard::setup()` calls:
```rust
execute!(stdout, EnterAlternateScreen, EnableMouseCapture, cursor::Hide)
```

Mouse capture is enabled globally, but `event_loop.rs:135-141` discards all mouse events:
```rust
fn map_event(event: Event) -> Option<AppAction> {
    match event {
        Event::Key(key) => map_key_event(key),
        Event::Resize(cols, rows) => Some(AppAction::Resize(cols, rows)),
        // Mouse scroll could be mapped here in the future.
        _ => None,  // ← all mouse events are silently dropped
    }
}
```

So mouse capture is enabled but serves no purpose — it only prevents native terminal copy/paste.

### Fix Plan

**Step 1: Remove mouse capture entirely (simplest fix)**

In `app.rs`, remove `EnableMouseCapture` from `TerminalGuard::setup()` and `DisableMouseCapture` from `restore_terminal()`. This immediately restores native terminal mouse selection and copy behavior.

```rust
// Before:
execute!(stdout, EnterAlternateScreen, EnableMouseCapture, cursor::Hide)
// After:
execute!(stdout, EnterAlternateScreen, cursor::Hide)
```

```rust
// Before:
execute!(stdout, LeaveAlternateScreen, DisableMouseCapture, cursor::Show)
// After:
execute!(stdout, LeaveAlternateScreen, cursor::Show)
```

This is the correct approach because:
- The TUI doesn't use mouse events today
- Arrow keys already handle scrolling
- Users get native copy/paste for free

**Step 2: (Future, optional) Add mouse scroll support without capture**

If mouse scroll is desired later, crossterm supports `EnableMouseCapture` with specific modes. But the better path is to NOT capture mouse and let the terminal handle it. Scrolling via keyboard (Up/Down/PageUp/PageDown) already works.

---

## Issue 3: TUI Becomes Unresponsive on Connection Loss (No Ctrl+C)

### Problem

When the WebSocket connection to the gateway breaks, the TUI becomes completely unresponsive — even Ctrl+C doesn't work, forcing the user to kill the process externally.

### Root Cause

Looking at the main loop in `app.rs:345-468`, when the gateway reader channel returns `None` (connection lost), the code path is:

```rust
None => {
    // ... mark disconnected, draw ...
    reader_handle.abort();
    writer_handle.abort();

    // Reconnect loop — THIS BLOCKS THE MAIN SELECT! LOOP
    let (new_gw_rx, new_ws_tx, new_rh, new_wh) =
        self.reconnect_loop(&mut terminal).await?;
    // ...
}
```

The `reconnect_loop()` (`app.rs:557-601`) is an infinite loop with backoff:
```rust
loop {
    // ... compute delay ...
    time::sleep(delay).await;  // ← BLOCKS HERE
    match self.try_reconnect().await {
        Ok(result) => return Ok(result),
        Err(_) => { /* loop again */ }
    }
}
```

While `reconnect_loop()` is executing, the `tokio::select!` main loop is **not running**. This means:
- The `event_reader.receiver_mut().recv()` arm is not polled
- User input (including Ctrl+C) is buffered in the mpsc channel but never processed
- The TUI is effectively frozen until reconnection succeeds

### Fix Plan

**Step 1: Integrate reconnection into the main `select!` loop**

Instead of calling `reconnect_loop()` as a blocking subroutine, track reconnection state within the main `select!` loop so user input continues to be processed.

Introduce a reconnection state enum:
```rust
enum WsConnectionState {
    Connected {
        gateway_rx: mpsc::Receiver<GatewayServerFrame>,
        ws_tx: mpsc::Sender<GatewayClientFrame>,
        reader_handle: JoinHandle<()>,
        writer_handle: JoinHandle<()>,
    },
    Reconnecting {
        attempt: u32,
        next_retry: Instant,
    },
}
```

In the main `select!` loop, add a branch for reconnection that is a `tokio::time::sleep_until(next_retry)`. When the sleep completes, attempt one reconnection. If it fails, bump `attempt` and compute a new `next_retry`. Meanwhile, the user input arm stays active — Ctrl+C and Quit are always processed.

**Step 2: Always handle Cancel/Quit actions regardless of connection state**

In `handle_action()`, the `Cancel` and `Quit` actions already work correctly (they don't depend on `ws_tx`). The issue is purely that `handle_action()` is never called during the blocking `reconnect_loop()`. Fixing Step 1 resolves this.

**Step 3: Add a "force quit" safety net**

As a belt-and-suspenders approach, track consecutive Ctrl+C presses even if the adapter says CancelActiveTurn. If 2-3 consecutive Ctrl+C presses happen within a short window (e.g., 2 seconds), force exit regardless of turn state. This handles edge cases where the adapter incorrectly believes a turn is active.

---

## Issue 4: Gateway WebSocket Port Discovery Is Manual and Fragile

### Problem

The `oxydra-vm` gateway binds to `127.0.0.1:0` (random port). The bound port is:
- Printed to **stderr** via `eprintln!` in `oxydra-vm.rs:150-154`
- Written to `<workspace>/tmp/gateway-endpoint` marker file

When launching the TUI:
- In `--tui` mode with the runner, the runner reads the marker file — this works
- When launching `oxydra-tui` directly, the user must manually find the port by:
  1. Checking oxydra-vm's stderr output
  2. Or finding the marker file in the workspace
  3. Then passing `--gateway-endpoint ws://127.0.0.1:<port>/ws`

The stderr output is hard to reach because:
- In Process tier: stdout/stderr are piped to log files at `<workspace>/logs/oxydra-vm.stderr.log` (`lib.rs:1752-1768`)
- In Container tier: stderr is inside the container and requires `docker logs`
- The log pump writes line-by-line, so the endpoint may be buried among other output

### Root Cause

1. The gateway endpoint is printed to stderr (intended for the VM's own logging), not surfaced to the runner's stdout
2. The runner (`main.rs:120-126`) prints startup metadata to stdout but does NOT print the gateway endpoint — it's only available if you use `--tui --probe` mode, and even then it requires the marker file to exist first
3. Direct `oxydra-tui` invocation lacks auto-discovery — the TODO at `oxydra-tui.rs:82` says "runner-based discovery is not yet wired"

### Fix Plan

**Step 1: Print gateway endpoint in runner stdout (quick win)**

After `start_user()` succeeds, the runner should wait for the `gateway-endpoint` marker file to appear (with a timeout), read it, and print it to stdout:

```rust
// In main.rs, after the startup metadata block:
println!("gateway_endpoint={}", wait_for_gateway_endpoint(&workspace.tmp)?);
```

This gives the user (or a wrapper script) the endpoint without needing to dig through logs.

Implementation:
- Add a polling function `wait_for_gateway_endpoint(tmp_dir: &Path, timeout: Duration) -> Result<String, RunnerError>` that polls for the marker file with a configurable timeout (e.g., 30s)
- Call it in `main.rs` after `start_user()` succeeds, both in daemon and non-daemon modes
- Print as `gateway_endpoint=ws://127.0.0.1:XXXXX/ws` for easy parsing

**Step 2: Wire auto-discovery in `oxydra-tui` (medium effort)**

The `oxydra-tui` binary already has a `resolve_gateway_endpoint()` function with a TODO for runner-based discovery. Wire it:

Option A — **Marker file discovery** (simpler):
- Accept `--workspace-root` (or `--config` + `--user`) arguments
- Read `<workspace>/<user>/tmp/gateway-endpoint` (or `<workspace>/<user>/ipc/gateway-endpoint` after Issue 1 fix)
- This avoids needing to connect to the runner daemon at all

Option B — **Runner daemon discovery** (more robust):
- Connect to `runner-control.sock` and send a health-check
- Parse the gateway endpoint from the response
- Requires the runner daemon to be running and the socket to be reachable

**Recommendation:** Implement Option A first (it's simpler and works without the daemon), then consider Option B later.

Steps for Option A:
1. Add `--config` and `--user` CLI args to `oxydra-tui.rs` (mirroring runner's args)
2. In `resolve_gateway_endpoint()`, when `--gateway-endpoint` is not provided:
   a. Load `runner.toml` from `--config` path
   b. Resolve `workspace_root` from config
   c. Resolve user (same logic as runner's `resolve_user_id()`)
   d. Read `<workspace_root>/<user>/tmp/gateway-endpoint` (or `ipc/` after Issue 1)
   e. Return the endpoint URL

**Step 3: Log gateway endpoint at INFO level in oxydra-vm (visibility)**

Change the `eprintln!` in `oxydra-vm.rs:150-154` to `tracing::info!`:
```rust
info!(
    user_id = %args.user_id,
    gateway_endpoint = %gateway_endpoint(address),
    "oxydra-vm gateway listening"
);
```

Since tracing writes to stderr anyway (via `init_tracing()`), the behavior is the same, but it gains structured formatting and respects log level filters.

---

## Issue 5: Insufficient Logging Across the Stack

### Problem

Debugging runtime behavior is difficult because logging is minimal at info and debug levels. Key events that would help operators and developers understand what's happening are either not logged or only logged at trace level.

### Current State

The `init_tracing()` function in `crates/types/src/tracing.rs` initializes a basic stderr subscriber with no level filtering — it defaults to `INFO` unless `RUST_LOG` is set. Current logging:

| Component | What's logged | Level | What's missing |
|-----------|--------------|-------|----------------|
| Runner | Startup metadata, warnings | INFO/WARN | Gateway endpoint, user config resolution, workspace provisioning steps |
| oxydra-vm | Startup status (degraded/ready) | INFO/WARN | Gateway bind address (only `eprintln`), config loading steps |
| Runtime | Turn state transitions | DEBUG | Successful tool calls (name + duration), provider model used, token counts per turn |
| Gateway | WebSocket errors, lagged subscribers | DEBUG | Session creation/resumption, turn lifecycle (start/complete/cancel), health check results |
| Tool execution | Failed tool calls (self-correction) | WARN | Successful tool calls, tool arguments summary, execution timing |

### Fix Plan

**Step 1: Add `RUST_LOG` / `--log-level` support**

Currently `init_tracing()` doesn't configure an `EnvFilter`. Add `tracing-subscriber`'s `EnvFilter` so users can set `RUST_LOG=debug` or `RUST_LOG=oxydra=debug`:

```rust
pub fn init_tracing() {
    TRACING_INIT.call_once(|| {
        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
        let _ = tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_target(false)
            .with_ansi(false)
            .with_env_filter(filter)
            .try_init();
    });
}
```

Check if `tracing-subscriber` with the `env-filter` feature is already a dependency; add it if not.

**Step 2: Add INFO-level logging for key lifecycle events**

These should be visible by default (INFO level):

| Location | Log message |
|----------|-------------|
| `runner/src/main.rs` — after start_user | `info!(gateway_endpoint, "runner started guest")` |
| `runner/src/main.rs` — daemon mode | `info!(socket_path, "runner control socket listening")` |
| `oxydra-vm.rs` — after gateway bind | `info!(address, user_id, "gateway WebSocket listening")` (replace `eprintln!`) |
| `oxydra-vm.rs` — config loaded | `info!(provider, model, "agent config loaded")` |
| `runtime/src/lib.rs` — each provider call | `info!(provider, model, turn, max_turns, "calling provider")` |
| `gateway/src/lib.rs` — session created | `info!(user_id, runtime_session_id, "gateway session created")` |
| `gateway/src/lib.rs` — turn started | `info!(turn_id, runtime_session_id, "turn started")` |
| `gateway/src/lib.rs` — turn completed | `info!(turn_id, "turn completed")` |
| `gateway/src/lib.rs` — turn cancelled | `info!(turn_id, "turn cancelled")` |

**Step 3: Add DEBUG-level logging for operational details**

These are visible with `RUST_LOG=debug`:

| Location | Log message |
|----------|-------------|
| `runtime/src/tool_execution.rs` — after successful execute | `debug!(tool_name, duration_ms, "tool call succeeded")` |
| `runtime/src/tool_execution.rs` — before execute | `debug!(tool_name, "executing tool call")` |
| `runtime/src/lib.rs` — provider response | `debug!(turn, input_tokens, output_tokens, cost, "provider response received")` |
| `runtime/src/lib.rs` — tool batch | `debug!(batch_size, "executing read-only tool batch")` |
| `runtime/src/budget.rs` — context budget | `debug!(utilization_pct, token_count, limit, "context budget check")` |
| `gateway/src/lib.rs` — health check | `debug!(healthy, session_id, "health check responded")` |
| `gateway/src/lib.rs` — reconnection | `debug!(connection_id, user_id, "client reconnected to existing session")` |
| `runner/src/bootstrap.rs` — config resolution | `debug!(path, "loading config file")` |
| `runner/src/bootstrap.rs` — provider built | `debug!(provider_id, model_id, "provider initialized")` |

**Step 4: Surface provider/model in logs and TUI status bar**

The active provider and model are important context for operators and users. They should appear in:

- **Logs (INFO level):** Every provider call should log which provider and model is being used. This is already partially covered in Step 2 (`oxydra-vm.rs` config loaded, `runtime/src/lib.rs` provider call), but should also appear in the gateway's turn-started log entry:
  ```
  | `gateway/src/lib.rs` — turn started | `info!(turn_id, provider, model, "turn started")` |
  ```

- **TUI status bar:** The `StatusBar` widget already has a `model_name: Option<&str>` field and renders `" | model: {name}"` when present (`widgets.rs:307-309`). It's currently always passed `None` (`widgets.rs:390`). To wire it up:

  1. Add `provider_id` and `model_id` fields to `GatewayHelloAck` in `types/src/channel.rs`:
     ```rust
     pub struct GatewayHelloAck {
         // ... existing fields ...
         #[serde(default, skip_serializing_if = "Option::is_none")]
         pub provider_id: Option<String>,
         #[serde(default, skip_serializing_if = "Option::is_none")]
         pub model_id: Option<String>,
     }
     ```
     Using `Option` and `skip_serializing_if` keeps backward compatibility — old clients ignore unknown fields, old servers send `null`.

  2. Populate them in the gateway when constructing `HelloAck` (`gateway/src/lib.rs`). The `GatewayServer` already holds provider/model via its `turn_runner` (`RuntimeGatewayTurnRunner` has `provider: ProviderId` and `model: ModelId`). Expose a method on the `GatewayTurnRunner` trait (or store them directly on `GatewayServer`) and include them in the `HelloAck` response.

  3. Store them in the TUI adapter state — add `model_id: Option<String>` to `TuiUiState` in `tui/src/channel_adapter.rs`, populated from `HelloAck`.

  4. Pass to `StatusBar` in `render_app()` — change `None` on line 390 to `adapter_state.model_id.as_deref()`.

  The TUI status bar will then show e.g.: `o connected | session: rt-alice | model: gpt-4o | idle [Ctrl+C to exit]`

  This approach also benefits from the `HealthStatus` frame — if `GatewayHealthStatus` similarly gains optional `provider_id`/`model_id` fields, the TUI can update the display if the model ever changes mid-session (e.g., model hot-swap in future multi-agent scenarios).

**Step 5: Ensure no secrets in logs**

Review all new log points to ensure:
- API keys are never logged (even at TRACE level)
- `bootstrap_envelope` logging doesn't include `sidecar_endpoint` details at INFO
- Tool arguments are summarized, not dumped in full (truncate to first 200 chars at DEBUG)

---

## Issue 6: Additional UX Issues Found During Review

### 6a. No Multi-Line Input Support

**Problem:** The TUI maps `Enter` directly to `Submit` (`event_loop.rs:163`). There is no way to enter multi-line prompts, which are common for code-related queries.

**Fix:** Map `Shift+Enter` (or `Alt+Enter`) to insert a newline character, and keep `Enter` as submit. crossterm reports `KeyModifiers::SHIFT` which can be checked:

```rust
(KeyCode::Enter, false) => Some(AppAction::Submit),
(KeyCode::Enter, true) if key.modifiers.contains(KeyModifiers::SHIFT) => Some(AppAction::Char('\n')),
```

Note: `Shift+Enter` detection depends on terminal emulator support. Some terminals don't distinguish `Enter` from `Shift+Enter`. An alternative is `Alt+Enter`:
```rust
let alt = key.modifiers.contains(KeyModifiers::ALT);
// ...
(KeyCode::Enter, _) if alt => Some(AppAction::Char('\n')),
```

The InputBar widget (`widgets.rs`) already uses `Paragraph` which renders newlines, so the rendering side should work. The input area height (currently 3 rows, `widgets.rs` layout) may need to grow dynamically for multi-line content.

### 6b. No Runtime Activity Visibility for Channels

**Problem:** When a turn involves multi-step internal work — tool calls, multiple provider round-trips, rolling summaries — the user sees nothing but the spinner and "Waiting...". There's no indication of what's happening or why it's taking time. This affects all channels (TUI, future API clients, etc.), not just the TUI.

**Root Cause — full data flow analysis:**

The runtime's `run_session_internal()` (`runtime/src/lib.rs:198-335`) loops internally: `provider call → tool execution → provider call → ...`. Each iteration is an internal "turn" (up to `max_turns`). From the gateway's perspective, this entire multi-turn loop is a single `run_turn()` call that blocks until the final response.

The existing streaming pipeline:

```
Runtime (run_session_internal)
  │ sends StreamItem via stream_events channel
  ▼
Gateway turn_runner (run_turn)
  │ ONLY forwards StreamItem::Text as String via delta_sender
  │ DROPS: ToolCallDelta, ReasoningDelta, UsageUpdate, FinishReason
  ▼
Gateway (start_turn)
  │ wraps String as GatewayServerFrame::AssistantDelta
  ▼
Channel (TUI / future clients)
```

Three gaps exist:

1. **Runtime doesn't emit progress events.** There's no `StreamItem` variant for "I'm executing tool X" or "I'm on internal turn 3/8" or "triggering rolling summary". These transitions exist in the code (`tracing::debug!` at lines 227, 259, 269) but only go to the log, not to `stream_events`.

2. **Gateway turn_runner filters out non-text items.** In `turn_runner.rs:86-89`, only `StreamItem::Text` is forwarded:
   ```rust
   if let Some(StreamItem::Text(delta)) = maybe_event {
       let _ = delta_sender.send(delta);
   }
   ```

3. **Gateway protocol lacks a progress frame type.** `GatewayServerFrame` has no variant for status/progress updates during a turn.

**Assessment — Is this worth doing?**

Yes, and it's cleaner than it looks. The key insight is that `StreamItem` already exists as the internal event bus. Adding new variants to `StreamItem` is the right internal-level change — it naturally flows to all channels without channel-specific code.

The changes are localized to 4 files and don't require restructuring:
- `types/src/model.rs` — add `StreamItem` variants (the internal event type)
- `runtime/src/lib.rs` — emit progress events at existing transition points
- `types/src/channel.rs` — add `GatewayServerFrame` variant (the protocol type)
- `gateway/src/turn_runner.rs` — forward progress items (1 match arm)
- `gateway/src/lib.rs` — publish progress frames (1 match arm)

Each channel then independently decides how to render progress (TUI shows system messages, API clients can use it for status polling, etc.).

**Fix Plan:**

**Step 1: Add progress variants to `StreamItem` (types crate)**

In `crates/types/src/model.rs`, add a single new variant:

```rust
pub enum StreamItem {
    Text(String),
    ToolCallDelta(ToolCallDelta),
    ReasoningDelta(String),
    UsageUpdate(UsageUpdate),
    ConnectionLost(String),
    FinishReason(String),
    Progress(RuntimeProgressEvent),  // ← new
}

/// A progress event emitted by the runtime during multi-step execution.
/// Channels can surface these to users however they like (status line,
/// system message, log entry, etc.).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeProgressEvent {
    /// What's happening right now.
    pub kind: RuntimeProgressKind,
    /// Human-readable summary (e.g. "Executing file_read").
    pub message: String,
    /// Current internal turn (1-indexed).
    pub turn: usize,
    /// Maximum turns allowed.
    pub max_turns: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuntimeProgressKind {
    /// About to call the LLM provider.
    ProviderCall,
    /// Executing one or more tool calls.
    ToolExecution { tool_names: Vec<String> },
    /// A specific tool call completed.
    ToolCompleted { tool_name: String, success: bool },
    /// Triggering context rolling summary.
    RollingSummary,
}
```

This is deliberately a flat, simple enum. No nesting, no IDs to track — just "here's what's happening now."

**Step 2: Emit progress events from the runtime**

In `crates/runtime/src/lib.rs`, emit `StreamItem::Progress` at the existing transition points inside `run_session_internal()`. These are places where `tracing::debug!` already logs the transitions:

```rust
// Before provider call (line ~227):
if let Some(ref stream_events) = stream_events {
    let _ = stream_events.send(StreamItem::Progress(RuntimeProgressEvent {
        kind: RuntimeProgressKind::ProviderCall,
        message: format!("Calling provider (turn {turn}/{max})", max = self.limits.max_turns),
        turn,
        max_turns: self.limits.max_turns,
    }));
}

// Before tool execution (line ~268):
let tool_names: Vec<String> = tool_calls.iter().map(|tc| tc.name.clone()).collect();
if let Some(ref stream_events) = stream_events {
    let _ = stream_events.send(StreamItem::Progress(RuntimeProgressEvent {
        kind: RuntimeProgressKind::ToolExecution { tool_names: tool_names.clone() },
        message: format!("Executing tools: {}", tool_names.join(", ")),
        turn,
        max_turns: self.limits.max_turns,
    }));
}
```

In `crates/runtime/src/tool_execution.rs`, emit `ToolCompleted` after each tool finishes:

```rust
// After successful or failed tool execution (in execute_tool_and_format):
// Note: stream_events would need to be passed down, or ToolCompleted
// events could be emitted at the call site in lib.rs instead.
```

For rolling summary, in the `maybe_trigger_rolling_summary` call site.

**Step 3: Add progress frame to the gateway protocol**

In `crates/types/src/channel.rs`:

```rust
pub enum GatewayServerFrame {
    // ... existing variants ...
    TurnProgress(GatewayTurnProgress),  // ← new
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayTurnProgress {
    pub request_id: String,
    pub session: GatewaySession,
    pub turn: GatewayTurnStatus,
    pub progress: RuntimeProgressEvent,
}
```

**Step 4: Forward progress in the gateway**

In `crates/gateway/src/turn_runner.rs`, change the filter to forward progress events:

```rust
// Currently (line 87):
if let Some(StreamItem::Text(delta)) = maybe_event {
    let _ = delta_sender.send(delta);
}

// Change delta_sender to carry StreamItem instead of String,
// or add a second channel for progress events.
```

The cleanest approach: change `delta_sender` from `mpsc::UnboundedSender<String>` to `mpsc::UnboundedSender<StreamItem>`. Then in `gateway/src/lib.rs` `start_turn()`, match on the received item:

```rust
match maybe_delta {
    Some(StreamItem::Text(delta)) => {
        session.publish(GatewayServerFrame::AssistantDelta(...));
    }
    Some(StreamItem::Progress(progress)) => {
        session.publish(GatewayServerFrame::TurnProgress(...));
    }
    _ => {} // Other stream items not surfaced to channels
}
```

This change to `delta_sender`'s type is internal to the gateway crate and does not affect the `GatewayTurnRunner` trait's public API — the trait's `delta_sender` parameter type changes but that's used only by `RuntimeGatewayTurnRunner`.

**Step 5: Channel-specific rendering (TUI example)**

Each channel decides how to present progress. For TUI:

In `tui/src/ui_model.rs`, handle `GatewayServerFrame::TurnProgress` in `apply_server_frame()`:

```rust
GatewayServerFrame::TurnProgress(progress) => {
    let msg = format!("[{}/{}] {}", progress.progress.turn,
        progress.progress.max_turns, progress.progress.message);
    // Update a status field rather than pushing to message history,
    // so progress messages don't clutter the conversation.
    self.status_line = msg;
}
```

This keeps progress transient (shown in the status bar area, not permanently in chat history). Alternatively, tool execution events could be appended as dim system messages — that's a rendering choice the TUI makes, not a protocol decision.

**Complexity assessment:** This is ~150 lines of new code across 5 files. The `StreamItem` and gateway protocol changes are additive (no existing behavior changes). The runtime changes are small insertions at existing log points. There's no restructuring needed. The main consideration is the `delta_sender` type change in the gateway, but that's internal.

**What this does NOT do (and shouldn't):**
- Does not surface tool call arguments or results (security concern — tool output goes through scrubbing)
- Does not surface individual streaming chunks for tool call JSON assembly (too noisy)
- Does not add a separate WebSocket channel for progress (over-engineering)
- Does not require channels to handle progress — it's optional for any channel implementation

### 6c. No Graceful Shutdown — Socket and Marker File Cleanup

**Problem:** When oxydra-vm or the runner exits (normally or via signal), the `gateway-endpoint` marker file and socket files are not cleaned up. Stale marker files cause `--tui` mode to attempt connection to a dead endpoint, leading to confusing errors.

**Root Cause:** `oxydra-vm.rs` writes the marker file but never removes it. The runner daemon (`main.rs:171`) cleans up `runner-control.sock` on shutdown, but `gateway-endpoint` cleanup is the VM's responsibility and it doesn't do it.

**Fix:**
1. In `oxydra-vm.rs`, register a shutdown handler (via `tokio::signal::ctrl_c()` or a `Drop` guard) that removes the marker file on exit
2. In the runner's `RunnerStartup::shutdown()`, remove the `gateway-endpoint` marker file from `workspace.tmp` (or `workspace.ipc` after Issue 1)
3. In `connect_tui()`, if the marker file exists but the gateway probe fails, provide a clear error message: "Found gateway endpoint file but the gateway is not responding. The previous session may have exited without cleanup. Remove `<path>` and restart."

### 6d. TUI Input Bar Should Show Disabled State More Clearly

**Problem:** When the TUI is disconnected or reconnecting, the input bar border color changes to gray (`widgets.rs:235-238`) but the visual difference is subtle. The user may not realize input is disabled.

**Fix:**
1. Show a clear text message inside the input area during disconnected/reconnecting states: `"[Disconnected — reconnecting...]"` or `"[Connection lost — Ctrl+C to exit]"`
2. The status bar already shows connection state, but the input area itself should reinforce this since that's where the user's attention is

### 6e. `oxydra-tui` Requires Separate Installation

**Problem:** Running `--tui` on the runner (`main.rs:113`) calls `launch_tui_binary()` which searches PATH for `oxydra-tui`. If not installed, the user gets:

```
`oxydra-tui` was not found in PATH. Install it with `cargo install --path crates/tui`...
```

This is a friction point — the user has to know about and separately install the TUI binary.

**Fix (two approaches):**

Approach A — **Bundled binary discovery** (preferred): The runner already has `bundled_process_tier_executable()` logic for finding `oxydra-vm` next to the runner binary. Apply the same pattern for `oxydra-tui`:
1. Check if `oxydra-tui` exists in the same directory as the runner executable
2. If found, use it; otherwise fall back to PATH search
3. The build system (scripts, CI) should ensure both binaries are placed together

Approach B — **Embed TUI in runner**: Instead of spawning a separate binary, make the runner's `--tui` mode directly use the `tui` crate as a library dependency. The `tui` crate already exposes `TuiApp` as a public struct. This eliminates the need for a separate binary entirely, but increases the runner's binary size and couples them.

**Recommendation:** Approach A is less invasive. Approach B is cleaner long-term but requires careful consideration of the binary size and separation-of-concerns trade-off.

---

## Summary Table

| # | Issue | Root Cause | Severity | Fix Scope |
|---|-------|-----------|----------|-----------|
| 1 | Socket files in shared `tmp/` | IPC and scratch share the same directory | **Critical** (security) | runner, oxydra-vm, backend, sandbox policy |
| 2 | No mouse copy/paste | `EnableMouseCapture` enabled but mouse events are dropped | **Medium** | app.rs (2 lines) |
| 3 | TUI frozen on disconnect | `reconnect_loop()` blocks the main `select!` loop | **High** | app.rs (restructure main loop) |
| 4 | Manual port discovery | Gateway port not surfaced to runner stdout; no auto-discovery in TUI | **High** | main.rs, oxydra-vm.rs, oxydra-tui.rs |
| 5 | Insufficient logging | Missing key lifecycle events at INFO/DEBUG | **Medium** | types, runtime, gateway, runner, oxydra-vm |
| 6a | No multi-line input | `Enter` = submit, no alternative for newlines | **Low** | event_loop.rs, widgets.rs |
| 6b | No runtime activity visibility | Progress events not emitted or forwarded to channels | **Medium** | types, runtime, gateway, tui (~150 LOC) |
| 6c | Stale marker/socket files | No cleanup on shutdown | **Medium** | oxydra-vm.rs, runner lib.rs |
| 6d | Subtle disabled input state | Only border color changes on disconnect | **Low** | widgets.rs |
| 6e | TUI binary not bundled | Separate `cargo install` required | **Low** | main.rs, build scripts |

### Recommended Fix Order

1. **Issue 3** — TUI frozen on disconnect (most immediately frustrating UX problem)
2. **Issue 2** — Mouse copy/paste (trivial 2-line fix, high UX impact)
3. **Issue 1** — Socket security (critical security, requires more files but straightforward)
4. **Issue 4** — Port discovery (Step 1: print in runner stdout; Step 2: auto-discovery)
5. **Issue 6c** — Stale file cleanup (pairs naturally with Issue 1)
6. **Issue 5** — Logging (incremental, can be done alongside other changes)
7. **Issue 6a-6e** — Remaining UX improvements (lower priority, incremental)
