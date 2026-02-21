# Phase 10 Gap Fix: Implementation Plan

## Overview

This plan addresses three Phase 10 gaps that must be resolved before Phase 12 (TUI/ops need lifecycle control). All changes are additive and do not rewrite existing Phase 10 work.

**Gaps covered:**
- **A** — Runner control plane not wired (persistent daemon + VM log capture)
- **B** — Container/microvm bootstrap wiring (args + envelope delivery)
- **C** — Linux microvm config input mismatch (Firecracker expects JSON path, gets OCI tag)

**Execution order:** A1 → A2 → C → B-docker → B-microvm-linux → B-microvm-macos → Verification

---

## Step 1: Runner Persistent Daemon (Gap A — Control Plane Wiring)

**Problem:** `main.rs` prints startup status and exits. `RunnerStartup::serve_control_unix_listener` (lib.rs:755-763) and all control handlers (lib.rs:652-731) exist but are never called.

**Crates touched:** `runner` (main.rs only)

### 1.1 Add `--daemon` flag to CLI

**File:** `crates/runner/src/main.rs`

```rust
// Add to CliArgs struct:
#[arg(long = "daemon")]
daemon: bool,
```

### 1.2 Add async runtime + daemon loop in `main.rs`

**File:** `crates/runner/src/main.rs`

After the existing startup status printing block, add a daemon branch:

```rust
if args.daemon {
    // Build control socket path inside the user's tmp workspace
    let control_socket_path = startup.workspace.tmp.join("runner-control.sock");

    // Remove stale socket if it exists
    let _ = std::fs::remove_file(&control_socket_path);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| CliError::Runner(RunnerError::AsyncRuntimeInit { source: e }))?;

    rt.block_on(async {
        let listener = tokio::net::UnixListener::bind(&control_socket_path)
            .map_err(|e| /* wrap in RunnerError */)?;

        println!("control_socket={}", control_socket_path.display());

        // Install signal handler for graceful shutdown
        let shutdown_signal = tokio::signal::ctrl_c();
        tokio::select! {
            result = startup.serve_control_unix_listener(listener) => {
                result.map_err(|e| /* wrap */)?;
            }
            _ = shutdown_signal => {
                startup.shutdown()?;
            }
        }
        // Cleanup socket file
        let _ = std::fs::remove_file(&control_socket_path);
        Ok(())
    })?;
}
```

### 1.3 Key behaviors

- **Foreground process**: No double-fork daemonization. Phase 12 TUI or systemd/launchd can manage it.
- **Signal handling**: SIGINT/SIGTERM triggers `startup.shutdown()` which calls `launch.shutdown()` → kills child processes.
- **Stale socket cleanup**: Remove socket file on startup if it exists (avoids "address already in use" after crash).
- **Socket path convention**: `<workspace>/tmp/runner-control.sock` — discoverable by TUI/ops via the workspace layout.

### 1.4 Tests to add

- Unit test: verify CLI parses `--daemon` flag.
- Integration test: spawn daemon in background, send `HealthCheck` via control socket, verify response, send `ShutdownUser`, verify clean exit.

---

## Step 2: Guest Log Capture (Gap A — VM Log Capture)

**Problem:** All three backend spawn paths use `Stdio::null()` for stdout/stderr (backend.rs:336-338, 358-359). Guest output is silently discarded.

**Crates touched:** `runner` (backend.rs, lib.rs)

### 2.1 Create log directory in workspace

**File:** `crates/runner/src/lib.rs` (in `provision_user_workspace`)

Add a `logs` subdirectory:

```rust
let logs = root.join("logs");
// Add to the create_dir_all loop
```

Add `logs: PathBuf` field to `UserWorkspace`.

### 2.2 Replace `Stdio::null()` with `Stdio::piped()` in process spawns

**File:** `crates/runner/src/backend.rs`

In `spawn_process_guest` (line 334-346):
```rust
// Before:
.stdin(Stdio::null())
.stdout(Stdio::null())
.stderr(Stdio::null());

// After:
.stdin(Stdio::null())
.stdout(Stdio::piped())
.stderr(Stdio::piped());
```

