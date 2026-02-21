# Gap 4 Resolution Plan: Real WASM Tool Isolation via `wasmtime` + WASI

## Current State

`HostWasmToolRunner` enforces capability profiles (mount boundaries, read-only/read-write) via
**host-side `std::fs` calls** with path canonicalization checks. This is logically correct but not
hardware-enforced — a TOCTOU race between canonicalization and file access could theoretically
bypass checks.

## Target State

File tool operations execute inside a `wasmtime` WASM sandbox with **WASI preopened directories**
as the enforcement boundary. The guest physically cannot access paths outside its preopened mounts.
Web tools remain host-side (they have no FS access by design).

## Architecture Overview

```
┌─────────────────────────────────────────────────────┐
│  WasmWasiToolRunner (implements WasmToolRunner)      │
│                                                      │
│  file ops ──► wasmtime Engine ──► WASM Guest Module  │
│               │  Store + WASI ctx                    │
│               │  preopened dirs per profile           │
│               │  stdin(args) → stdout(result)         │
│                                                      │
│  web ops  ──► host reqwest (unchanged)               │
└─────────────────────────────────────────────────────┘
```

## What Stays Unchanged

- `WasmToolRunner` trait signature
- `WasmCapabilityProfile` enum and `mount_profile` logic
- `WasmWorkspaceMounts` struct
- `WasmMount` / `WasmInvocationMetadata` / `WasmInvocationResult` types
- `SecurityPolicy` and `WorkspaceSecurityPolicy`
- All tool implementations in `crates/tools/` (they call `invoke_wasm_tool` which routes through
  the trait)
- Web egress policy and SSRF protection
- Tool registry and execution flow

---

## Step-by-Step Implementation Plan

### Step 1: Create WASM Guest Crate (`crates/wasm-guest/`)

**Purpose:** A minimal Rust binary that compiles to `wasm32-wasip1`, implementing all file
operations using only WASI filesystem APIs.

**Files to create:**
- `crates/wasm-guest/Cargo.toml` — target `wasm32-wasip1`, dependencies: `serde_json` only
- `crates/wasm-guest/src/main.rs` — entry point: reads JSON `{op, args}` from stdin, dispatches to
  operation handler, writes JSON result to stdout

**Operations implemented in the guest:**
- `file_read` — `std::fs::read_to_string` (WASI-backed, scoped to preopened dirs)
- `file_write` — `std::fs::write`
- `file_edit` — read + `replacen` + write (same logic as `HostWasmToolRunner::file_edit`)
- `file_delete` — `std::fs::remove_file` / `std::fs::remove_dir_all`
- `file_list` — `std::fs::read_dir`
- `file_search` — recursive search (same logic as `collect_search_matches`)
- `vault_copyto_read` — `std::fs::read_to_string` (WASI limits to vault preopen)
- `vault_copyto_write` — `std::fs::write` (WASI limits to shared/tmp preopens)

**Stdin/stdout protocol:**

```
stdin:  {"op": "file_read", "args": {"path": "/shared/foo.txt"}}
stdout: {"ok": "file contents here"}
   OR:  {"err": "failed to read: No such file"}
exit 0 on success, exit 1 on operation error
```

Inside the guest, paths are guest-relative (e.g., `/shared/foo.txt` maps to the preopened
directory labeled `"shared"`). The guest uses `std::fs` which routes through WASI — it cannot
escape preopened boundaries regardless of what paths it constructs.

**`Cargo.toml`:**

```toml
[package]
name = "wasm-guest"
version = "0.1.0"
edition = "2024"

[[bin]]
name = "oxydra_wasm_guest"
path = "src/main.rs"

[dependencies]
serde_json = "1"
serde = { version = "1", features = ["derive"] }

[profile.release]
opt-level = "z"
strip = true
lto = true
```

### Step 2: Build Infrastructure for the Guest Module

**Approach:** Compile `wasm-guest` as a build step and embed the resulting `.wasm` binary via
`include_bytes!`.

