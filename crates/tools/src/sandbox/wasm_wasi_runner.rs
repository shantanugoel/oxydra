//! `WasmWasiToolRunner` — hardware-enforced file tool isolation via wasmtime + WASI.
//!
//! File tool operations execute inside a `wasmtime` WASM sandbox. WASI preopened
//! directories are the enforcement boundary: the guest physically cannot access
//! paths outside its preopened mounts regardless of the paths it constructs.
//!
//! Web tools (`web_fetch`, `web_search`) continue to run host-side via `reqwest`
//! because they have no filesystem access by design and are unaffected by TOCTOU.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use reqwest::Client;
use serde_json::Value;
use wasmtime::{Config, Engine, Linker, Module, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{
    DirPerms, FilePerms, WasiCtxBuilder,
    p1::{WasiP1Ctx, add_to_linker_sync},
    p2::pipe::{MemoryInputPipe, MemoryOutputPipe},
};

use super::{
    SandboxError,
    wasm_runner::{
        WasmCapabilityProfile, WasmInvocationMetadata, WasmInvocationResult, WasmMount,
        WasmToolRunner, WasmWorkspaceMounts,
    },
};

/// WASM guest binary compiled from `crates/wasm-guest`, embedded at build time.
const GUEST_WASM: &[u8] = include_bytes!("../../guest/oxydra_wasm_guest.wasm");

/// Maximum bytes the guest may write to stdout (16 MB).
const MAX_GUEST_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

/// Store data combining the WASI context and resource limiter.
struct StoreData {
    wasi: WasiP1Ctx,
    limits: StoreLimits,
}

/// WASM/WASI tool runner providing hardware-enforced filesystem isolation.
///
/// File operations run inside a `wasmtime` sandbox with WASI preopened directories
/// as the hard security boundary. Web tools continue to execute host-side.
pub struct WasmWasiToolRunner {
    engine: Engine,
    module: Module,
    mounts: WasmWorkspaceMounts,
    request_counter: AtomicU64,
    audit_records: Mutex<Vec<WasmInvocationMetadata>>,
    http_client: Client,
}

impl WasmWasiToolRunner {
    /// Create a new runner for the given workspace mount configuration.
    ///
    /// Pre-compiles the WASM guest module once (expensive) and reuses the
    /// `Engine` + `Module` across invocations. Only the per-invocation `Store`
    /// and WASI context are created fresh each time (cheap).
    pub fn new(mounts: WasmWorkspaceMounts) -> Result<Self, SandboxError> {
        let mut config = Config::new();
        config.epoch_interruption(true);
        config.max_wasm_stack(1 << 20); // 1 MB stack

        let engine = Engine::new(&config).map_err(|e| SandboxError::WasmInvocationFailed {
            tool: "init".to_owned(),
            request_id: "init".to_owned(),
            message: format!("failed to create WASM engine: {e}"),
        })?;

        let module =
            Module::new(&engine, GUEST_WASM).map_err(|e| SandboxError::WasmInvocationFailed {
                tool: "init".to_owned(),
                request_id: "init".to_owned(),
                message: format!("failed to compile WASM guest module: {e}"),
            })?;

        let http_client = Client::builder()
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_else(|_| Client::new());

        // Spawn epoch ticker: increments every 100 ms.
        // set_epoch_deadline(100) → ~10 second execution timeout.
        let engine_clone = engine.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(100));
                engine_clone.increment_epoch();
            }
        });

        Ok(Self {
            engine,
            module,
            mounts,
            request_counter: AtomicU64::new(0),
            audit_records: Mutex::new(Vec::new()),
            http_client,
        })
    }

    /// Create a runner using the bootstrap workspace layout
    /// (`workspace_root/{shared,tmp,vault}/`).
    pub fn for_bootstrap_workspace(workspace_root: impl AsRef<Path>) -> Result<Self, SandboxError> {
        Self::new(WasmWorkspaceMounts::for_bootstrap_workspace(workspace_root))
    }

    fn next_request_id(&self, tool_name: &str) -> String {
        let n = self.request_counter.fetch_add(1, Ordering::Relaxed) + 1;
        format!("{tool_name}-{n}")
    }

    fn record_audit(&self, metadata: WasmInvocationMetadata) {
        if let Ok(mut records) = self.audit_records.lock() {
            records.push(metadata);
        }
    }

    /// Translate an absolute host path to a guest-relative path
    /// (e.g. `/ws/shared/foo.txt` → `/shared/foo.txt`).
    fn translate_path_for_guest(host_path: &str, mounts: &[WasmMount]) -> Result<String, String> {
        let raw = host_path.trim();
        if raw.is_empty() {
            return Err("path argument must not be empty".to_owned());
        }

        let path = PathBuf::from(raw);
        let canonical = canonicalize_path_or_parent(&path)?;

        for mount in mounts {
            let canonical_mount =
                fs::canonicalize(&mount.path).unwrap_or_else(|_| mount.path.clone());
            if let Ok(relative) = canonical.strip_prefix(&canonical_mount) {
                let guest_path = Path::new("/").join(mount.label).join(relative);
                return Ok(guest_path.to_string_lossy().into_owned());
            }
        }

        Err(format!(
            "path `{host_path}` is not within any preopened mount"
        ))
    }

    /// Rewrite path-valued arguments from absolute host paths to guest-relative paths.
    fn translate_arguments(
        tool_name: &str,
        arguments: &Value,
        mounts: &[WasmMount],
    ) -> Result<Value, String> {
        let mut args = arguments.clone();

        match tool_name {
            "file_read" | "file_read_bytes" | "file_write" | "file_edit" | "file_delete"
            | "file_search" => {
                if let Some(path) = arguments.get("path").and_then(Value::as_str) {
                    let guest_path = Self::translate_path_for_guest(path, mounts)?;
                    args["path"] = Value::String(guest_path);
                }
            }
            "file_list" => {
                if let Some(path) = arguments.get("path").and_then(Value::as_str) {
                    let guest_path = Self::translate_path_for_guest(path, mounts)?;
                    args["path"] = Value::String(guest_path);
                }
                // No path defaults to "/shared" in the guest.
            }
            "vault_copyto_read" => {
                if let Some(path) = arguments.get("source_path").and_then(Value::as_str) {
                    let guest_path = Self::translate_path_for_guest(path, mounts)?;
                    args["source_path"] = Value::String(guest_path);
                }
            }
            "vault_copyto_write" => {
                if let Some(path) = arguments.get("destination_path").and_then(Value::as_str) {
                    let guest_path = Self::translate_path_for_guest(path, mounts)?;
                    args["destination_path"] = Value::String(guest_path);
                }
            }
            _ => {}
        }

        Ok(args)
    }

    /// Execute a file operation inside the WASM sandbox and return the result string.
    ///
    /// The actual WASM execution runs in `spawn_blocking` because
    /// `add_to_linker_sync` from wasmtime-wasi internally drives async WASI I/O
    /// with its own tokio runtime, which conflicts with calling it from within an
    /// existing tokio context.
    async fn execute_in_wasm(
        &self,
        tool_name: &str,
        profile: WasmCapabilityProfile,
        arguments: &Value,
    ) -> Result<String, String> {
        // 1. Translate host paths to guest-relative paths (on the async thread).
        let mounts = profile.mount_profile(&self.mounts);
        let translated_args = Self::translate_arguments(tool_name, arguments, &mounts)?;

        // 2. Serialize invocation as JSON for guest stdin.
        let input = serde_json::to_vec(&serde_json::json!({
            "op": tool_name,
            "args": translated_args,
        }))
        .map_err(|e| format!("failed to serialize args: {e}"))?;

        // Prepare mount descriptors for the blocking thread
        // (PathBuf, guest_label, read_only)
        let mount_descriptors: Vec<(PathBuf, &'static str, bool)> = mounts
            .iter()
            .map(|m| (m.path.clone(), m.label, m.read_only))
            .collect();

        // Clone engine/module (Arc-backed — cheap).
        let engine = self.engine.clone();
        let module = self.module.clone();

        // 3. Run blocking WASM execution on a dedicated thread so that
        //    wasmtime-wasi can create its own tokio runtime without conflicting
        //    with the caller's runtime.
        tokio::task::spawn_blocking(move || {
            execute_guest_sync(&engine, &module, &input, &mount_descriptors)
        })
        .await
        .map_err(|e| format!("wasm blocking task panicked: {e}"))?
    }

    /// Host-side capability profile check (defense-in-depth).
    ///
    /// These checks are retained even though WASI preopens provide the hard
    /// boundary. They catch misuse early and produce clear diagnostics without
    /// paying the WASM instantiation cost.
    fn enforce_capability_profile(
        &self,
        tool_name: &str,
        profile: WasmCapabilityProfile,
    ) -> Result<(), String> {
        let expected = expected_capability_profile(tool_name)
            .ok_or_else(|| format!("unknown wasm tool operation `{tool_name}`"))?;
        if expected != profile {
            return Err(format!(
                "tool `{tool_name}` requires capability profile `{expected:?}`, got `{profile:?}`"
            ));
        }
        Ok(())
    }
}

