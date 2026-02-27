use std::{
    collections::BTreeSet,
    env, fs,
    net::{IpAddr, Ipv4Addr},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use base64::Engine;
use reqwest::{
    Client, RequestBuilder,
    header::{ACCEPT, ACCEPT_LANGUAGE, USER_AGENT},
};
use serde_json::Value;

use super::SandboxError;

pub(crate) const SHARED_DIR_NAME: &str = "shared";
pub(crate) const TMP_DIR_NAME: &str = "tmp";
pub(crate) const VAULT_DIR_NAME: &str = "vault";
const MAX_SEARCH_MATCHES: usize = 200;
pub(crate) const WEB_TOOL_USER_AGENT: &str = "oxydra/0.1 (+https://github.com/shantanugoel/oxydra)";
const WEB_EGRESS_MODE_ENV_KEY: &str = "OXYDRA_WEB_EGRESS_MODE";
const WEB_EGRESS_ALLOWLIST_ENV_KEY: &str = "OXYDRA_WEB_EGRESS_ALLOWLIST";
const WEB_EGRESS_PROXY_URL_ENV_KEY: &str = "OXYDRA_WEB_EGRESS_PROXY_URL";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WebEgressMode {
    DefaultSafe,
    StrictAllowlistProxy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WebEgressPolicy {
    mode: WebEgressMode,
    allowlist: BTreeSet<String>,
}

impl Default for WebEgressPolicy {
    fn default() -> Self {
        Self {
            mode: WebEgressMode::DefaultSafe,
            allowlist: BTreeSet::new(),
        }
    }
}

impl WebEgressPolicy {
    fn from_environment() -> Result<Self, String> {
        let mode = match read_non_empty_env(WEB_EGRESS_MODE_ENV_KEY)
            .map(|value| value.to_ascii_lowercase())
            .as_deref()
        {
            None | Some("default") | Some("safe") => WebEgressMode::DefaultSafe,
            Some("strict") | Some("strict_allowlist_proxy") => WebEgressMode::StrictAllowlistProxy,
            Some(value) => {
                return Err(format!(
                    "unsupported web egress mode `{value}`; expected default or strict"
                ));
            }
        };

        let allowlist = read_non_empty_env(WEB_EGRESS_ALLOWLIST_ENV_KEY)
            .map(|raw| {
                raw.split(',')
                    .filter_map(normalize_allowlist_entry)
                    .collect::<BTreeSet<_>>()
            })
            .unwrap_or_default();
        let proxy_url = read_non_empty_env(WEB_EGRESS_PROXY_URL_ENV_KEY);

        if mode == WebEgressMode::StrictAllowlistProxy {
            if allowlist.is_empty() {
                return Err(format!(
                    "strict web egress mode requires `{WEB_EGRESS_ALLOWLIST_ENV_KEY}` with one or more destinations"
                ));
            }
            let proxy_url = proxy_url.ok_or_else(|| {
                format!(
                    "strict web egress mode requires `{WEB_EGRESS_PROXY_URL_ENV_KEY}` to be set"
                )
            })?;
            validate_proxy_url(&proxy_url)?;
        } else if let Some(proxy_url) = proxy_url {
            validate_proxy_url(&proxy_url)?;
        }

        Ok(Self { mode, allowlist })
    }

    fn destination_allowlisted(&self, host: &str, port: u16) -> bool {
        if self.allowlist.is_empty() {
            return false;
        }

        let host = normalize_host_for_allowlist(host);
        self.allowlist.contains(&host) || self.allowlist.contains(&format!("{host}:{port}"))
    }
}

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

    pub fn from_mount_roots(
        shared: impl AsRef<Path>,
        tmp: impl AsRef<Path>,
        vault: impl AsRef<Path>,
    ) -> Self {
        Self {
            shared: shared.as_ref().to_path_buf(),
            tmp: tmp.as_ref().to_path_buf(),
            vault: vault.as_ref().to_path_buf(),
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
        let http_client = Client::builder()
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_else(|_| Client::new());
        Self {
            mounts,
            request_counter: AtomicU64::new(0),
            audit_records: Mutex::new(Vec::new()),
            http_client,
        }
    }

    pub fn for_bootstrap_workspace(workspace_root: impl AsRef<Path>) -> Self {
        Self::new(WasmWorkspaceMounts::for_bootstrap_workspace(workspace_root))
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
            "file_read_bytes" => self.file_read_bytes(arguments),
            "file_write" => self.file_write(arguments),
            "file_edit" => self.file_edit(arguments),
            "file_delete" => self.file_delete(arguments),
            "file_list" => self.file_list(arguments),
            "file_search" => self.file_search(arguments),
            "web_fetch" => super::web_fetch::execute(&self.http_client, arguments).await,
            "web_search" => super::web_search::execute(&self.http_client, arguments).await,
            "vault_copyto_read" => self.vault_copyto_read(arguments),
            "vault_copyto_write" => self.vault_copyto_write(arguments),
            _ => Err(format!("unknown wasm tool operation `{tool_name}`")),
        }
    }

    fn enforce_capability_profile(
        &self,
        tool_name: &str,
        profile: WasmCapabilityProfile,
        arguments: &Value,
    ) -> Result<(), String> {
        let expected_profile = expected_capability_profile(tool_name)
            .ok_or_else(|| format!("unknown wasm tool operation `{tool_name}`"))?;
        if expected_profile != profile {
            return Err(format!(
                "tool `{tool_name}` requires capability profile `{:?}`, got `{:?}`",
                expected_profile, profile
            ));
        }

        match tool_name {
            "file_read" | "file_read_bytes" | "file_search" => {
                let path = required_string(arguments, "path", tool_name)?;
                self.enforce_mounted_path(profile, &path, false)
            }
            "file_list" => {
                let path = optional_string(arguments, "path")
                    .unwrap_or_else(|| self.mounts.shared.to_string_lossy().into_owned());
                self.enforce_mounted_path(profile, &path, false)
            }
            "file_write" | "file_edit" | "file_delete" => {
                let path = required_string(arguments, "path", tool_name)?;
                self.enforce_mounted_path(profile, &path, true)
            }
            "vault_copyto_read" => {
                let source_path = required_string(arguments, "source_path", tool_name)?;
                self.enforce_mounted_path(profile, &source_path, false)
            }
            "vault_copyto_write" => {
                let destination_path = required_string(arguments, "destination_path", tool_name)?;
                self.enforce_mounted_path(profile, &destination_path, true)
            }
            "web_fetch" | "web_search" => Ok(()),
            _ => Err(format!("unknown wasm tool operation `{tool_name}`")),
        }
    }

    fn enforce_mounted_path(
        &self,
        profile: WasmCapabilityProfile,
        path: &str,
        write_access: bool,
    ) -> Result<(), String> {
        let canonical_target = canonicalize_invocation_path(path, true)?;
        let mounts = profile.mount_profile(&self.mounts);
        let allowed_mount = mounts
            .iter()
            .find(|mount| path_within_mount(&canonical_target, &mount.path));

        match allowed_mount {
            Some(mount) if write_access && mount.read_only => Err(format!(
                "path `{path}` resolved to `{}` requires write access, but mount `{}` is read-only",
                canonical_target.display(),
                mount.label
            )),
            Some(_) => Ok(()),
            None => Err(format!(
                "path `{path}` resolved to `{}` outside capability mounts [{}]",
                canonical_target.display(),
                format_mount_roots(&mounts)
            )),
        }
    }

    fn file_read(&self, arguments: &Value) -> Result<String, String> {
        let path = required_string(arguments, "path", "file_read")?;
        fs::read_to_string(&path).map_err(|error| format!("failed to read `{path}`: {error}"))
    }

    fn file_read_bytes(&self, arguments: &Value) -> Result<String, String> {
        let path = required_string(arguments, "path", "file_read_bytes")?;
        let bytes = fs::read(&path).map_err(|error| format!("failed to read `{path}`: {error}"))?;
        Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
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
        let path = optional_string(arguments, "path")
            .unwrap_or_else(|| self.mounts.shared.to_string_lossy().into_owned());
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
        self.enforce_capability_profile(tool_name, profile, arguments)
            .map_err(|message| SandboxError::WasmInvocationFailed {
                tool: tool_name.to_owned(),
                request_id: request_id.clone(),
                message,
            })?;

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

fn canonicalize_invocation_path(
    raw_path: &str,
    allow_missing_leaf: bool,
) -> Result<PathBuf, String> {
    let path = raw_path.trim();
    if path.is_empty() {
        return Err("path argument must not be empty".to_owned());
    }

    let target_path = normalize_absolute_path(PathBuf::from(path));
    if !allow_missing_leaf || target_path.exists() {
        return fs::canonicalize(&target_path).map_err(|error| {
            format!(
                "failed to canonicalize `{}`: {error}",
                target_path.display()
            )
        });
    }

    let mut existing_ancestor = target_path.as_path();
    while !existing_ancestor.exists() {
        existing_ancestor = existing_ancestor.parent().ok_or_else(|| {
            format!(
                "path `{}` does not include a canonicalizable parent directory",
                target_path.display()
            )
        })?;
    }
    let canonical_ancestor = fs::canonicalize(existing_ancestor).map_err(|error| {
        format!(
            "failed to canonicalize parent `{}` for `{}`: {error}",
            existing_ancestor.display(),
            target_path.display()
        )
    })?;
    let suffix = target_path
        .strip_prefix(existing_ancestor)
        .map_err(|error| {
            format!(
                "failed to normalize `{}` from canonical ancestor `{}`: {error}",
                target_path.display(),
                existing_ancestor.display()
            )
        })?;
    Ok(canonical_ancestor.join(suffix))
}

fn path_within_mount(path: &Path, mount_root: &Path) -> bool {
    let normalized_mount = normalize_absolute_path(mount_root.to_path_buf());
    let canonical_mount = fs::canonicalize(&normalized_mount).unwrap_or(normalized_mount);
    path == canonical_mount || path.starts_with(&canonical_mount)
}

fn normalize_absolute_path(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(&path))
            .unwrap_or(path)
    }
}

fn format_mount_roots(mounts: &[WasmMount]) -> String {
    mounts
        .iter()
        .map(|mount| format!("{}={}", mount.label, mount.path.display()))
        .collect::<Vec<_>>()
        .join(", ")
}

pub(crate) fn request_with_default_headers(request: RequestBuilder) -> RequestBuilder {
    request
        .header(USER_AGENT, WEB_TOOL_USER_AGENT)
        .header(
            ACCEPT,
            "application/json,text/html,application/xhtml+xml,text/plain;q=0.9,*/*;q=0.8",
        )
        .header(ACCEPT_LANGUAGE, "en-US,en;q=0.9")
}

pub(crate) async fn read_response_bytes(
    mut response: reqwest::Response,
    max_bytes: usize,
) -> Result<(Vec<u8>, bool), String> {
    let mut output = Vec::new();
    let mut truncated = false;
    loop {
        let chunk = response
            .chunk()
            .await
            .map_err(|error| format!("failed to read response chunk: {error}"))?;
        let Some(chunk) = chunk else {
            break;
        };
        if output.len() >= max_bytes {
            truncated = true;
            break;
        }
        let remaining = max_bytes.saturating_sub(output.len());
        if chunk.len() > remaining {
            output.extend_from_slice(&chunk[..remaining]);
            truncated = true;
            break;
        }
        output.extend_from_slice(&chunk);
    }
    Ok((output, truncated))
}

pub(crate) async fn parse_and_validate_web_url(raw_url: &str) -> Result<reqwest::Url, String> {
    let parsed_url = reqwest::Url::parse(raw_url)
        .map_err(|error| format!("invalid url `{raw_url}`: {error}"))?;
    if !matches!(parsed_url.scheme(), "http" | "https") {
        return Err(format!(
            "web fetch only supports http/https URLs, got scheme `{}`",
            parsed_url.scheme()
        ));
    }

    let host = parsed_url
        .host_str()
        .ok_or_else(|| format!("url `{raw_url}` does not include a hostname"))?;
    let port = parsed_url.port_or_known_default().ok_or_else(|| {
        format!(
            "url `{raw_url}` is missing a known port for scheme `{}`",
            parsed_url.scheme()
        )
    })?;
    let egress_policy = WebEgressPolicy::from_environment()?;
    let destination_allowlisted = egress_policy.destination_allowlisted(host, port);
    if egress_policy.mode == WebEgressMode::StrictAllowlistProxy && !destination_allowlisted {
        return Err(format!(
            "strict web egress policy blocked destination `{host}:{port}` because it is not allowlisted"
        ));
    }
    let resolved_ips = resolve_host_ips(host, port).await?;
    enforce_web_target_ips(host, &resolved_ips, &egress_policy, destination_allowlisted)?;
    Ok(parsed_url)
}

async fn resolve_host_ips(host: &str, port: u16) -> Result<Vec<IpAddr>, String> {
    let resolved = tokio::net::lookup_host((host, port))
        .await
        .map_err(|error| format!("failed to resolve host `{host}`: {error}"))?;
    let unique_ips = resolved
        .map(|socket_address| socket_address.ip())
        .collect::<BTreeSet<_>>();
    if unique_ips.is_empty() {
        return Err(format!("host `{host}` resolved to no IP addresses"));
    }
    Ok(unique_ips.into_iter().collect())
}

fn enforce_web_target_ips(
    host: &str,
    resolved_ips: &[IpAddr],
    _policy: &WebEgressPolicy,
    destination_allowlisted: bool,
) -> Result<(), String> {
    for ip in resolved_ips {
        if let Some(reason) = blocked_ip_reason(*ip) {
            // An explicitly allowlisted host bypasses the local/private IP block
            // in all egress modes. This lets users reach self-hosted services
            // (e.g. a local SearxNG instance) without switching to strict mode.
            if destination_allowlisted {
                continue;
            }
            return Err(format!(
                "web target `{host}` resolved to blocked address `{ip}` ({reason})"
            ));
        }
    }
    Ok(())
}

fn read_non_empty_env(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn normalize_allowlist_entry(entry: &str) -> Option<String> {
    let entry = entry.trim();
    if entry.is_empty() {
        return None;
    }

    if let Ok(url) = reqwest::Url::parse(entry) {
        let host = url
            .host_str()
            .map(normalize_host_for_allowlist)
            .filter(|value| !value.is_empty())?;
        return Some(match url.port() {
            Some(port) => format!("{host}:{port}"),
            None => host,
        });
    }

    Some(normalize_host_for_allowlist(entry))
}

fn normalize_host_for_allowlist(host: &str) -> String {
    host.trim().trim_end_matches('.').to_ascii_lowercase()
}

fn validate_proxy_url(proxy_url: &str) -> Result<(), String> {
    let parsed = reqwest::Url::parse(proxy_url)
        .map_err(|error| format!("invalid web egress proxy url `{proxy_url}`: {error}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(format!(
            "web egress proxy url `{proxy_url}` must use http or https scheme"
        ));
    }
    if parsed.host_str().is_none() {
        return Err(format!(
            "web egress proxy url `{proxy_url}` must include a hostname"
        ));
    }
    Ok(())
}

fn blocked_ip_reason(ip: IpAddr) -> Option<&'static str> {
    match ip {
        IpAddr::V4(v4) if v4 == Ipv4Addr::new(169, 254, 169, 254) => {
            Some("metadata endpoint is blocked")
        }
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            if octets[0] == 127 {
                Some("loopback range 127.0.0.0/8 is blocked")
            } else if octets[0] == 169 && octets[1] == 254 {
                Some("link-local range 169.254.0.0/16 is blocked")
            } else if octets[0] == 10 {
                Some("private range 10.0.0.0/8 is blocked")
            } else if octets[0] == 172 && (16..=31).contains(&octets[1]) {
                Some("private range 172.16.0.0/12 is blocked")
            } else if octets[0] == 192 && octets[1] == 168 {
                Some("private range 192.168.0.0/16 is blocked")
            } else {
                None
            }
        }
        IpAddr::V6(v6) if v6.is_loopback() => Some("loopback IPv6 addresses are blocked"),
        IpAddr::V6(v6) if (v6.segments()[0] & 0xfe00) == 0xfc00 => {
            Some("unique-local IPv6 range fc00::/7 is blocked")
        }
        IpAddr::V6(v6) if (v6.segments()[0] & 0xffc0) == 0xfe80 => {
            Some("link-local IPv6 range fe80::/10 is blocked")
        }
        _ => None,
    }
}