Add a `build.rs` to `crates/sandbox/` that:
1. Invokes `cargo build --target wasm32-wasip1 -p wasm-guest --release`
2. Copies the output `.wasm` to `crates/sandbox/guest/oxydra_wasm_guest.wasm`
3. Emits `cargo:rerun-if-changed=../wasm-guest/src/` to trigger rebuild when guest source changes

In `crates/sandbox/src/wasm_wasi_runner.rs`:

```rust
const GUEST_WASM: &[u8] = include_bytes!("../guest/oxydra_wasm_guest.wasm");
```

**Alternative for initial development:** Ship a pre-compiled `.wasm` artifact in the repo and add
a CI step that rebuilds + verifies it matches. This avoids build.rs complexity with
cross-compilation toolchain requirements. Recommended as the starting point.

**CI requirement:** `rustup target add wasm32-wasip1` must be installed in CI.

### Step 3: Add `wasmtime` Dependencies (Feature-Gated)

**File:** `crates/sandbox/Cargo.toml`

```toml
[features]
default = ["wasm-isolation"]
wasm-isolation = ["dep:wasmtime", "dep:wasmtime-wasi"]

[dependencies]
wasmtime = { version = "29", optional = true }
wasmtime-wasi = { version = "29", optional = true }
# ... existing dependencies unchanged
```

Feature-gating keeps the ~15 MB `wasmtime` dependency optional. In `--insecure`/Process tier, the
existing `HostWasmToolRunner` remains available without it.

### Step 4: Implement `WasmWasiToolRunner`

**New file:** `crates/sandbox/src/wasm_wasi_runner.rs`

**Struct definition:**

```rust
pub struct WasmWasiToolRunner {
    engine: wasmtime::Engine,
    module: wasmtime::Module,
    mounts: WasmWorkspaceMounts,
    request_counter: AtomicU64,
    audit_records: Mutex<Vec<WasmInvocationMetadata>>,
    http_client: Client,  // For web tools (host-side, unchanged)
}
```

**Constructor:** Pre-compiles the WASM module once at startup (expensive) and reuses the `Engine` +
`Module` across invocations. Only `Store` + WASI context are per-invocation (cheap).

```rust
impl WasmWasiToolRunner {
    pub fn new(mounts: WasmWorkspaceMounts) -> Result<Self, SandboxError> {
        let mut config = wasmtime::Config::new();
        config.epoch_interruption(true);
        config.max_wasm_stack(1 << 20);  // 1 MB stack

        let engine = wasmtime::Engine::new(&config)?;
        let module = wasmtime::Module::new(&engine, GUEST_WASM)?;

        let http_client = Client::builder()
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;

        Ok(Self {
            engine,
            module,
            mounts,
            request_counter: AtomicU64::new(0),
            audit_records: Mutex::new(Vec::new()),
            http_client,
        })
    }

    pub fn for_bootstrap_workspace(workspace_root: impl AsRef<Path>) -> Result<Self, SandboxError> {
        Self::new(WasmWorkspaceMounts::for_bootstrap_workspace(workspace_root))
    }
}
```

### Step 5: WASI Preopened Directory Configuration Per Profile

The core security enforcement. Each `WasmCapabilityProfile` maps to a set of WASI preopened
directories with explicit permissions:

```rust
fn configure_wasi_for_profile(
    &self,
    profile: WasmCapabilityProfile,
) -> Result<WasiCtxBuilder, String> {
    let mut builder = WasiCtxBuilder::new();
    builder.inherit_stderr();  // Surface guest errors in host logs

    for mount in profile.mount_profile(&self.mounts) {
        let (dir_perms, file_perms) = if mount.read_only {
            (DirPerms::READ, FilePerms::READ)
        } else {
            (DirPerms::READ | DirPerms::MUTATE, FilePerms::READ | FilePerms::WRITE)
        };

        let host_dir = Dir::open_ambient_dir(&mount.path, ambient_authority())
            .map_err(|e| format!("failed to open mount `{}`: {e}", mount.path.display()))?;

        builder.preopened_dir(host_dir, dir_perms, file_perms, mount.label)
            .map_err(|e| format!("failed to preopen `{}`: {e}", mount.label))?;
    }

    Ok(builder)
}
```