In `spawn_process_guest_with_startup_stdin` (line 354-359):
```rust
// Before:
.stdin(Stdio::piped())
.stdout(Stdio::null())
.stderr(Stdio::null());

// After:
.stdin(Stdio::piped())
.stdout(Stdio::piped())
.stderr(Stdio::piped());
```

### 2.3 Add log pump helper

**File:** `crates/runner/src/lib.rs` (new function)

```rust
/// Spawns background tasks that read from child stdout/stderr pipes and write
/// to log files. Returns JoinHandles for cleanup on shutdown.
fn spawn_log_pumps(
    child: &mut Child,
    log_dir: &Path,
    role: RunnerGuestRole,
) -> Vec<tokio::task::JoinHandle<()>> {
    let mut handles = Vec::new();

    if let Some(stdout) = child.stdout.take() {
        let path = log_dir.join(format!("{}.stdout.log", role.as_label()));
        handles.push(tokio::spawn(pump_to_file(stdout, path)));
    }
    if let Some(stderr) = child.stderr.take() {
        let path = log_dir.join(format!("{}.stderr.log", role.as_label()));
        handles.push(tokio::spawn(pump_to_file(stderr, path)));
    }

    handles
}

async fn pump_to_file(pipe: impl AsyncRead + Unpin, path: PathBuf) {
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::fs::OpenOptions;
    use tokio::io::AsyncWriteExt;

    let reader = BufReader::new(pipe);
    let mut lines = reader.lines();
    let Ok(mut file) = OpenOptions::new()
        .create(true).append(true).open(&path).await else { return };

    while let Ok(Some(line)) = lines.next_line().await {
        let _ = file.write_all(line.as_bytes()).await;
        let _ = file.write_all(b"\n").await;
    }
}
```

### 2.4 Wire log pumps into guest handle creation

After `spawn_process_guest` / `spawn_process_guest_with_startup_stdin` returns a `Child`, take the stdout/stderr handles before storing the child:

- Pass `log_dir` (from `SandboxLaunchRequest.workspace.logs`) into backend methods.
- Store log pump handles in `RunnerGuestHandle` (new field: `log_tasks: Vec<tokio::task::JoinHandle<()>>`).
- In `RunnerGuestHandle::shutdown()`, after killing the child, await log tasks with a 2-second timeout.

### 2.5 Docker container log capture

For Docker containers (launched via bollard, detached), use one of:
- **Option A (simpler):** After `start_container`, spawn a `docker logs -f <container_name>` process and pipe its stdout/stderr to log files.
- **Option B:** Use bollard's `logs` streaming API: `docker.logs(&container_name, ...)` and pump to files.

Prefer Option B since bollard is already a dependency.

### 2.6 Surface log paths in control plane responses

Add to `RunnerControlHealthStatus` in `types/runner.rs`:

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub log_dir: Option<String>,
```

Populate from `startup.workspace.logs` in `handle_control` → `HealthCheck` branch.

### 2.7 Tests to add

- Unit test: verify log files are created and contain expected output after a guest writes to stdout.
- Integration test: start a process-tier guest that echoes to stdout, verify log file contents.

---

## Step 3: Fix Linux MicroVM Config Mismatch (Gap C)

**Problem:** `launch_microvm_linux` passes `guest_images.oxydra_vm` as Firecracker `--config-file` (backend.rs:30-31), but this field defaults to `"oxydra-vm:latest"` — an OCI image tag, not a file path.

**Crates touched:** `types` (runner.rs), `runner` (backend.rs)

### 3.1 Add Firecracker-specific config fields to `RunnerGuestImages`

**File:** `crates/types/src/runner.rs`

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnerGuestImages {
    #[serde(default = "default_oxydra_vm_image")]
    pub oxydra_vm: String,       // OCI image ref for Docker/macOS
    #[serde(default = "default_shell_vm_image")]
    pub shell_vm: String,        // OCI image ref for Docker/macOS

    /// Firecracker JSON config file path for oxydra-vm (Linux microvm only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firecracker_oxydra_vm_config: Option<String>,

    /// Firecracker JSON config file path for shell-vm (Linux microvm only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firecracker_shell_vm_config: Option<String>,
}
```

