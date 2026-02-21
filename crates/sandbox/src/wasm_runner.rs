use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use async_trait::async_trait;
use reqwest::Client;
use serde_json::Value;

use crate::SandboxError;

const SHARED_DIR_NAME: &str = "shared";
const TMP_DIR_NAME: &str = "tmp";
const VAULT_DIR_NAME: &str = "vault";
const MAX_SEARCH_MATCHES: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WasmCapabilityProfile {
    FileReadOnly,
    FileReadWrite,
    Web,
    VaultReadStep,
    VaultWriteStep,
}

impl WasmCapabilityProfile {
    pub fn mount_profile(self, mounts: &WasmWorkspaceMounts) -> Vec<WasmMount> {
        match self {
            Self::FileReadOnly => vec![
                WasmMount::new("shared", mounts.shared.clone(), true),
                WasmMount::new("tmp", mounts.tmp.clone(), true),
                WasmMount::new("vault", mounts.vault.clone(), true),
            ],
            Self::FileReadWrite => vec![
                WasmMount::new("shared", mounts.shared.clone(), false),
                WasmMount::new("tmp", mounts.tmp.clone(), false),
            ],
            Self::Web => Vec::new(),
            Self::VaultReadStep => vec![WasmMount::new("vault", mounts.vault.clone(), true)],
            Self::VaultWriteStep => vec![
                WasmMount::new("shared", mounts.shared.clone(), false),
                WasmMount::new("tmp", mounts.tmp.clone(), false),
            ],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WasmWorkspaceMounts {
    pub shared: PathBuf,
    pub tmp: PathBuf,
    pub vault: PathBuf,
}

impl WasmWorkspaceMounts {
    pub fn for_bootstrap_workspace(workspace_root: impl AsRef<Path>) -> Self {
        let workspace_root = workspace_root.as_ref();
        Self {
            shared: workspace_root.join(SHARED_DIR_NAME),
            tmp: workspace_root.join(TMP_DIR_NAME),
            vault: workspace_root.join(VAULT_DIR_NAME),
        }
    }

    pub fn for_direct_workspace(workspace_root: impl AsRef<Path>) -> Self {
        let workspace_root = workspace_root.as_ref().to_path_buf();
        Self {
            shared: workspace_root.clone(),
            tmp: workspace_root.clone(),
            vault: workspace_root,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WasmMount {
    pub label: &'static str,
    pub path: PathBuf,
    pub read_only: bool,
}

impl WasmMount {
    fn new(label: &'static str, path: PathBuf, read_only: bool) -> Self {
        Self {
            label,
            path,
            read_only,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WasmInvocationMetadata {
    pub request_id: String,
    pub operation_id: Option<String>,
    pub tool_name: String,
    pub profile: WasmCapabilityProfile,
    pub mounts: Vec<WasmMount>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WasmInvocationResult {
    pub metadata: WasmInvocationMetadata,
    pub output: String,
}

#[async_trait]
pub trait WasmToolRunner: Send + Sync {
    async fn invoke(
        &self,
        tool_name: &str,
        profile: WasmCapabilityProfile,
        arguments: &Value,
        operation_id: Option<&str>,
    ) -> Result<WasmInvocationResult, SandboxError>;

    fn audit_log(&self) -> Vec<WasmInvocationMetadata>;
}

pub struct HostWasmToolRunner {
    mounts: WasmWorkspaceMounts,
    request_counter: AtomicU64,
    audit_records: Mutex<Vec<WasmInvocationMetadata>>,
    http_client: Client,
}

impl HostWasmToolRunner {
    pub fn new(mounts: WasmWorkspaceMounts) -> Self {
        Self {
            mounts,
            request_counter: AtomicU64::new(0),
            audit_records: Mutex::new(Vec::new()),
            http_client: Client::new(),
        }
    }

    pub fn for_bootstrap_workspace(workspace_root: impl AsRef<Path>) -> Self {
        Self::new(WasmWorkspaceMounts::for_bootstrap_workspace(workspace_root))
    }

    pub fn for_direct_workspace(workspace_root: impl AsRef<Path>) -> Self {
        Self::new(WasmWorkspaceMounts::for_direct_workspace(workspace_root))
    }

    fn next_request_id(&self, tool_name: &str) -> String {
        let request_number = self.request_counter.fetch_add(1, Ordering::Relaxed) + 1;
        format!("{tool_name}-{request_number}")
    }

    fn record_audit(&self, metadata: WasmInvocationMetadata) {
        if let Ok(mut records) = self.audit_records.lock() {
            records.push(metadata);
        }
    }

    async fn execute_operation(
        &self,
        tool_name: &str,
        arguments: &Value,
    ) -> Result<String, String> {
        match tool_name {
            "file_read" => self.file_read(arguments),
            "file_write" => self.file_write(arguments),
            "file_edit" => self.file_edit(arguments),
            "file_delete" => self.file_delete(arguments),
            "file_list" => self.file_list(arguments),
            "file_search" => self.file_search(arguments),
            "web_fetch" => self.web_fetch(arguments).await,
            "web_search" => self.web_search(arguments).await,
            "vault_copyto_read" => self.vault_copyto_read(arguments),
            "vault_copyto_write" => self.vault_copyto_write(arguments),
            _ => Err(format!("unknown wasm tool operation `{tool_name}`")),
        }
    }

    fn file_read(&self, arguments: &Value) -> Result<String, String> {
        let path = required_string(arguments, "path", "file_read")?;
        fs::read_to_string(&path).map_err(|error| format!("failed to read `{path}`: {error}"))
    }

    fn file_write(&self, arguments: &Value) -> Result<String, String> {
        let path = required_string(arguments, "path", "file_write")?;
        let content = required_string(arguments, "content", "file_write")?;
        fs::write(&path, content.as_bytes())
            .map_err(|error| format!("failed to write `{path}`: {error}"))?;
        Ok(format!("wrote {} bytes to {path}", content.len()))
    }

    fn file_edit(&self, arguments: &Value) -> Result<String, String> {
        let path = required_string(arguments, "path", "file_edit")?;
        let old_text = required_string(arguments, "old_text", "file_edit")?;
        let new_text = required_string(arguments, "new_text", "file_edit")?;
        if old_text.is_empty() {
            return Err("old_text must not be empty".to_owned());
        }
        let content = fs::read_to_string(&path)
            .map_err(|error| format!("failed to read `{path}`: {error}"))?;
        let occurrences = content.matches(&old_text).count();
        if occurrences == 0 {
            return Err("old_text was not found in target file".to_owned());
        }
        if occurrences > 1 {
            return Err(format!(
                "old_text matched {occurrences} locations; provide a more specific snippet"
            ));
        }
        let updated = content.replacen(&old_text, &new_text, 1);
        fs::write(&path, updated.as_bytes())
            .map_err(|error| format!("failed to write `{path}`: {error}"))?;
        Ok(format!("updated {path}"))
    }

    fn file_delete(&self, arguments: &Value) -> Result<String, String> {
        let path = required_string(arguments, "path", "file_delete")?;
        let metadata =
            fs::metadata(&path).map_err(|error| format!("failed to stat `{path}`: {error}"))?;
        if metadata.is_dir() {
            fs::remove_dir_all(&path)
                .map_err(|error| format!("failed to remove `{path}`: {error}"))?;
            Ok(format!("deleted directory {path}"))
        } else {
            fs::remove_file(&path)
                .map_err(|error| format!("failed to remove `{path}`: {error}"))?;
            Ok(format!("deleted file {path}"))
        }
    }

    fn file_list(&self, arguments: &Value) -> Result<String, String> {
        let path = optional_string(arguments, "path").unwrap_or_else(|| ".".to_owned());
        let mut entries = Vec::new();
        let directory =
            fs::read_dir(&path).map_err(|error| format!("failed to list `{path}`: {error}"))?;
        for entry in directory {
            let entry =
                entry.map_err(|error| format!("failed to read entry in `{path}`: {error}"))?;
            let mut label = entry.file_name().to_string_lossy().to_string();
            if entry.path().is_dir() {
                label.push('/');
            }
            entries.push(label);
        }
        entries.sort();
        if entries.is_empty() {
            Ok("no entries found".to_owned())
        } else {
            Ok(entries.join("\n"))
        }
    }

    fn file_search(&self, arguments: &Value) -> Result<String, String> {
        let path = required_string(arguments, "path", "file_search")?;
        let query = required_string(arguments, "query", "file_search")?;
        if query.is_empty() {
            return Err("query must not be empty".to_owned());
        }
        let mut matches = Vec::new();
        collect_search_matches(Path::new(&path), &query, &mut matches)?;
        if matches.is_empty() {
            Ok("no matches found".to_owned())
        } else {
            Ok(matches.join("\n"))
        }
    }

    async fn web_fetch(&self, arguments: &Value) -> Result<String, String> {
        let url = required_string(arguments, "url", "web_fetch")?;
        let response = self
            .http_client
            .get(&url)
            .send()
            .await
            .map_err(|error| format!("web fetch request failed for `{url}`: {error}"))?;
        let status = response.status();
        let body = response.text().await.map_err(|error| {
            format!("failed to read web fetch response body for `{url}`: {error}")
        })?;
        if !status.is_success() {
            return Err(format!(
                "web fetch returned status {status} for `{url}`: {}",
                truncate_web_body(&body)
            ));
        }
        Ok(body)
    }

    async fn web_search(&self, arguments: &Value) -> Result<String, String> {
        let query = required_string(arguments, "query", "web_search")?;
        if query.is_empty() {
            return Err("query must not be empty".to_owned());
        }
        let encoded_query = query
            .split_whitespace()
            .filter(|token| !token.is_empty())
            .collect::<Vec<_>>()
            .join("+");
        let url = format!("https://duckduckgo.com/?q={encoded_query}");
        self.web_fetch(&serde_json::json!({ "url": url })).await
    }

    fn vault_copyto_read(&self, arguments: &Value) -> Result<String, String> {
        let source_path = required_string(arguments, "source_path", "vault_copyto_read")?;
        fs::read_to_string(&source_path)
            .map_err(|error| format!("failed to read vault source `{source_path}`: {error}"))
    }

    fn vault_copyto_write(&self, arguments: &Value) -> Result<String, String> {
        let destination_path =
            required_string(arguments, "destination_path", "vault_copyto_write")?;
        let content = required_string(arguments, "content", "vault_copyto_write")?;
        fs::write(&destination_path, content.as_bytes()).map_err(|error| {
            format!("failed to write destination `{destination_path}`: {error}")
        })?;
        Ok(format!(
            "copied {} bytes to {destination_path}",
            content.len()
        ))
    }
}

#[async_trait]
impl WasmToolRunner for HostWasmToolRunner {
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

        let output = self
            .execute_operation(tool_name, arguments)
            .await
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

fn optional_string(arguments: &Value, field: &str) -> Option<String> {
    arguments
        .get(field)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn required_string(arguments: &Value, field: &str, tool_name: &str) -> Result<String, String> {
    arguments
        .get(field)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("tool `{tool_name}` requires string argument `{field}`"))
}

fn truncate_web_body(body: &str) -> String {
    const MAX_BODY_PREVIEW_BYTES: usize = 256;
    if body.len() <= MAX_BODY_PREVIEW_BYTES {
        return body.to_owned();
    }
    let mut cutoff = MAX_BODY_PREVIEW_BYTES;
    while cutoff > 0 && !body.is_char_boundary(cutoff) {
        cutoff -= 1;
    }
    format!("{}...[truncated]", &body[..cutoff])
}

fn collect_search_matches(
    path: &Path,
    query: &str,
    matches: &mut Vec<String>,
) -> Result<(), String> {
    if matches.len() >= MAX_SEARCH_MATCHES {
        return Ok(());
    }

    let metadata = fs::metadata(path)
        .map_err(|error| format!("failed to inspect `{}`: {error}", path.display()))?;
    if metadata.is_dir() {
        let entries = fs::read_dir(path)
            .map_err(|error| format!("failed to list `{}`: {error}", path.display()))?;
        for entry in entries {
            let entry = entry.map_err(|error| {
                format!("failed to read entry in `{}`: {error}", path.display())
            })?;
            collect_search_matches(&entry.path(), query, matches)?;
            if matches.len() >= MAX_SEARCH_MATCHES {
                break;
            }
        }
        return Ok(());
    }

    if !metadata.is_file() {
        return Ok(());
    }

    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(_) => return Ok(()),
    };
    for (line_index, line) in content.lines().enumerate() {
        if line.contains(query) {
            matches.push(format!("{}:{}:{line}", path.display(), line_index + 1));
            if matches.len() >= MAX_SEARCH_MATCHES {
                break;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    use serde_json::json;

    use super::*;

    #[test]
    fn capability_profiles_map_to_expected_mount_sets() {
        let mounts = WasmWorkspaceMounts::for_bootstrap_workspace("/tmp/ws2-profile");
        let ro_mounts = WasmCapabilityProfile::FileReadOnly.mount_profile(&mounts);
        assert_eq!(ro_mounts.len(), 3);
        assert!(ro_mounts.iter().all(|mount| mount.read_only));

        let rw_mounts = WasmCapabilityProfile::FileReadWrite.mount_profile(&mounts);
        assert_eq!(rw_mounts.len(), 2);
        assert!(rw_mounts.iter().all(|mount| !mount.read_only));

        let web_mounts = WasmCapabilityProfile::Web.mount_profile(&mounts);
        assert!(web_mounts.is_empty());

        let vault_read = WasmCapabilityProfile::VaultReadStep.mount_profile(&mounts);
        assert_eq!(vault_read.len(), 1);
        assert_eq!(vault_read[0].label, "vault");
        assert!(vault_read[0].read_only);
    }

    #[tokio::test]
    async fn host_runner_records_audit_metadata_for_file_invocation() {
        let workspace_root = unique_workspace("wasm-audit");
        let shared_file = workspace_root.join(SHARED_DIR_NAME).join("audit.txt");
        fs::write(&shared_file, "audit-ok").expect("shared file should be writable");

        let runner = HostWasmToolRunner::for_bootstrap_workspace(&workspace_root);
        let result = runner
            .invoke(
                "file_read",
                WasmCapabilityProfile::FileReadOnly,
                &json!({ "path": shared_file.to_string_lossy() }),
                Some("operation-1"),
            )
            .await
            .expect("file_read invocation should succeed");

        assert_eq!(result.output, "audit-ok");
        assert!(result.metadata.request_id.starts_with("file_read-"));
        assert_eq!(result.metadata.operation_id.as_deref(), Some("operation-1"));
        assert_eq!(result.metadata.profile, WasmCapabilityProfile::FileReadOnly);
        assert_eq!(runner.audit_log().len(), 1);

        let _ = fs::remove_dir_all(workspace_root);
    }

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
}