**Profile → WASI preopens mapping:**

| Profile | Preopened Dirs | Permissions |
|---------|---------------|-------------|
| `FileReadOnly` | shared, tmp, vault | All read-only |
| `FileReadWrite` | shared, tmp | Read-write |
| `Web` | (none) | No FS access |
| `VaultReadStep` | vault | Read-only |
| `VaultWriteStep` | shared, tmp | Read-write |

### Step 6: Host-Side Path Translation

Before passing arguments to the WASM guest, the host rewrites absolute host paths to
guest-relative paths (relative to the preopened directory label):

```rust
fn translate_path_for_guest(
    host_path: &str,
    mounts: &[WasmMount],
) -> Result<String, String> {
    let canonical = canonicalize_invocation_path(host_path, true)?;
    for mount in mounts {
        let canonical_mount = fs::canonicalize(&mount.path)
            .unwrap_or_else(|_| mount.path.clone());
        if let Ok(relative) = canonical.strip_prefix(&canonical_mount) {
            // Guest path: /shared/subdir/file.txt
            let guest_path = Path::new("/")
                .join(mount.label)
                .join(relative);
            return Ok(guest_path.to_string_lossy().into_owned());
        }
    }
    Err(format!(
        "path `{host_path}` is not within any preopened mount"
    ))
}
```

For arguments with path fields, translate before serializing to stdin. This keeps the guest simple
and unaware of host filesystem layout.

### Step 7: Implement the WASM Invocation Flow

```rust
async fn execute_in_wasm(
    &self,
    tool_name: &str,
    profile: WasmCapabilityProfile,
    arguments: &Value,
) -> Result<String, String> {
    // 1. Translate host paths to guest-relative paths
    let mounts = profile.mount_profile(&self.mounts);
    let translated_args = translate_arguments(tool_name, arguments, &mounts)?;

    // 2. Serialize invocation as JSON for guest stdin
    let input = serde_json::to_vec(&json!({
        "op": tool_name,
        "args": translated_args,
    }))
    .map_err(|e| format!("failed to serialize args: {e}"))?;

    // 3. Build WASI context with preopened dirs
    let wasi_ctx = self.configure_wasi_for_profile(profile)?;
    let stdin = MemoryInputPipe::new(input);
    let stdout = MemoryOutputPipe::new(MAX_GUEST_OUTPUT_BYTES);
    wasi_ctx.stdin(stdin).stdout(stdout.clone());

    // 4. Apply memory limits
    let limits = StoreLimitsBuilder::new()
        .memory_size(64 * 1024 * 1024)  // 64 MB
        .instances(1)
        .build();

    // 5. Create store, link WASI, instantiate
    let mut store = Store::new(&self.engine, wasi_ctx.build());
    store.limiter(|_| &limits);
    store.set_epoch_deadline(100);  // Interrupt after ~10s

    let mut linker: Linker<WasiCtx> = Linker::new(&self.engine);
    wasmtime_wasi::add_to_linker_sync(&mut linker)
        .map_err(|e| format!("failed to link WASI: {e}"))?;

    let instance = linker
        .instantiate(&mut store, &self.module)
        .map_err(|e| format!("failed to instantiate guest: {e}"))?;

    // 6. Execute guest
    let start = instance
        .get_typed_func::<(), ()>(&mut store, "_start")
        .map_err(|e| format!("guest missing _start: {e}"))?;

    match start.call(&mut store, ()) {
        Ok(()) => {}
        Err(trap) => {
            // Epoch interruption surfaces as a trap
            return Err(format!("guest execution failed: {trap}"));
        }
    }

    // 7. Parse output
    let output_bytes = stdout.contents();
    let result: GuestResult = serde_json::from_slice(&output_bytes)
        .map_err(|e| format!("failed to parse guest output: {e}"))?;

    match result {
        GuestResult::Ok { ok } => Ok(ok),
        GuestResult::Err { err } => Err(err),
    }
}
```