### 3.2 Add preflight validation

**File:** `crates/types/src/runner.rs` (in `RunnerGuestImages::validate`)

No changes needed at the type level — these are optional. Validation happens at launch time (Step 3.3).

### 3.3 Update `launch_microvm_linux` to use the new fields

**File:** `crates/runner/src/backend.rs`

```rust
fn launch_microvm_linux(
    &self,
    request: &SandboxLaunchRequest,
) -> Result<SandboxLaunch, RunnerError> {
    // Validate Firecracker config paths are provided for Linux microvm
    let fc_oxydra_config = request.guest_images.firecracker_oxydra_vm_config
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| RunnerError::MissingFirecrackerConfig {
            field: "firecracker_oxydra_vm_config",
        })?;

    let runtime_api_socket = request.workspace.tmp.join("oxydra-vm-firecracker.sock");
    let runtime_command = RunnerCommandSpec::new(
        FIRECRACKER_BINARY,
        vec![
            "--api-sock".to_owned(),
            runtime_api_socket.to_string_lossy().into_owned(),
            "--config-file".to_owned(),
            fc_oxydra_config.to_owned(),  // Now a real file path
        ],
    );
    // ... rest unchanged ...
```

Do the same for the sidecar using `firecracker_shell_vm_config`.

### 3.4 Add new error variant

**File:** `crates/runner/src/lib.rs`

```rust
#[error("linux microvm requires `{field}` in [guest_images] config")]
MissingFirecrackerConfig { field: &'static str },
```

### 3.5 Update test fixtures

Update `write_runner_config_fixture` in `tests.rs` and `startup_integration.rs` to include the new fields when testing microvm-linux path.

### 3.6 Tests to add

- Verify `launch_microvm_linux` fails with clear error if `firecracker_oxydra_vm_config` is `None`.
- Verify `launch_microvm_linux` succeeds when the field is set to a valid path.
- Verify `launch_microvm_macos` and `launch_container` still use `oxydra_vm` (OCI tag) unaffected.

---

## Step 4: Container Bootstrap Delivery (Gap B — Docker)

**Problem:** Bootstrap envelope is only sent for `SandboxTier::Process` (lib.rs:162-164). Container guests never receive it. `send_startup_bootstrap` silently no-ops for `DockerContainer` lifecycle.

**Crates touched:** `runner` (lib.rs, backend.rs)

### 4.1 Write bootstrap envelope to file before container launch

**File:** `crates/runner/src/lib.rs` or `backend.rs`

```rust
fn write_bootstrap_file(
    workspace: &UserWorkspace,
    bootstrap: &RunnerBootstrapEnvelope,
) -> Result<PathBuf, RunnerError> {
    let bootstrap_dir = workspace.tmp.join("bootstrap");
    fs::create_dir_all(&bootstrap_dir).map_err(|source| RunnerError::ProvisionWorkspace {
        path: bootstrap_dir.clone(),
        source,
    })?;
    let bootstrap_path = bootstrap_dir.join("runner_bootstrap.json");
    let payload = serde_json::to_vec_pretty(bootstrap)
        .map_err(BootstrapEnvelopeError::from)?;
    fs::write(&bootstrap_path, &payload).map_err(|source| RunnerError::ProvisionWorkspace {
        path: bootstrap_path.clone(),
        source,
    })?;
    // Restrict permissions (owner-read-only)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&bootstrap_path,
            fs::Permissions::from_mode(0o400));
    }
    Ok(bootstrap_path)
}
```

### 4.2 Mount the bootstrap file into the container

**File:** `crates/runner/src/lib.rs` (in `launch_docker_container_async`)

