# Plan: rust-bash as Sandboxed Shell for Process Tier

**Status:** Proposed  
**Created:** 2026-04-04  
**Updated:** 2026-04-04  
**Reviewed by:** Oracle (GPT-5.4) — 2026-04-04

---

## Executive Summary

The Process tier currently hard-disables shell tools because it has no sidecar and `--insecure` host execution is deemed too risky as a default. This plan introduces **rust-bash** — a pure-Rust sandboxed bash interpreter — as a new `BashBackend` variant, giving Process tier a **limited but genuinely sandboxed shell** without containers, VMs, or host process spawning.

rust-bash executes bash scripts entirely in-process with a virtual filesystem, 80+ built-in commands (grep, sed, awk, jq, find, curl, etc.), configurable execution limits, and network policy controls. No `std::process::Command` is ever called — every command is a Rust reimplementation. This makes it safe to enable without the `--insecure` flag.

### Relationship to existing plans

The existing `process-tier-shell-browser.md` plan describes enabling shell via `LocalProcessShellSession` (real host bash execution, requiring `--insecure`). This plan is **complementary, not competing** — it addresses a different use case:

| | This plan (rust-bash) | Existing plan (host-local) |
|---|---|---|
| **Execution model** | In-process Rust interpreter | Real `std::process::Command` |
| **Commands available** | 80+ built-in only | All host system commands |
| **Requires `--insecure`** | No | Yes |
| **Can run cargo/git/python** | No | Yes |
| **Isolation guarantee** | Genuine sandbox (VFS + limits) | Best-effort OS hardening |
| **Use case** | Text processing, file manipulation, data transformation | Full development workflows |

Both can coexist: rust-bash is the safe default for Process tier; host-local shell (if/when implemented) is the opt-in `--insecure` escalation for development workflows that need real toolchains.

---

## Goals

1. Give Process tier a usable shell without `--insecure` or containers.
2. Provide genuine sandboxing: virtual filesystem, execution limits, no subprocess spawning.
3. Integrate cleanly with existing `BashTool`/`BashBackend` architecture.
4. Respect existing config model: agent.toml controls policy, runner-user.toml applies restrictions.
5. Make the "limited" nature transparent: clear tool description, startup reporting, and error messages when agents try unavailable commands.
6. No changes to Container or MicroVm tier behavior.

## Non-goals

1. Replacing the sidecar shell for Container/MicroVm tiers.
2. Bridging rust-bash commands to real host processes (no escape hatches).
3. Supporting browser tool via rust-bash (browser remains disabled in Process tier).
4. Full bash compatibility parity — rust-bash is "good enough" for text processing and file operations.

---

## Architecture

### New BashBackend variant

```rust
// crates/tools/src/lib.rs
enum BashBackend {
    Host,
    Session(Arc<Mutex<Box<dyn ShellSession>>>),
    #[cfg(feature = "sandboxed-shell")]
    Sandboxed(Arc<std::sync::Mutex<rust_bash::RustBash>>),  // NEW — std::sync::Mutex, not tokio
    Disabled(SessionStatus),
}
```

The `Sandboxed` backend calls `rust_bash::RustBash::exec()` directly — no `ShellSession` trait needed since rust-bash is synchronous and returns `ExecResult { stdout, stderr, exit_code }` in one call (no streaming).

**Important: `std::sync::Mutex`**, not `tokio::sync::Mutex`, because `exec()` is synchronous and must be called inside `spawn_blocking`.

### Filesystem strategy: MountableFs with explicit mounts

**Critical design constraint:** `WorkspaceSecurityPolicy::enforce_shell_command()` only validates the _command name_ against the allowlist — it does **not** parse or validate file path arguments inside shell commands. This means the VFS backend is the **real filesystem boundary** for rust-bash. Rooting at the wrong level would expose sensitive directories.