### Step 8: Web Tools Bypass (Host-Side, Unchanged)

Web tools (`web_fetch`, `web_search`) have no FS access by design and continue executing on the
host via `reqwest`. The `WasmToolRunner` implementation dispatches based on operation name:

```rust
#[async_trait]
impl WasmToolRunner for WasmWasiToolRunner {
    async fn invoke(
        &self,
        tool_name: &str,
        profile: WasmCapabilityProfile,
        arguments: &Value,
        operation_id: Option<&str>,
    ) -> Result<WasmInvocationResult, SandboxError> {
        let request_id = self.next_request_id(tool_name);
        let metadata = WasmInvocationMetadata {
            request_id: request_id.clone(),
            operation_id: operation_id.map(ToOwned::to_owned),
            tool_name: tool_name.to_owned(),
            profile,
            mounts: profile.mount_profile(&self.mounts),
        };
        self.record_audit(metadata.clone());

        // Retain host-side capability profile checks as defense-in-depth.
        // They catch misuse early and produce clear diagnostics before WASM
        // instantiation cost is paid.
        self.enforce_capability_profile(tool_name, profile, arguments)
            .map_err(|message| SandboxError::WasmInvocationFailed {
                tool: tool_name.to_owned(),
                request_id: request_id.clone(),
                message,
            })?;

        let output = match tool_name {
            "web_fetch" => {
                crate::web_fetch::execute(&self.http_client, arguments).await
            }
            "web_search" => {
                crate::web_search::execute(&self.http_client, arguments).await
            }
            _ => {
                self.execute_in_wasm(tool_name, profile, arguments).await
            }
        }
        .map_err(|message| SandboxError::WasmInvocationFailed {
            tool: tool_name.to_owned(),
            request_id: request_id.clone(),
            message,
        })?;

        Ok(WasmInvocationResult { metadata, output })
    }

    fn audit_log(&self) -> Vec<WasmInvocationMetadata> {
        self.audit_records
            .lock()
            .map(|records| records.clone())
            .unwrap_or_default()
    }
}
```

**Note:** The existing `enforce_capability_profile` checks are **retained** as defense-in-depth.
Even though the WASM sandbox provides the hard boundary, host-side checks catch misuse early and
produce clear error messages without paying the WASM instantiation cost.

### Step 9: Epoch Interruption Background Thread

Epoch-based interruption requires a background thread that periodically increments the engine
epoch. Add this to the `WasmWasiToolRunner` constructor:

```rust
// Spawn epoch ticker: increments every 100ms
// 100 ticks (set_epoch_deadline(100)) ≈ 10 second timeout
let engine_clone = engine.clone();
std::thread::spawn(move || loop {
    std::thread::sleep(Duration::from_millis(100));
    engine_clone.increment_epoch();
});
```

This provides the timeout mechanism without requiring `tokio::time::timeout` to cancel an
async-unaware WASM execution.

### Step 10: Wire Into Runner/Runtime

**File:** `crates/sandbox/src/lib.rs` — Export the new runner conditionally:

```rust
#[cfg(feature = "wasm-isolation")]
mod wasm_wasi_runner;
#[cfg(feature = "wasm-isolation")]
pub use wasm_wasi_runner::WasmWasiToolRunner;
```

**File:** `crates/runner/src/` or wherever tools are constructed — Select runner at startup:

```rust
let wasm_runner: Arc<dyn WasmToolRunner> = {
    #[cfg(feature = "wasm-isolation")]
    if sandbox_tier != SandboxTier::Process {
        match WasmWasiToolRunner::for_bootstrap_workspace(&workspace_root) {
            Ok(runner) => Arc::new(runner),
            Err(e) => {
                tracing::warn!("failed to initialize WASM runner: {e}; falling back to host runner");
                Arc::new(HostWasmToolRunner::for_bootstrap_workspace(&workspace_root))
            }
        }
    } else {
        Arc::new(HostWasmToolRunner::for_bootstrap_workspace(&workspace_root))
    }
    #[cfg(not(feature = "wasm-isolation"))]
    Arc::new(HostWasmToolRunner::for_bootstrap_workspace(&workspace_root))
};
```