Add the bootstrap file as a read-only bind mount:

```rust
// In the binds construction:
binds.push(format!(
    "{}:/run/oxydra/bootstrap:ro",
    bootstrap_path.to_string_lossy()
));
```

Add an environment variable telling the guest where to find it:

```rust
// In ContainerCreateBody:
env: Some(vec![
    "OXYDRA_BOOTSTRAP_FILE=/run/oxydra/bootstrap".to_owned(),
]),
```

### 4.3 Thread bootstrap path through to docker launch

Currently `launch_docker_container_async` doesn't receive the bootstrap. Add `bootstrap_file: Option<PathBuf>` parameter. Callers in `launch_container` and `launch_microvm_macos` pass it.

### 4.4 Remove the Process-only guard in `start_user_for_host`

**File:** `crates/runner/src/lib.rs` (line 162-164)

```rust
// Before:
if sandbox_tier == SandboxTier::Process {
    launch.launch.runtime.send_startup_bootstrap(&bootstrap)?;
}

// After:
match sandbox_tier {
    SandboxTier::Process => {
        launch.launch.runtime.send_startup_bootstrap(&bootstrap)?;
    }
    SandboxTier::Container | SandboxTier::MicroVm => {
        // Bootstrap delivered via mounted file (written before launch)
    }
}
```

The key change is that the bootstrap file is written *before* `self.backend.launch(...)` is called, so the container can read it at startup.

### 4.5 Restructure: write bootstrap file before launch for non-Process tiers

Move the bootstrap envelope creation and file writing to happen **before** `self.backend.launch(...)`:

```rust
// 1. Build bootstrap envelope (already happens before launch)
// 2. For non-Process tiers, write bootstrap file
let bootstrap_file = if sandbox_tier != SandboxTier::Process {
    Some(write_bootstrap_file(&workspace, &bootstrap)?)
} else {
    None
};

// 3. Pass bootstrap_file into SandboxLaunchRequest
let mut launch = self.backend.launch(SandboxLaunchRequest {
    // ... existing fields ...
    bootstrap_file,
})?;

// 4. For Process tier, send via stdin (existing behavior)
if sandbox_tier == SandboxTier::Process {
    launch.launch.runtime.send_startup_bootstrap(&bootstrap)?;
}
```

Add `bootstrap_file: Option<PathBuf>` to `SandboxLaunchRequest`.

### 4.6 Tests to add

- Verify bootstrap file is written with correct content for container tier.
- Verify bootstrap file is bind-mounted into the container (check docker create args).
- Verify `OXYDRA_BOOTSTRAP_FILE` env var is set on the container.
- Verify Process tier still uses stdin delivery (unchanged behavior).

---

## Step 5: Linux MicroVM Bootstrap Delivery (Gap B — Firecracker)

**Problem:** Firecracker microvms receive no bootstrap envelope. Need a delivery mechanism that works with the Firecracker process model.

**Crates touched:** `runner` (backend.rs)

### 5.1 Approach: Bootstrap via generated Firecracker config (boot_args)

Firecracker's `--config-file` accepts a JSON config with a `boot-source.boot_args` field. The bootstrap can be injected into the kernel command line.

### 5.2 Generate a per-launch Firecracker config

**File:** `crates/runner/src/backend.rs`

Instead of passing the user's template config directly to `--config-file`, generate a per-launch config:

```rust
fn generate_firecracker_config(
    template_path: &str,
    bootstrap: &RunnerBootstrapEnvelope,
    output_path: &Path,
) -> Result<(), RunnerError> {
    // 1. Read template config
    let template_content = fs::read_to_string(template_path)
        .map_err(|source| RunnerError::ReadConfig {
            path: PathBuf::from(template_path), source,
        })?;
    let mut config: serde_json::Value = serde_json::from_str(&template_content)
        .map_err(|source| RunnerError::ParseFirecrackerConfig {
            path: PathBuf::from(template_path), source,
        })?;

    // 2. Inject bootstrap into boot_args
    let bootstrap_json = serde_json::to_string(bootstrap)
        .map_err(BootstrapEnvelopeError::from)?;
    let bootstrap_b64 = base64_encode(&bootstrap_json);

    // Enforce cmdline size limit (kernel typically allows ~4096 bytes)
    if bootstrap_b64.len() > 2048 {
        return Err(RunnerError::BootstrapTooLargeForCmdline {
            bytes: bootstrap_b64.len(),
            max_bytes: 2048,
        });
    }

    // Append to existing boot_args
    if let Some(boot_source) = config.get_mut("boot-source") {
        if let Some(boot_args) = boot_source.get_mut("boot_args") {
            let existing = boot_args.as_str().unwrap_or("");
            *boot_args = serde_json::Value::String(
                format!("{existing} oxydra.bootstrap_b64={bootstrap_b64}")
            );
        }
    }

    // 3. Write generated config
    let output = serde_json::to_string_pretty(&config)
        .map_err(BootstrapEnvelopeError::from)?;
    fs::write(output_path, output)
        .map_err(|source| RunnerError::ProvisionWorkspace {
            path: output_path.to_path_buf(), source,
        })?;

    Ok(())
}
```

### 5.3 Update `launch_microvm_linux` to use generated config

**File:** `crates/runner/src/backend.rs`

```rust
fn launch_microvm_linux(
    &self,
    request: &SandboxLaunchRequest,
) -> Result<SandboxLaunch, RunnerError> {
    let fc_template = request.guest_images.firecracker_oxydra_vm_config
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| RunnerError::MissingFirecrackerConfig {
            field: "firecracker_oxydra_vm_config",
        })?;

    // Generate per-launch config with bootstrap injected
    let generated_config = request.workspace.tmp.join("firecracker-oxydra-vm.json");
    if let Some(bootstrap_file) = &request.bootstrap_file {
        // Read the bootstrap, inject into FC config
        let bootstrap_bytes = fs::read(bootstrap_file)?;
        let bootstrap: RunnerBootstrapEnvelope = serde_json::from_slice(&bootstrap_bytes)?;
        generate_firecracker_config(fc_template, &bootstrap, &generated_config)?;
    } else {
        // Copy template as-is (shouldn't happen if bootstrap_file is always set)
        fs::copy(fc_template, &generated_config)?;
    }

    let runtime_api_socket = request.workspace.tmp.join("oxydra-vm-firecracker.sock");
    let runtime_command = RunnerCommandSpec::new(
        FIRECRACKER_BINARY,
        vec![
            "--api-sock".to_owned(),
            runtime_api_socket.to_string_lossy().into_owned(),
            "--config-file".to_owned(),
            generated_config.to_string_lossy().into_owned(),
        ],
    );
    // ... rest unchanged ...
}
```

### 5.4 Fallback for large bootstrap payloads

If the bootstrap exceeds the 2KB cmdline limit:
- Fall back to a **virtio-blk mounted file** approach (mount a small ext4 image containing the bootstrap).
- Or use a **vsock channel** (guest connects to host vsock server on a well-known CID/port).
- **For now:** fail with a clear error. The bootstrap envelope in practice is small (user_id, paths, policy) — well under 2KB.

### 5.5 Guest-side contract (documentation only — no runner changes)

Document the contract for the guest init:
- Process tier: reads bootstrap from stdin (length-prefixed JSON, `--bootstrap-stdin` flag).
- Container tier: reads bootstrap from file at `$OXYDRA_BOOTSTRAP_FILE` (default `/run/oxydra/bootstrap`).
- Linux MicroVM tier: reads bootstrap from `/proc/cmdline` key `oxydra.bootstrap_b64=<base64>`.

### 5.6 Tests to add

- Verify `generate_firecracker_config` correctly injects `oxydra.bootstrap_b64` into `boot_args`.
- Verify generated config is valid JSON and preserves all template fields.
- Verify error when bootstrap exceeds cmdline size limit.
- Verify `launch_microvm_linux` uses the generated config path.

---