Additionally, the existing shell contract presents files at paths like `/shared/foo`, `/tmp/bar`. If we naively root `ReadWriteFs` at `<workspace>/shared/` and set `cwd="/"`, then `/shared/foo` would resolve to `<workspace>/shared/shared/foo` — which is wrong.

**Solution: Use `MountableFs`** with explicit mounts matching the established path contract:

```rust
use rust_bash::{MountableFs, ReadWriteFs, InMemoryFs};

let mountable = MountableFs::new()
    .mount("/", Arc::new(InMemoryFs::new()))                                      // empty root
    .mount("/shared", Arc::new(ReadWriteFs::with_root(&workspace_shared)?))       // read-write
    .mount("/tmp", Arc::new(ReadWriteFs::with_root(&workspace_tmp)?));            // read-write
    // /vault is intentionally NOT mounted — shell has no vault access
    // /.oxydra is intentionally NOT mounted — internal directory is hidden

let shell = RustBashBuilder::new()
    .fs(Arc::new(mountable))
    .cwd("/shared")
    .build()?;
```

**Why this layout:**
- `/shared` → read-write via `ReadWriteFs`: agent files live here, and file_write/file_edit tools also write here, keeping both views consistent
- `/tmp` → read-write via `ReadWriteFs`: scratch space, consistent with other tiers
- `/vault` → **not mounted**: vault access requires the two-step vault_copyto flow, not direct shell access
- `/.oxydra` → **not mounted**: internal directory is denied at the policy level in all tiers
- `/` root → `InMemoryFs`: safe fallback for paths outside known mounts
- `cwd` starts at `/shared` to match agent expectations

**Why not raw ReadWriteFs at workspace root:** that would expose `/.oxydra/` (internal DB, config) and `/vault/` (secrets) to shell commands like `cat /.oxydra/db.sqlite3` — a security gap since shell command arguments are not path-checked.

**Why not OverlayFs:** the agent uses file_write/file_edit tools that write to real disk. If shell operates on a separate overlay, `cat /shared/file.txt` wouldn't see edits made by file_write. Both tools must see the same files.

### Execution model: always spawn_blocking

rust-bash's `exec()` is synchronous and can block for up to `max_execution_time` (default 30s). Running it inline in an async context would block the tokio runtime.

**Mandatory approach:** always use `tokio::task::spawn_blocking`:

```rust
let result = tokio::task::spawn_blocking(move || {
    shell.lock().unwrap().exec(&command)
}).await.map_err(|e| /* JoinError handling */)?;
```

**Timeout strategy:** Set rust-bash's inner `max_execution_time` to be slightly shorter (e.g., 5s less) than the outer oxydra tool timeout. This ensures rust-bash terminates the script gracefully before the outer timeout fires, avoiding zombie blocking tasks. If the inner timeout fires, the error message clearly says "execution time limit exceeded".

### Execution limits mapping

| agent.toml | rust-bash `ExecutionLimits` |
|---|---|
| `command_timeout_secs` | `max_execution_time` (minus 5s safety margin) |
| (hardcoded defaults) | `max_command_count: 10_000` |
| (hardcoded defaults) | `max_output_size: 10MB` |
| (hardcoded defaults) | Other limits use rust-bash defaults |

### Network policy

Network access from the sandboxed shell is **hard-disabled** for v1. No configuration toggle.

rust-bash supports a `NetworkPolicy` with URL allow-lists, but enabling it would bypass oxydra's SSRF protections (IP blocking, domain validation) that the `web_fetch` tool enforces. The `curl` command inside rust-bash would have a weaker security posture than `web_fetch`.

```rust
NetworkPolicy {
    enabled: false,
    ..Default::default()
}
```

If shell network access is needed in the future (v2), it should either route through oxydra's existing web security policy or replicate its SSRF protections.

**Note:** The dependency must include the `network` feature for `NetworkPolicy` to be available (even to disable it). Without the feature, the stub module provides a `NetworkPolicy::default()` with `enabled: false`, which is equivalent.

### Command allowlist interaction