pub(crate) fn truncate_web_body(body: &str) -> String {
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
        net::{IpAddr, Ipv4Addr},
        sync::OnceLock,
        time::{SystemTime, UNIX_EPOCH},
    };

    use base64::Engine;
    use serde_json::json;

    use super::*;

    fn env_lock() -> &'static tokio::sync::Mutex<()> {
        static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
    }

    #[derive(Debug)]
    struct EnvVarGuard {
        key: &'static str,
        previous: Option<String>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = env::var(key).ok();
            // SAFETY: tests guard environment writes with a process-wide mutex.
            unsafe { env::set_var(key, value) };
            Self { key, previous }
        }

        fn unset(key: &'static str) -> Self {
            let previous = env::var(key).ok();
            // SAFETY: tests guard environment writes with a process-wide mutex.
            unsafe { env::remove_var(key) };
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(value) = &self.previous {
                // SAFETY: tests guard environment writes with a process-wide mutex.
                unsafe { env::set_var(self.key, value) };
            } else {
                // SAFETY: tests guard environment writes with a process-wide mutex.
                unsafe { env::remove_var(self.key) };
            }
        }
    }

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

    #[tokio::test]
    async fn host_runner_file_read_bytes_returns_base64_encoded_data() {
        let workspace_root = unique_workspace("wasm-read-bytes");
        let shared_file = workspace_root.join(SHARED_DIR_NAME).join("photo.bin");
        let expected = vec![0_u8, 1, 2, 3, 254, 255];
        fs::write(&shared_file, &expected).expect("shared file should be writable");

        let runner = HostWasmToolRunner::for_bootstrap_workspace(&workspace_root);
        let result = runner
            .invoke(
                "file_read_bytes",
                WasmCapabilityProfile::FileReadOnly,
                &json!({ "path": shared_file.to_string_lossy() }),
                None,
            )
            .await
            .expect("file_read_bytes invocation should succeed");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(result.output.as_bytes())
            .expect("base64 output should decode");
        assert_eq!(decoded, expected);

        let _ = fs::remove_dir_all(workspace_root);
    }

    #[tokio::test]
    async fn host_runner_file_list_without_path_defaults_to_shared_mount() {
        let workspace_root = unique_workspace("wasm-list-default-shared");
        let shared_file = workspace_root.join(SHARED_DIR_NAME).join("listed.txt");
        let tmp_file = workspace_root
            .join(TMP_DIR_NAME)
            .join("hidden-from-default.txt");
        fs::write(&shared_file, "in-shared").expect("shared file should be writable");
        fs::write(&tmp_file, "in-tmp").expect("tmp file should be writable");

        let runner = HostWasmToolRunner::for_bootstrap_workspace(&workspace_root);
        let result = runner
            .invoke(
                "file_list",
                WasmCapabilityProfile::FileReadOnly,
                &json!({}),
                None,
            )
            .await
            .expect("file_list invocation should succeed");
        assert!(result.output.contains("listed.txt"));
        assert!(
            !result.output.contains("hidden-from-default.txt"),
            "default listing should be rooted at shared/"
        );

        let _ = fs::remove_dir_all(workspace_root);
    }

    #[tokio::test]
    async fn host_runner_denies_paths_outside_capability_mounts() {
        let workspace_root = unique_workspace("wasm-capability-deny");
        let outside = env::temp_dir().join(format!(
            "oxydra-wasm-outside-{}-{}.txt",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock should be after unix epoch")
                .as_nanos()
        ));
        fs::write(&outside, "blocked").expect("outside file should be writable");

        let runner = HostWasmToolRunner::for_bootstrap_workspace(&workspace_root);
        let denied = runner
            .invoke(
                "file_read",
                WasmCapabilityProfile::FileReadOnly,
                &json!({ "path": outside.to_string_lossy() }),
                None,
            )
            .await
            .expect_err("outside path should be denied by capability mounts");
        assert!(matches!(
            denied,
            SandboxError::WasmInvocationFailed { tool, message, .. }
                if tool == "file_read" && message.contains("outside capability mounts")
        ));

        let _ = fs::remove_file(outside);
        let _ = fs::remove_dir_all(workspace_root);
    }

    #[tokio::test]
    async fn host_runner_rejects_profile_mismatches_before_execution() {
        let workspace_root = unique_workspace("wasm-profile-mismatch");
        let target = workspace_root.join(SHARED_DIR_NAME).join("target.txt");
        let runner = HostWasmToolRunner::for_bootstrap_workspace(&workspace_root);

        let denied = runner
            .invoke(
                "file_write",
                WasmCapabilityProfile::FileReadOnly,
                &json!({
                    "path": target.to_string_lossy(),
                    "content": "should-not-write"
                }),
                None,
            )
            .await
            .expect_err("profile mismatch should be rejected");
        assert!(matches!(
            denied,
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
    async fn web_fetch_blocks_metadata_endpoint_targets() {
        let _env_lock = env_lock().lock().await;
        let workspace = unique_workspace("web-block-meta");
        let runner = HostWasmToolRunner::for_bootstrap_workspace(&workspace);
        let denied = runner
            .invoke(
                "web_fetch",
                WasmCapabilityProfile::Web,
                &json!({ "url": "http://169.254.169.254/latest/meta-data" }),
                None,
            )
            .await
            .expect_err("metadata endpoint target should be denied before request execution");
        assert!(matches!(
            denied,
            SandboxError::WasmInvocationFailed { tool, message, .. }
                if tool == "web_fetch"
                    && message.contains("169.254.169.254")
                    && message.contains("metadata endpoint")
        ));

        let _ = fs::remove_dir_all(workspace);
    }

    #[tokio::test]
    async fn web_fetch_blocks_private_rfc1918_targets() {
        let _env_lock = env_lock().lock().await;
        let workspace = unique_workspace("web-block-rfc1918");
        let runner = HostWasmToolRunner::for_bootstrap_workspace(&workspace);
        let denied = runner
            .invoke(
                "web_fetch",
                WasmCapabilityProfile::Web,
                &json!({ "url": "http://192.168.1.25/internal" }),
                None,
            )
            .await
            .expect_err("RFC1918 target should be denied before request execution");
        assert!(matches!(
            denied,
            SandboxError::WasmInvocationFailed { tool, message, .. }
                if tool == "web_fetch"
                    && message.contains("192.168.1.25")
                    && message.contains("192.168.0.0/16")
        ));

        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn web_target_policy_checks_all_resolved_ips() {
        let policy = WebEgressPolicy::default();
        let result = enforce_web_target_ips(
            "example.org",
            &[
                IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)),
                IpAddr::V4(Ipv4Addr::new(192, 168, 1, 8)),
            ],
            &policy,
            false,
        );
        assert!(matches!(
            result,
            Err(message)
                if message.contains("example.org")
                    && message.contains("192.168.1.8")
                    && message.contains("192.168.0.0/16")
        ));
    }

    #[tokio::test]
    async fn strict_web_egress_blocks_non_allowlisted_destinations() {
        let _env_lock = env_lock().lock().await;
        let _mode = EnvVarGuard::set(WEB_EGRESS_MODE_ENV_KEY, "strict");
        let _allowlist = EnvVarGuard::set(WEB_EGRESS_ALLOWLIST_ENV_KEY, "example.com");
        let _proxy = EnvVarGuard::set(WEB_EGRESS_PROXY_URL_ENV_KEY, "http://proxy.internal:8080");

        let error = parse_and_validate_web_url("https://example.org/search")
            .await
            .expect_err("strict mode should reject non-allowlisted destinations");
        assert!(error.contains("not allowlisted"));
    }

    #[tokio::test]
    async fn strict_web_egress_requires_proxy_configuration() {
        let _env_lock = env_lock().lock().await;
        let _mode = EnvVarGuard::set(WEB_EGRESS_MODE_ENV_KEY, "strict");
        let _allowlist = EnvVarGuard::set(WEB_EGRESS_ALLOWLIST_ENV_KEY, "127.0.0.1");
        let _proxy = EnvVarGuard::unset(WEB_EGRESS_PROXY_URL_ENV_KEY);

        let error = parse_and_validate_web_url("http://127.0.0.1/internal")
            .await
            .expect_err("strict mode should require proxy configuration");
        assert!(error.contains(WEB_EGRESS_PROXY_URL_ENV_KEY));
    }

    #[tokio::test]
    async fn strict_web_egress_allows_allowlisted_loopback_destinations() {
        let _env_lock = env_lock().lock().await;
        let _mode = EnvVarGuard::set(WEB_EGRESS_MODE_ENV_KEY, "strict");
        let _allowlist = EnvVarGuard::set(WEB_EGRESS_ALLOWLIST_ENV_KEY, "127.0.0.1");
        let _proxy = EnvVarGuard::set(WEB_EGRESS_PROXY_URL_ENV_KEY, "http://proxy.internal:8080");

        let parsed = parse_and_validate_web_url("http://127.0.0.1/internal")
            .await
            .expect("strict mode should allow explicitly allowlisted loopback destinations");
        assert_eq!(parsed.host_str(), Some("127.0.0.1"));
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