## Step 6: macOS MicroVM Bootstrap (Gap B — Docker-backed)

**Problem:** macOS microvm uses Docker containers internally. Same fix as Step 4.

**Crates touched:** `runner` (backend.rs)

### 6.1 Apply the same mounted-file approach as container tier

Since `launch_microvm_macos` calls `self.launch_docker_guest(...)`, which calls `launch_docker_container_async`, the bootstrap file mounting from Step 4 automatically applies.

**Ensure:**
- `bootstrap_file` from `SandboxLaunchRequest` is passed through `launch_docker_guest` → `launch_docker_container_async`.
- The env var `OXYDRA_BOOTSTRAP_FILE=/run/oxydra/bootstrap` is set.

### 6.2 Tests to add

- Verify macOS microvm receives bootstrap via mounted file (same assertions as container tier).

---

## Step 7: Control Plane Metadata for Phase 12

**Problem:** Phase 12 TUI needs to know log file locations and guest identifiers.

**Crates touched:** `types` (runner.rs), `runner` (lib.rs)

### 7.1 Extend `RunnerControlHealthStatus`

**File:** `crates/types/src/runner.rs`

```rust
pub struct RunnerControlHealthStatus {
    // ... existing fields ...
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_container_name: Option<String>,
}
```

### 7.2 Populate in `handle_control`

**File:** `crates/runner/src/lib.rs`

In the `HealthCheck` branch of `handle_control`, populate:
- `log_dir` from `self.workspace.logs.to_string_lossy()`
- `runtime_pid` from `self.launch.runtime.pid`
- `runtime_container_name` from the Docker container handle (if applicable)

### 7.3 Tests to add

- Verify health check response includes log_dir.
- Verify health check response includes runtime_pid for process-tier.

---

## Verification Gate

All of these must pass before the gaps are considered resolved:

1. **`cargo test -p runner`** — all existing + new unit tests pass.
2. **`cargo test -p runner --test startup_integration`** — all integration tests pass.
3. **`cargo test -p types`** — serialization/validation tests for new fields pass.
4. **Manual smoke test:**
   - `runner --daemon --user alice --insecure` stays alive and creates control socket.
   - Sending `HealthCheck` over the control socket returns healthy status with log_dir.
   - Sending `ShutdownUser` causes clean exit.
   - Guest stdout/stderr appears in `<workspace>/logs/`.
5. **`cargo deny check`**, **`cargo clippy`**, **`cargo fmt --check`** — all pass.

---

## Dependency Graph

```
Step 1 (A1: daemon)  ──────────────────────────────────────────────────┐
Step 2 (A2: log capture)  ─────────────────────────────────────────────┤
Step 3 (C: firecracker config) ──┬─────────────────────────────────────┤
                                 │                                     │
Step 4 (B: container bootstrap) ─┤                                     ├─→ Step 7 (metadata)
                                 │                                     │
Step 5 (B: linux microvm) ───────┘  (depends on Step 3)                │
Step 6 (B: macos microvm) ─────────(reuses Step 4 docker path) ───────┘
```

Steps 1, 2, 3, 4 can be worked in parallel (no data dependencies between them).
Step 5 depends on Step 3 (needs the new Firecracker config field).
Step 6 depends on Step 4 (reuses the Docker bootstrap file path).
Step 7 depends on Steps 1+2 (needs daemon + log dir to exist).

---

## Risk Mitigations

| Risk | Mitigation |
|------|------------|
| Docker detach silently drops logs | Use bollard's log streaming API (already a dependency) |
| Kernel cmdline size limit (~4KB) exceeded | Enforce 2KB max on base64 payload; fail with clear error |
| Firecracker config template drift | Validate generated JSON before launch; log the full config path |
| Shutdown hangs waiting for log pumps | Kill child process first, then join log tasks with 2s timeout |
| Stale control socket after crash | Remove socket file at startup; create in user's tmp dir |
| Bootstrap file contains sensitive paths | Write with mode 0o400; mount read-only in containers |