The existing `WorkspaceSecurityPolicy::enforce_shell_command()` checks commands against the configured allowlist before execution. This check still applies to the rust-bash backend — it provides defense-in-depth even though rust-bash only has built-in commands.

The current default allowlist includes `cargo`, `git`, `python`, etc. which don't exist in rust-bash. **Use the existing allowlist as-is.** Commands not in rust-bash simply fail with "command not found" from rust-bash itself. The allowlist is a ceiling, not a floor — no config changes needed.

---

## Detailed Code Changes

### Phase 1: Add rust-bash dependency and BashBackend::Sandboxed

#### `Cargo.toml` (workspace root)

Add rust-bash to workspace dependencies. Use git source since crates.io does not have the latest releases due to an upstream `brush-parser` dependency that is only available via git. Include `network` feature so `NetworkPolicy` is available (even though we disable it — the type must be constructible):

```toml
[workspace.dependencies]
rust-bash = { git = "https://github.com/shantanugoel/rust-bash.git", default-features = false, features = ["native-fs", "network"] }
```

**Note:** Pin to a specific `rev = "..."` or `tag = "..."` once a stable commit is chosen for integration. Using an unpinned git dependency is acceptable during development but should be pinned before merging.

#### `crates/tools/Cargo.toml`

Add optional rust-bash dependency:

```toml
[features]
default = ["wasm-isolation"]
wasm-isolation = ["dep:wasmtime", "dep:wasmtime-wasi"]
sandboxed-shell = ["dep:rust-bash"]   # NEW

[dependencies]
rust-bash = { workspace = true, optional = true }
```

#### `crates/runner/Cargo.toml`

Propagate the feature flag so `cfg!(feature = "sandboxed-shell")` works in runner code:

```toml
[features]
default = ["sandboxed-shell"]
sandboxed-shell = ["tools/sandboxed-shell"]   # NEW — propagates to tools crate
```

#### `crates/tools/src/lib.rs` — BashBackend and BashTool changes

1. Add the new variant (note: `std::sync::Mutex`, not `tokio::sync::Mutex`):

```rust
enum BashBackend {
    Host,
    Session(Arc<Mutex<Box<dyn ShellSession>>>),
    #[cfg(feature = "sandboxed-shell")]
    Sandboxed(Arc<std::sync::Mutex<rust_bash::RustBash>>),
    Disabled(SessionStatus),
}
```

2. Add constructor:

```rust
impl BashTool {
    #[cfg(feature = "sandboxed-shell")]
    pub fn from_sandboxed_shell(shell: rust_bash::RustBash, timeout: Duration) -> Self {
        Self {
            backend: BashBackend::Sandboxed(Arc::new(std::sync::Mutex::new(shell))),
            command_timeout: timeout,
        }
    }
}
```

3. Add execution arm in `BashTool::execute()` (after the existing `BashBackend::Host` arm, around line 862).

**Must use `spawn_blocking`** — rust-bash exec is synchronous:

```rust
#[cfg(feature = "sandboxed-shell")]
BashBackend::Sandboxed(shell) => {
    let shell = Arc::clone(shell);
    let command = request.command.clone();
    let result = tokio::task::spawn_blocking(move || {
        let mut guard = shell.lock().map_err(|e| e.to_string())?;
        guard.exec(&command).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| execution_failed(SHELL_EXEC_TOOL_NAME, format!("task join error: {e}")))?
    .map_err(|e| execution_failed(SHELL_EXEC_TOOL_NAME, e))?;

    let combined = combine_command_output(
        result.stdout.as_bytes(),
        result.stderr.as_bytes(),
    );

    if result.exit_code == 0 {
        if combined.is_empty() {
            Ok("command completed with no output".to_owned())
        } else {
            Ok(combined)
        }
    } else {
        let message = if combined.is_empty() {
            format!("command exited with status {}", result.exit_code)
        } else {
            format!(
                "command exited with status {}: {}",
                result.exit_code, combined
            )
        };
        Err(execution_failed(SHELL_EXEC_TOOL_NAME, message))
    }
}
```