/// Execute the WASM guest synchronously on the current thread.
///
/// This function must be called from a thread that does **not** have an active
/// tokio runtime (e.g. via `tokio::task::spawn_blocking`), because
/// `add_to_linker_sync` drives async WASI syscalls using its own internal runtime.
fn execute_guest_sync(
    engine: &Engine,
    module: &Module,
    input: &[u8],
    // (host_path, guest_label, read_only) per mount
    mounts: &[(PathBuf, &'static str, bool)],
) -> Result<String, String> {
    // Build WASI context with preopened directories.
    let mut wasi_builder = WasiCtxBuilder::new();
    wasi_builder.inherit_stderr();

    for (host_path, label, read_only) in mounts {
        let (dir_perms, file_perms) = if *read_only {
            (DirPerms::READ, FilePerms::READ)
        } else {
            (DirPerms::all(), FilePerms::all())
        };
        let guest_path = format!("/{label}");
        wasi_builder
            .preopened_dir(host_path, &guest_path, dir_perms, file_perms)
            .map_err(|e| {
                format!(
                    "failed to preopen `{}` → `{guest_path}`: {e}",
                    host_path.display()
                )
            })?;
    }

    let stdin = MemoryInputPipe::new(input.to_vec());
    let stdout = MemoryOutputPipe::new(MAX_GUEST_OUTPUT_BYTES);
    wasi_builder.stdin(stdin).stdout(stdout.clone());
    let wasi_p1_ctx = wasi_builder.build_p1();

    // Apply memory limits: 64 MB heap, 1 instance.
    let limits = StoreLimitsBuilder::new()
        .memory_size(64 * 1024 * 1024)
        .instances(1)
        .build();

    let mut store = Store::new(
        engine,
        StoreData {
            wasi: wasi_p1_ctx,
            limits,
        },
    );
    store.limiter(|data| &mut data.limits);
    store.set_epoch_deadline(100); // 100 ticks × 100 ms = ~10 s timeout

    let mut linker: Linker<StoreData> = Linker::new(engine);
    add_to_linker_sync(&mut linker, |data| &mut data.wasi)
        .map_err(|e| format!("failed to link WASI: {e}"))?;

    let instance = linker
        .instantiate(&mut store, module)
        .map_err(|e| format!("failed to instantiate guest: {e}"))?;

    let start = instance
        .get_typed_func::<(), ()>(&mut store, "_start")
        .map_err(|e| format!("guest missing _start: {e}"))?;

    match start.call(&mut store, ()) {
        Ok(()) => {}
        Err(trap) => {
            return Err(format!("guest execution failed: {trap}"));
        }
    }

    let output_bytes = stdout.contents();
    let result: serde_json::Value = serde_json::from_slice(&output_bytes)
        .map_err(|e| format!("failed to parse guest output: {e}"))?;

    if let Some(ok) = result.get("ok").and_then(serde_json::Value::as_str) {
        Ok(ok.to_owned())
    } else if let Some(err) = result.get("err").and_then(serde_json::Value::as_str) {
        Err(err.to_owned())
    } else {
        Err(format!("unexpected guest output format: {result}"))
    }
}

fn expected_capability_profile(tool_name: &str) -> Option<WasmCapabilityProfile> {
    match tool_name {
        "file_read" | "file_read_bytes" | "file_search" | "file_list" => {
            Some(WasmCapabilityProfile::FileReadOnly)
        }
        "file_write" | "file_edit" | "file_delete" => Some(WasmCapabilityProfile::FileReadWrite),
        "web_fetch" | "web_search" => Some(WasmCapabilityProfile::Web),
        "vault_copyto_read" => Some(WasmCapabilityProfile::VaultReadStep),
        "vault_copyto_write" => Some(WasmCapabilityProfile::VaultWriteStep),
        _ => None,
    }
}

/// Canonicalize a path, walking up to the nearest existing ancestor if the
/// leaf does not yet exist (e.g. a file about to be created).
fn canonicalize_path_or_parent(path: &Path) -> Result<PathBuf, String> {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };

    if abs.exists() {
        return fs::canonicalize(&abs)
            .map_err(|e| format!("failed to canonicalize `{}`: {e}", abs.display()));
    }

    let mut ancestor = abs.as_path();
    loop {
        match ancestor.parent() {
            Some(p) => ancestor = p,
            None => {
                return Err(format!(
                    "path `{}` has no canonicalizable ancestor",
                    abs.display()
                ));
            }
        }
        if ancestor.exists() {
            break;
        }
    }

    let canonical_ancestor = fs::canonicalize(ancestor)
        .map_err(|e| format!("failed to canonicalize `{}`: {e}", ancestor.display()))?;

    let suffix = abs.strip_prefix(ancestor).map_err(|e| {
        format!(
            "failed to compute relative path from `{}` to `{}`: {e}",
            ancestor.display(),
            abs.display()
        )
    })?;

    Ok(canonical_ancestor.join(suffix))
}

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

        // Retain host-side capability profile check as defense-in-depth.
        self.enforce_capability_profile(tool_name, profile)
            .map_err(|message| SandboxError::WasmInvocationFailed {
                tool: tool_name.to_owned(),
                request_id: request_id.clone(),
                message,
            })?;

        let output = match tool_name {
            "web_fetch" => super::web_fetch::execute(&self.http_client, arguments).await,
            "web_search" => super::web_search::execute(&self.http_client, arguments).await,
            _ => self.execute_in_wasm(tool_name, profile, arguments).await,
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

#[cfg(test)]
mod tests {
    use std::{
        env, fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    use base64::Engine;
    use serde_json::json;

    use super::super::wasm_runner::{SHARED_DIR_NAME, TMP_DIR_NAME, VAULT_DIR_NAME};
    use super::*;

    fn unique_workspace(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after unix epoch")
            .as_nanos();
        let root = env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(root.join(SHARED_DIR_NAME)).expect("shared dir should be created");
        fs::create_dir_all(root.join(TMP_DIR_NAME)).expect("tmp dir should be created");
        fs::create_dir_all(root.join(VAULT_DIR_NAME)).expect("vault dir should be created");
        root
    }

    fn make_runner(workspace_root: &Path) -> WasmWasiToolRunner {
        WasmWasiToolRunner::for_bootstrap_workspace(workspace_root)
            .expect("WasmWasiToolRunner should initialize")
    }

    #[tokio::test]
    async fn wasm_runner_reads_file_from_preopened_dir() {
        let workspace_root = unique_workspace("wasi-read");
        let shared_file = workspace_root.join(SHARED_DIR_NAME).join("hello.txt");
        fs::write(&shared_file, "wasi-content").expect("test file should be writable");

        let runner = make_runner(&workspace_root);
        let result = runner
            .invoke(
                "file_read",
                WasmCapabilityProfile::FileReadOnly,
                &json!({ "path": shared_file.to_string_lossy() }),
                Some("op-read"),
            )
            .await
            .expect("file_read should succeed");

        assert_eq!(result.output, "wasi-content");
        assert!(result.metadata.request_id.starts_with("file_read-"));
        assert_eq!(result.metadata.operation_id.as_deref(), Some("op-read"));
        assert_eq!(result.metadata.profile, WasmCapabilityProfile::FileReadOnly);

        let _ = fs::remove_dir_all(workspace_root);
    }

    #[tokio::test]
    async fn wasm_runner_reads_binary_file_bytes_via_file_read_bytes() {
        let workspace_root = unique_workspace("wasi-read-bytes");
        let shared_file = workspace_root.join(SHARED_DIR_NAME).join("photo.bin");
        let expected = vec![0_u8, 1, 2, 3, 254, 255];
        fs::write(&shared_file, &expected).expect("test file should be writable");

        let runner = make_runner(&workspace_root);
        let result = runner
            .invoke(
                "file_read_bytes",
                WasmCapabilityProfile::FileReadOnly,
                &json!({ "path": shared_file.to_string_lossy() }),
                Some("op-read-bytes"),
            )
            .await
            .expect("file_read_bytes should succeed");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(result.output.as_bytes())
            .expect("base64 output should decode");
        assert_eq!(decoded, expected);

        let _ = fs::remove_dir_all(workspace_root);
    }

    #[tokio::test]
    async fn wasm_runner_file_list_without_path_defaults_to_shared_mount() {
        let workspace_root = unique_workspace("wasi-list-default-shared");
        let shared_file = workspace_root.join(SHARED_DIR_NAME).join("listed.txt");
        let tmp_file = workspace_root.join(TMP_DIR_NAME).join("tmp-only.txt");
        fs::write(&shared_file, "in-shared").expect("shared file should be writable");
        fs::write(&tmp_file, "in-tmp").expect("tmp file should be writable");

        let runner = make_runner(&workspace_root);
        let result = runner
            .invoke(
                "file_list",
                WasmCapabilityProfile::FileReadOnly,
                &json!({}),
                None,
            )
            .await
            .expect("file_list should succeed");
        assert!(result.output.contains("listed.txt"));
        assert!(
            !result.output.contains("tmp-only.txt"),
            "default listing should target shared/"
        );

        let _ = fs::remove_dir_all(workspace_root);
    }

    #[tokio::test]
    async fn wasm_runner_writes_file_within_allowed_mount() {
        let workspace_root = unique_workspace("wasi-write");
        let target = workspace_root.join(SHARED_DIR_NAME).join("output.txt");

        let runner = make_runner(&workspace_root);
        let result = runner
            .invoke(
                "file_write",
                WasmCapabilityProfile::FileReadWrite,
                &json!({ "path": target.to_string_lossy(), "content": "written" }),
                None,
            )
            .await
            .expect("file_write should succeed");

        assert!(result.output.contains("wrote"));
        assert_eq!(fs::read_to_string(&target).unwrap(), "written");

        let _ = fs::remove_dir_all(workspace_root);
    }

    #[tokio::test]
    async fn wasm_runner_denies_paths_outside_preopened_dirs() {
        let workspace_root = unique_workspace("wasi-deny-outside");
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let outside = env::temp_dir().join(format!(
            "oxydra-wasi-outside-{}-{nanos}.txt",
            std::process::id()
        ));
        fs::write(&outside, "blocked").expect("outside file should be writable");

        let runner = make_runner(&workspace_root);
        let err = runner
            .invoke(
                "file_read",
                WasmCapabilityProfile::FileReadOnly,
                &json!({ "path": outside.to_string_lossy() }),
                None,
            )
            .await
            .expect_err("path outside preopened dirs should be denied");

        assert!(matches!(
            err,
            SandboxError::WasmInvocationFailed { tool, message, .. }
                if tool == "file_read" && message.contains("not within any preopened mount")
        ));

        let _ = fs::remove_file(outside);
        let _ = fs::remove_dir_all(workspace_root);
    }

    #[tokio::test]
    async fn wasm_runner_rejects_profile_mismatches_before_wasm_execution() {
        let workspace_root = unique_workspace("wasi-profile-mismatch");
        let target = workspace_root.join(SHARED_DIR_NAME).join("nope.txt");

        let runner = make_runner(&workspace_root);
        let err = runner
            .invoke(
                "file_write",
                WasmCapabilityProfile::FileReadOnly, // wrong profile
                &json!({ "path": target.to_string_lossy(), "content": "should-not-write" }),
                None,
            )
            .await
            .expect_err("profile mismatch should be rejected");

        assert!(matches!(
            err,
            SandboxError::WasmInvocationFailed { tool, message, .. }
                if tool == "file_write"
                    && message.contains("requires capability profile")
                    && message.contains("FileReadWrite")
        ));
        assert!(!target.exists());
        assert_eq!(runner.audit_log().len(), 1);

        let _ = fs::remove_dir_all(workspace_root);
    }

    #[tokio::test]
    async fn wasm_runner_vault_two_step_read_then_write() {
        let workspace_root = unique_workspace("wasi-vault");
        let vault_file = workspace_root.join(VAULT_DIR_NAME).join("secret.txt");
        fs::write(&vault_file, "vault-secret").expect("vault file should be writable");

        let runner = make_runner(&workspace_root);

        // Step 1: read from vault
        let read_result = runner
            .invoke(
                "vault_copyto_read",
                WasmCapabilityProfile::VaultReadStep,
                &json!({ "source_path": vault_file.to_string_lossy() }),
                None,
            )
            .await
            .expect("vault_copyto_read should succeed");
        assert_eq!(read_result.output, "vault-secret");

        // Step 2: write to shared (not vault)
        let dest = workspace_root.join(SHARED_DIR_NAME).join("decrypted.txt");
        let write_result = runner
            .invoke(
                "vault_copyto_write",
                WasmCapabilityProfile::VaultWriteStep,
                &json!({
                    "destination_path": dest.to_string_lossy(),
                    "content": read_result.output
                }),
                None,
            )
            .await
            .expect("vault_copyto_write should succeed");
        assert!(write_result.output.contains("copied"));
        assert_eq!(fs::read_to_string(&dest).unwrap(), "vault-secret");

        let _ = fs::remove_dir_all(workspace_root);
    }

    #[tokio::test]
    async fn wasm_runner_records_audit_for_every_invocation() {
        let workspace_root = unique_workspace("wasi-audit");
        let file = workspace_root.join(SHARED_DIR_NAME).join("audit.txt");
        fs::write(&file, "ok").expect("test file should be writable");

        let runner = make_runner(&workspace_root);
        runner
            .invoke(
                "file_read",
                WasmCapabilityProfile::FileReadOnly,
                &json!({ "path": file.to_string_lossy() }),
                Some("op-1"),
            )
            .await
            .expect("file_read should succeed");

        let log = runner.audit_log();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].tool_name, "file_read");
        assert_eq!(log[0].operation_id.as_deref(), Some("op-1"));

        let _ = fs::remove_dir_all(workspace_root);
    }

    #[tokio::test]
    async fn wasm_runner_web_fetch_blocks_metadata_endpoint() {
        let workspace_root = unique_workspace("wasi-web-block");
        let runner = WasmWasiToolRunner::for_bootstrap_workspace(&workspace_root)
            .expect("runner should initialize");

        let err = runner
            .invoke(
                "web_fetch",
                WasmCapabilityProfile::Web,
                &json!({ "url": "http://169.254.169.254/latest/meta-data" }),
                None,
            )
            .await
            .expect_err("metadata endpoint should be blocked");

        assert!(matches!(
            err,
            SandboxError::WasmInvocationFailed { tool, message, .. }
                if tool == "web_fetch" && message.contains("metadata endpoint")
        ));

        let _ = fs::remove_dir_all(workspace_root);
    }
}