No changes needed to the `WasmToolRunner` trait, tool implementations in `crates/tools/`, or tool
registry — the abstraction boundary is clean.

### Step 11: Testing

**Unit tests in `wasm_wasi_runner.rs`** (mirror the existing `HostWasmToolRunner` tests, plus
WASM-specific cases):

1. File read through WASM returns correct content from preopened dir
2. File write through WASM creates file within allowed mount
3. File read of a path outside preopened dirs fails with WASI permission error (not just
   host-side check — verifies the WASM boundary is enforced)
4. Read-only profile (`FileReadOnly`) prevents write operations at the WASM level even if the
   host-side profile check is somehow bypassed
5. Profile mismatch rejected before WASM instantiation
6. Vault two-step: `VaultReadStep` can access vault dir, `VaultWriteStep` can access shared/tmp
   but not vault
7. Web tools (`web_fetch`, `web_search`) execute on host with same behavior as `HostWasmToolRunner`
8. Epoch interruption fires and surfaces as `SandboxError::WasmInvocationFailed` for runaway guests
9. Memory limit enforcement traps when guest allocates beyond limit
10. Audit metadata is recorded for every WASM invocation

**Behavioral parity test:**
Run each file operation through both `HostWasmToolRunner` and `WasmWasiToolRunner` on the same
inputs and assert identical results. This guards against regressions when swapping runners.

**Integration tests:**
Full tool invocation through `ToolRegistry` → `WasmWasiToolRunner` → WASM guest → result, for
each file tool.

### Step 12: CI Pipeline Updates

Add to CI matrix:

```yaml
- name: Install wasm32-wasip1 target
  run: rustup target add wasm32-wasip1

- name: Compile WASM guest module
  run: cargo build --target wasm32-wasip1 -p wasm-guest --release

- name: Verify embedded WASM artifact is current
  run: |
    # Compare fresh build against committed artifact
    diff crates/sandbox/guest/oxydra_wasm_guest.wasm \
      target/wasm32-wasip1/release/oxydra_wasm_guest.wasm

- name: Test sandbox with WASM isolation enabled
  run: cargo test -p sandbox --features wasm-isolation

- name: Test sandbox with WASM isolation disabled (fallback path)
  run: cargo test -p sandbox --no-default-features
```

---

## Security Properties Gained

| Threat | Before (Host) | After (WASM) |
|--------|--------------|-------------|
| TOCTOU race (path check → file access gap) | Vulnerable | Eliminated — guest cannot access unmounted paths |
| Symlink escape after canonicalization | Vulnerable window | Eliminated — WASI denies traversal beyond preopens |
| Guest reads host process memory | N/A (same process) | Impossible — WASM linear memory isolation |
| Guest opens arbitrary file descriptors | Possible | Only preopened FDs available to guest |
| Runaway computation / CPU burn | Timeout only | Epoch interruption + memory limit |
| Unbounded memory allocation | Unconstrained | `StoreLimitsBuilder` enforces hard cap |

---

## Estimated Scope

| Component | Lines |
|-----------|-------|
| `crates/wasm-guest/src/main.rs` (new) | ~300-400 |
| `crates/sandbox/src/wasm_wasi_runner.rs` (new) | ~400-500 |
| `crates/sandbox/build.rs` (new) | ~50-80 |
| `crates/sandbox/Cargo.toml` modifications | ~10 |
| `crates/sandbox/src/lib.rs` modifications | ~10 |
| Runner wiring modifications | ~20-30 |
| Tests | ~300-400 |
| **Total** | **~1,100-1,430 lines** |

---

## Dependencies on Other Gaps

This gap is **independent** of Gaps 1, 2, 3, and 5. It targets the sandbox crate only and does not
require provider streaming, TUI rendering, memory upserts, or the Responses API provider.

Per the build plan's gap resolution table, this gap should be resolved **before Phase 15**
(multi-agent orchestration), which needs hard isolation boundaries for subagent execution.