### Phase 2: Bootstrap wiring

The current codebase equates "shell available" with "sidecar exists" in multiple places. All must be updated for Process tier sandboxed shell.

#### Sites that assume shell ↔ sidecar (all need changes)

| Location | Current assumption | Change needed |
|---|---|---|
| `runner/src/lib.rs:~1677` | `resolve_requested_capabilities()` hard-disables shell for Process | Allow shell when `sandboxed-shell` feature is enabled |
| `runner/src/lib.rs:~162-170` | `pre_compute_sidecar_endpoint()` called when shell requested | Skip sidecar endpoint for Process tier |
| `runner/src/lib.rs:~172-182` | `build_startup_status_report()` derives `sidecar_available` from shell/browser | `sidecar_available` must be `false` when shell uses sandboxed backend |
| `runner/src/lib.rs:~1637-1670` | Startup status assumes sidecar for shell | Handle sandboxed shell case |
| `runner/src/lib.rs:~1337-1342` | Control-plane health equates shell with sidecar | Distinguish sandboxed shell |
| `runner/src/lib.rs:~66` | `PROCESS_TIER_WARNING` says "shell/browser tools are disabled" | Update text when sandboxed shell is enabled |
| `types/src/runner.rs:~712-727` | `RunnerBootstrapEnvelope::validate()` rejects shell without sidecar | Allow for Process tier |
| `tools/src/lib.rs:~975-1032` | `bootstrap_bash_tool()` requires sidecar endpoint | Add sandboxed shell path |
| `tools/src/registry.rs:~259-277` | `ToolAvailability::startup_status()` sets `sidecar_available = shell\|\|browser` | Don't set sidecar_available for sandboxed shell |
| `runner/src/bootstrap.rs:~917-922` | System prompt says Process tier disables shell/browser | Update for sandboxed shell |
| `runner/src/backend.rs:~429-430` | `launch_process()` returns `shell_available: false` | Return `true` when feature enabled |

#### `crates/runner/src/lib.rs` — resolve_requested_capabilities (line ~1677)

Lift the Process tier shell gate when the `sandboxed-shell` feature is enabled:

```rust
fn resolve_requested_capabilities(
    sandbox_tier: SandboxTier,
    agent_tools: &types::ToolsConfig,
    user_behavior: &types::RunnerBehaviorOverrides,
) -> RequestedCapabilities {
    let mut shell = match sandbox_tier {
        SandboxTier::Process => {
            // rust-bash provides a sandboxed shell for Process tier
            cfg!(feature = "sandboxed-shell") && agent_tools.shell_enabled()
        }
        _ => agent_tools.shell_enabled(),
    };
    // browser remains disabled for Process tier (unchanged)
    let mut browser =
        !matches!(sandbox_tier, SandboxTier::Process) && agent_tools.browser_enabled();

    if let Some(enabled) = user_behavior.shell_enabled {
        shell &= enabled;
    }
    if let Some(enabled) = user_behavior.browser_enabled {
        browser &= enabled;
    }

    RequestedCapabilities { shell, browser }
}
```

#### `crates/runner/src/lib.rs` — pre_compute_sidecar_endpoint guard (line ~162)

Process tier with sandboxed shell does not need a sidecar endpoint:

```rust
let pre_sidecar_endpoint = if (capabilities.shell || capabilities.browser)
    && sandbox_tier != SandboxTier::Process  // no sidecar for process tier
{
    Some(pre_compute_sidecar_endpoint(sandbox_tier, host_os, &workspace))
} else {
    None
};
```

#### `crates/types/src/runner.rs` — RunnerBootstrapEnvelope::validate()

Relax **both** validation constraints for Process tier. The current code has two checks:

```rust
// Check 1: sidecar_endpoint=None → shell/browser must be false
// Relax for Process tier:
if self.sidecar_endpoint.is_none()
    && self.sandbox_tier != SandboxTier::Process
    && (startup_status.sidecar_available
        || startup_status.shell_available
        || startup_status.browser_available)
{
    return Err(BootstrapEnvelopeError::InvalidField {
        field: "startup_status.shell_available",
    });
}

// Check 2: shell/browser requires sidecar_available
// Relax for Process tier:
if self.sandbox_tier != SandboxTier::Process
    && (startup_status.shell_available || startup_status.browser_available)
    && !startup_status.sidecar_available
{
    return Err(BootstrapEnvelopeError::InvalidField {
        field: "startup_status.sidecar_available",
    });
}
```

#### `crates/tools/src/lib.rs` — bootstrap_bash_tool (line ~975)

Add a sandboxed shell path when no sidecar endpoint exists but Process tier shell is requested:

```rust
async fn bootstrap_bash_tool(
    bootstrap: Option<&RunnerBootstrapEnvelope>,
) -> (BashTool, SessionStatus, SessionStatus) {
    let Some(bootstrap) = bootstrap else {
        // ... existing: no bootstrap envelope → disabled
    };

    let Some(endpoint) = bootstrap.sidecar_endpoint.clone() else {
        // NEW: Process tier with sandboxed shell
        #[cfg(feature = "sandboxed-shell")]
        if bootstrap.sandbox_tier == SandboxTier::Process {
            if let Some(status) = &bootstrap.startup_status {
                if status.shell_available {
                    return bootstrap_sandboxed_shell(bootstrap);
                }
            }
        }

        // ... existing: no sidecar → disabled
    };

    // ... existing: connect to sidecar
}
```

New helper function. Note: uses types/methods that actually exist in the codebase (the workspace path and shell config are extracted from the bootstrap envelope's known fields):

```rust
#[cfg(feature = "sandboxed-shell")]
fn bootstrap_sandboxed_shell(
    bootstrap: &RunnerBootstrapEnvelope,
) -> (BashTool, SessionStatus, SessionStatus) {
    use rust_bash::{RustBashBuilder, ExecutionLimits, NetworkPolicy, MountableFs, ReadWriteFs, InMemoryFs};

    // Extract workspace paths from the bootstrap envelope.
    // The workspace root is available from the envelope; shared/ and tmp/ are
    // subdirectories within it.
    let workspace_root = &bootstrap.workspace_root;
    let shared_path = workspace_root.join("shared");
    let tmp_path = workspace_root.join("tmp");

    // Extract timeout from the effective shell config embedded in the envelope,
    // falling back to the default.
    let timeout_secs = bootstrap
        .effective_shell_config
        .as_ref()
        .and_then(|c| c.command_timeout_secs)
        .unwrap_or(DEFAULT_SHELL_COMMAND_TIMEOUT_SECS);

    // Inner timeout slightly shorter than outer, so rust-bash terminates
    // gracefully before the tool-level timeout fires.
    let inner_timeout = timeout_secs.saturating_sub(5).max(5);

    let limits = ExecutionLimits {
        max_execution_time: Duration::from_secs(inner_timeout),
        ..Default::default()
    };

    // Build MountableFs matching the established path contract:
    // /shared → read-write, /tmp → read-write, /vault and /.oxydra → not mounted
    let mountable = match (|| -> Result<MountableFs, String> {
        let shared_fs = ReadWriteFs::with_root(&shared_path)
            .map_err(|e| format!("shared mount: {e}"))?;
        let tmp_fs = ReadWriteFs::with_root(&tmp_path)
            .map_err(|e| format!("tmp mount: {e}"))?;
        Ok(MountableFs::new()
            .mount("/", Arc::new(InMemoryFs::new()))
            .mount("/shared", Arc::new(shared_fs))
            .mount("/tmp", Arc::new(tmp_fs)))
    })() {
        Ok(fs) => fs,
        Err(e) => {
            tracing::warn!("failed to create sandboxed shell filesystem: {e}");
            let status = unavailable_status(
                SessionUnavailableReason::Disabled,
                format!("sandboxed shell filesystem init failed: {e}"),
            );
            return (
                BashTool::from_status(status.clone()),
                status.clone(),
                status,
            );
        }
    };

    let shell = match RustBashBuilder::new()
        .fs(Arc::new(mountable))
        .cwd("/shared")
        .execution_limits(limits)
        .network_policy(NetworkPolicy { enabled: false, ..Default::default() })
        .build()
    {
        Ok(shell) => shell,
        Err(e) => {
            tracing::warn!("failed to build sandboxed shell: {e}");
            let status = unavailable_status(
                SessionUnavailableReason::Disabled,
                format!("sandboxed shell init failed: {e}"),
            );
            return (
                BashTool::from_status(status.clone()),
                status.clone(),
                status,
            );
        }
    };

    let tool = BashTool::from_sandboxed_shell(
        shell,
        Duration::from_secs(timeout_secs),
    );
    let shell_status = SessionStatus::ready();
    // Browser remains disabled for Process tier
    let browser_status = unavailable_status(
        SessionUnavailableReason::Disabled,
        "browser tools are not available with the sandboxed shell",
    );
    (tool, shell_status, browser_status)
}
```

#### `crates/runner/src/backend.rs` — launch_process()

Update to report `shell_available: true` when `sandboxed-shell` feature is enabled:

```rust
// In launch_process() return values
shell_available: cfg!(feature = "sandboxed-shell"),
browser_available: false,
```

#### `crates/tools/src/registry.rs` — ToolAvailability::startup_status()

The current code sets `sidecar_available = shell_available || browser_available`. For Process tier with sandboxed shell, `sidecar_available` must remain `false`:

```rust
// When computing startup_status:
let sidecar_available = match sandbox_tier {
    SandboxTier::Process => false,  // sandboxed shell, no sidecar
    _ => shell_available || browser_available,
};
```

### Phase 3: Startup reporting and system prompt

#### Degraded reason

Add a new degraded reason code for sandboxed shell mode:

```rust
pub enum StartupDegradedReasonCode {
    // ... existing variants ...
    SandboxedShellLimited,  // shell available but limited to built-in commands
}
```

When Process tier launches with sandboxed shell, push this degraded reason with detail:

```
"shell is available via sandboxed interpreter (80+ built-in commands: grep, sed, awk, jq, find, curl, etc.). System commands (cargo, git, python, node) are not available. Use Container or MicroVm tier for full shell access."
```

#### System prompt appendage

In `crates/runner/src/bootstrap.rs`, when sandboxed shell is active, append to the system prompt:

```
Note: Shell commands execute in a sandboxed interpreter with built-in commands only.
Available: echo, cat, grep, sed, awk, jq, find, sort, curl, diff, tar, and ~70 more text/file utilities.
NOT available: cargo, git, python, node, npm, pip, or any system-installed programs.
Use shell for text processing, file inspection, and data transformation.
```

This is critical for agent effectiveness — the LLM needs to know what's available to avoid futile command attempts.

### Phase 4: Tool description update

The `shell_exec` tool definition (`SHELL_EXEC_TOOL_NAME`) has a description string that's sent to the LLM. When in sandboxed mode, this description should reflect the limited command set:

```rust
fn shell_tool_description(sandboxed: bool) -> &'static str {
    if sandboxed {
        "Execute bash commands in a sandboxed interpreter. Supports 80+ built-in commands \
         (echo, cat, grep, sed, awk, jq, find, sort, curl, diff, tar, etc.) for text processing \
         and file operations. System commands (cargo, git, python, node) are not available."
    } else {
        // existing description for sidecar/host shell
        "Execute a bash command on the system."
    }
}
```

---

## Configuration Model

### No new config fields needed (with one caveat)

The existing configuration model is sufficient for controlling access:

- **`agent.toml [tools.shell]`**: `enabled`, `allow`, `deny`, `command_timeout_secs` — all apply to rust-bash
- **`runner-user.toml [behavior]`**: `shell_enabled` — restrictive override still works
- **Feature flag**: `sandboxed-shell` in Cargo.toml controls compile-time inclusion

The Process tier shell is enabled by default when the `sandboxed-shell` feature is compiled in and `agent.toml` has `tools.shell.enabled = true` (the default). No new `process_tier.shell_enabled` runner config is needed because the sandboxed shell is safe to enable without operator opt-in — unlike the host-local shell plan which requires explicit operator approval.

**Caveat:** The `RunnerBootstrapEnvelope` needs to carry the effective `ShellConfig` (or at least `command_timeout_secs`) so that `bootstrap_sandboxed_shell()` can configure execution limits. This may require adding an `effective_shell_config: Option<ShellConfig>` field to the envelope, or passing it as a separate parameter. See Open Questions.

### Effective resolution

```
effective_shell =
  (tier == Process AND sandboxed-shell feature AND agent.tools.shell.enabled)
  OR (tier != Process AND sidecar_shell_available)
  AND user.behavior.shell_enabled.unwrap_or(true)
```

---

## Testing Strategy

### Unit tests

1. **BashBackend::Sandboxed execution**: Verify basic commands (echo, grep, cat, jq) return correct stdout/stderr/exit_code through the BashTool interface.
2. **Exit code propagation**: Non-zero exit codes produce the expected error format.
3. **Timeout enforcement**: Commands exceeding `max_execution_time` are terminated; error message says "execution time limit exceeded", not a generic timeout.
4. **Filesystem boundary — vault denied**: `cat /vault/secret.txt` fails (path does not exist, not mounted).
5. **Filesystem boundary — internal denied**: `cat /.oxydra/db.sqlite3` fails (not mounted).
6. **Filesystem boundary — shared works**: `echo test > /shared/file.txt && cat /shared/file.txt` succeeds.
7. **Filesystem boundary — escape denied**: `cat /etc/passwd`, `cat /../../../etc/passwd` fail (outside mounts).
8. **Network disabled**: `curl http://example.com` fails when network policy is disabled.
9. **Unknown commands**: Running `cargo build` or `git status` produces a clear "command not found" error.
10. **spawn_blocking**: Verify the execution does not block the async runtime (concurrent async tasks continue during shell exec).

### Integration tests

1. **Process tier bootstrap**: Start runner with Process tier (no `--insecure`), verify shell tool is registered and functional.
2. **Bootstrap envelope validation**: Verify `RunnerBootstrapEnvelope::validate()` accepts `shell_available: true` with `sidecar_endpoint: None` for Process tier.
3. **Bootstrap envelope validation (negative)**: Verify non-Process tiers still reject `shell_available: true` without sidecar.
4. **Agent interaction**: Run a simulated agent turn that uses shell for text processing (grep, awk, jq pipeline).
5. **Config interaction**: Verify `tools.shell.enabled = false` disables sandboxed shell. Verify `behavior.shell_enabled = false` disables it for that user.
6. **Startup status**: Verify `SandboxedShellLimited` degraded reason appears in status.
7. **Sidecar not attempted**: Verify Process tier does not attempt sidecar connection when sandboxed shell is enabled.
8. **Tool availability reporting**: Verify `sidecar_available` is `false` when sandboxed shell is the backend.

### Compatibility tests

1. **Common agent patterns**: Test the shell commands LLMs typically generate:
   - `grep -rn "pattern" /shared/src/`
   - `cat /shared/data.json | jq '.key'`
   - `find /shared -name "*.rs" -type f`
   - `echo "text" > /shared/output.txt && cat /shared/output.txt`
   - `sed 's/old/new/g' /shared/file.txt`
   - `wc -l /shared/src/*.rs | sort -n`
   - `ls -la /shared/`
   - `diff /shared/file1.txt /shared/file2.txt`

---

## Risks and Mitigations

| Risk | Severity | Mitigation |
|---|---|---|
| **Sidecar-coupling bugs** | High | The codebase assumes shell↔sidecar in 11+ locations. Most likely failure: "Process tier says shell available, but guest still tries connecting to nonexistent sidecar." Mitigate with thorough integration tests for bootstrap envelopes and tool registration in Process tier. |
| **Filesystem-policy drift** | High | `WorkspaceSecurityPolicy` does NOT parse file arguments inside shell commands — only the command name. The VFS is the real boundary. MountableFs with explicit mounts (no `/vault`, no `/.oxydra`) is critical. Test with `cat /.oxydra/db.sqlite3` to verify it fails. |
| **bash compatibility gaps** | Medium | rust-bash uses brush-parser and has extensive spec tests. Agent system prompt explicitly lists available commands. Pin to specific version. |
| **Agent frustration with missing commands** | Medium | Clear error messages + system prompt guidance + tool description update. Agent learns to use file_read/file_write for tasks that need system tools. |
| **Alpha dependency as security boundary** | Medium | rust-bash is alpha and becomes part of the security boundary (VFS isolation). Pin version, feature-gate, integration tests. Monitor upstream for security-relevant changes. |
| **Timeout/cancellation leaks** | Medium | `spawn_blocking` + inner timeout shorter than outer. If sandboxed exec overruns unexpectedly, the JoinError is caught. Consider marking the shell instance unusable if inner timeout fires. |
| **Misleading UX** | Medium | Prompt/tool description currently advertise Process tier as "no shell". If changed incompletely, agents will either ignore the shell or misuse it. Backend-specific tool description + system prompt + degraded reason must all be updated. |
| **Blocking async runtime** | Low | Always `spawn_blocking` with `std::sync::Mutex`. Inner timeout prevents unbounded blocking. |
| **Dependency size** | Low | Feature-gated behind `sandboxed-shell`. Only compiled when needed. |

---

## Implementation Order

1. **Phase 1**: Add dependency, `BashBackend::Sandboxed`, execution arm — can be tested in isolation
2. **Phase 2**: Bootstrap wiring — lifts the Process tier gate, wires sandboxed shell creation
3. **Phase 3**: Startup reporting — degraded reasons, system prompt
4. **Phase 4**: Tool description — LLM-facing description update
5. **Testing**: Unit + integration + compatibility tests throughout

Phases 1-2 are the core work. Phases 3-4 are polish but important for agent effectiveness.

---

## Resolved Questions (from Oracle review)

1. **Filesystem rooting** — Resolved: Use `MountableFs` with `/shared` (read-write), `/tmp` (read-write), no `/vault`, no `/.oxydra`. See Architecture → Filesystem strategy above.

2. **Synchronous exec + async runtime** — Resolved: Always `spawn_blocking` with `std::sync::Mutex`. Inner rust-bash timeout set 5s shorter than outer tool timeout.

3. **Network in sandboxed shell** — Resolved: Hard-disabled for v1. No config toggle. See Architecture → Network policy above.

## Open Questions

1. **Should the `sandboxed-shell` feature be default-on?** Oracle recommends default-off for one release/bake-in cycle, then default-on after real usage testing. Counter-argument: since it's genuinely sandboxed and safe, default-on gives immediate value. **Decision needed.**

2. **Should `effective_shell_config` be added to `RunnerBootstrapEnvelope`?** The bootstrap helper needs the shell config (timeout, etc.) from agent.toml. Currently this config is either written to disk (for sidecar) or not available in the envelope. Options: (a) add an `effective_shell_config: Option<ShellConfig>` field to the envelope, or (b) pass it as a separate parameter to `bootstrap_bash_tool()`. **Decision needed.**

3. **MountableFs `/tmp` access scope** — Should `/tmp` be read-write (matching other tiers) or read-only (more restrictive)? Read-write is consistent with Container/MicroVm behavior. **Recommendation: read-write for consistency.**
