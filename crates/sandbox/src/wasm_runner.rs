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
use reqwest::{
    Client, RequestBuilder,
    header::{ACCEPT, ACCEPT_LANGUAGE, CONTENT_TYPE, USER_AGENT},
};
use serde_json::{Value, json};

use crate::SandboxError;

const SHARED_DIR_NAME: &str = "shared";
const TMP_DIR_NAME: &str = "tmp";
const VAULT_DIR_NAME: &str = "vault";
const MAX_SEARCH_MATCHES: usize = 200;
const WEB_FETCH_MAX_RESPONSE_BYTES: usize = 5 * 1024 * 1024;
const WEB_FETCH_DEFAULT_MAX_BODY_CHARS: usize = 50_000;
const WEB_FETCH_ERROR_PREVIEW_BYTES: usize = 8 * 1024;
const WEB_SEARCH_DEFAULT_COUNT: usize = 5;
const WEB_SEARCH_MAX_COUNT: usize = 10;
const WEB_SEARCH_DEFAULT_MAX_SNIPPET_CHARS: usize = 2_000;
const WEB_SEARCH_RESPONSE_BYTES: usize = 1_024 * 1_024;
const GOOGLE_SEARCH_BASE_URL: &str = "https://www.googleapis.com/customsearch/v1";
const DUCKDUCKGO_SEARCH_BASE_URL: &str = "https://api.duckduckgo.com/";
const SEARXNG_SEARCH_PATH: &str = "/search";
const WEB_TOOL_USER_AGENT: &str = "oxydra/0.1 (+https://github.com/shantanugoel/oxydra)";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FetchOutputFormat {
    Auto,
    Raw,
    Text,
    MetadataOnly,
}

impl FetchOutputFormat {
    fn parse(raw: Option<&Value>) -> Result<Self, String> {
        let value = raw.and_then(Value::as_str).unwrap_or("auto");
        match value.to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "raw" => Ok(Self::Raw),
            "text" => Ok(Self::Text),
            "metadata_only" => Ok(Self::MetadataOnly),
            _ => Err(format!(
                "invalid output_format `{value}`; expected one of auto, raw, text, metadata_only"
            )),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Raw => "raw",
            Self::Text => "text",
            Self::MetadataOnly => "metadata_only",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FetchBodyMode {
    Raw,
    Text,
    MetadataOnly,
}

impl FetchBodyMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Raw => "raw",
            Self::Text => "text",
            Self::MetadataOnly => "metadata_only",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchProviderKind {
    DuckDuckGo,
    Google,
    Searxng,
}

impl SearchProviderKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::DuckDuckGo => "duckduckgo",
            Self::Google => "google",
            Self::Searxng => "searxng",
        }
    }
}

#[derive(Debug, Clone)]
struct SearchProviderConfig {
    kind: SearchProviderKind,
    base_urls: Vec<String>,
    google_api_key: Option<String>,
    google_engine_id: Option<String>,
    searxng_engines: Option<String>,
    searxng_categories: Option<String>,
    searxng_safesearch: Option<u8>,
    allow_private_base_urls: bool,
}

#[derive(Debug)]
struct WebFetchResponsePayload {
    url: String,
    status: u16,
    content_type: Option<String>,
    content_length: Option<u64>,
    output_format: FetchOutputFormat,
    mode: FetchBodyMode,
    body: Option<String>,
    truncated: bool,
    title: Option<String>,
    note: Option<String>,
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
        let output_format = FetchOutputFormat::parse(arguments.get("output_format"))?;
        let max_body_chars = parse_optional_usize(
            arguments.get("max_body_chars"),
            WEB_FETCH_DEFAULT_MAX_BODY_CHARS,
            "max_body_chars",
        )?;
        let parsed_url = parse_and_validate_web_url(&url).await?;
        let target_url = parsed_url.as_str().to_owned();
        let response = self
            .request_with_default_headers(self.http_client.get(parsed_url))
            .send()
            .await
            .map_err(|error| format!("web fetch request failed for `{target_url}`: {error}"))?;
        let status = response.status();
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        let normalized_content_type = content_type.as_deref().map(normalize_content_type);
        let content_length = response.content_length();

        if !status.is_success() {
            let (error_body, _) =
                read_response_bytes(response, WEB_FETCH_ERROR_PREVIEW_BYTES).await?;
            let preview = truncate_text(&String::from_utf8_lossy(&error_body), 512).0;
            return Err(format!(
                "web fetch returned status {status} for `{target_url}`: {}",
                truncate_web_body(&preview)
            ));
        }

        let (mode, extract_html) =
            plan_fetch_body_mode(output_format, normalized_content_type.as_deref())?;
        if mode == FetchBodyMode::MetadataOnly {
            let note = if output_format == FetchOutputFormat::MetadataOnly {
                Some("metadata_only requested; body omitted".to_owned())
            } else {
                Some("binary content omitted to reduce token usage".to_owned())
            };
            return Ok(build_web_fetch_response(WebFetchResponsePayload {
                url: target_url.clone(),
                status: status.as_u16(),
                content_type,
                content_length,
                output_format,
                mode,
                body: None,
                truncated: false,
                title: None,
                note,
            }));
        }

        let (body_bytes, truncated_by_bytes) =
            read_response_bytes(response, WEB_FETCH_MAX_RESPONSE_BYTES).await?;
        let (text, title) = if mode == FetchBodyMode::Text && extract_html {
            extract_html_text(&String::from_utf8_lossy(&body_bytes))
        } else {
            (String::from_utf8_lossy(&body_bytes).to_string(), None)
        };
        let (body, truncated_by_chars) = truncate_text(&text, max_body_chars);

        Ok(build_web_fetch_response(WebFetchResponsePayload {
            url: target_url,
            status: status.as_u16(),
            content_type,
            content_length,
            output_format,
            mode,
            body: Some(body),
            truncated: truncated_by_bytes || truncated_by_chars,
            title,
            note: None,
        }))
    }

    async fn web_search(&self, arguments: &Value) -> Result<String, String> {
        let query = required_string(arguments, "query", "web_search")?;
        let query = query.trim().to_owned();
        if query.is_empty() {
            return Err("query must not be empty".to_owned());
        }
        let count =
            parse_optional_usize(arguments.get("count"), WEB_SEARCH_DEFAULT_COUNT, "count")?
                .clamp(1, WEB_SEARCH_MAX_COUNT);
        let freshness = optional_string(arguments, "freshness");
        if let Some(value) = freshness.as_deref()
            && !matches!(value, "day" | "week" | "month" | "year")
        {
            return Err(format!(
                "invalid freshness `{value}`; expected one of day, week, month, year"
            ));
        }

        let provider = search_provider_from_env()?;
        let (base_url, payload) = self
            .fetch_search_payload_with_fallbacks(&provider, &query, count, freshness.as_deref())
            .await?;
        let results = parse_search_results(provider.kind, &query, &payload, count);
        Ok(json!({
            "query": query,
            "provider": provider.kind.as_str(),
            "base_url": base_url,
            "result_count": results.len(),
            "results": results
        })
        .to_string())
    }

    async fn fetch_search_payload_with_fallbacks(
        &self,
        provider: &SearchProviderConfig,
        query: &str,
        count: usize,
        freshness: Option<&str>,
    ) -> Result<(String, Value), String> {
        let mut last_error = None;
        for base_url in &provider.base_urls {
            match self
                .fetch_search_payload_from_base_url(provider, base_url, query, count, freshness)
                .await
            {
                Ok(payload) => return Ok((base_url.clone(), payload)),
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.unwrap_or_else(|| "web search failed".to_owned()))
    }

    async fn fetch_search_payload_from_base_url(
        &self,
        provider: &SearchProviderConfig,
        base_url: &str,
        query: &str,
        count: usize,
        freshness: Option<&str>,
    ) -> Result<Value, String> {
        let request = match provider.kind {
            SearchProviderKind::DuckDuckGo => self.build_duckduckgo_search_request(base_url, query),
            SearchProviderKind::Google => {
                self.build_google_search_request(provider, base_url, query, count, freshness)?
            }
            SearchProviderKind::Searxng => {
                self.build_searxng_search_request(provider, base_url, query, count, freshness)
            }
        };
        let request_url = request
            .try_clone()
            .and_then(|candidate| candidate.build().ok())
            .map(|request| request.url().to_string())
            .unwrap_or_else(|| base_url.to_owned());

        if !provider.allow_private_base_urls {
            parse_and_validate_web_url(&request_url).await?;
        }

        let response = request
            .send()
            .await
            .map_err(|error| format!("web search request failed against `{base_url}`: {error}"))?;
        if !response.status().is_success() {
            let status = response.status();
            let (error_bytes, _) =
                read_response_bytes(response, WEB_FETCH_ERROR_PREVIEW_BYTES).await?;
            let preview = truncate_text(&String::from_utf8_lossy(&error_bytes), 512).0;
            return Err(format!(
                "web search request to `{base_url}` failed with status {status}: {}",
                truncate_web_body(&preview)
            ));
        }

        let (body_bytes, _) = read_response_bytes(response, WEB_SEARCH_RESPONSE_BYTES).await?;
        serde_json::from_slice(&body_bytes).map_err(|error| {
            format!("web search response from `{base_url}` is not valid JSON: {error}")
        })
    }

    fn build_duckduckgo_search_request(&self, base_url: &str, query: &str) -> RequestBuilder {
        let params = vec![
            ("q".to_owned(), query.to_owned()),
            ("format".to_owned(), "json".to_owned()),
            ("no_html".to_owned(), "1".to_owned()),
            ("no_redirect".to_owned(), "1".to_owned()),
            ("skip_disambig".to_owned(), "1".to_owned()),
            ("t".to_owned(), "oxydra".to_owned()),
        ];
        self.request_with_default_headers(self.http_client.get(base_url))
            .query(&params)
    }

    fn build_google_search_request(
        &self,
        provider: &SearchProviderConfig,
        base_url: &str,
        query: &str,
        count: usize,
        freshness: Option<&str>,
    ) -> Result<RequestBuilder, String> {
        let api_key = provider.google_api_key.as_ref().ok_or_else(|| {
            "google search provider requires OXYDRA_WEB_SEARCH_GOOGLE_API_KEY".to_owned()
        })?;
        let engine_id = provider.google_engine_id.as_ref().ok_or_else(|| {
            "google search provider requires OXYDRA_WEB_SEARCH_GOOGLE_CX".to_owned()
        })?;
        let mut params = vec![
            ("key".to_owned(), api_key.clone()),
            ("cx".to_owned(), engine_id.clone()),
            ("q".to_owned(), query.to_owned()),
            ("num".to_owned(), count.to_string()),
        ];
        if let Some(freshness) = freshness {
            let sort_value = match freshness {
                "day" => Some("date:r:1d"),
                "week" => Some("date:r:7d"),
                "month" => Some("date:r:30d"),
                "year" => Some("date:r:365d"),
                _ => None,
            };
            if let Some(sort_value) = sort_value {
                params.push(("sort".to_owned(), sort_value.to_owned()));
            }
        }
        Ok(self
            .request_with_default_headers(self.http_client.get(base_url))
            .query(&params))
    }

    fn build_searxng_search_request(
        &self,
        provider: &SearchProviderConfig,
        base_url: &str,
        query: &str,
        count: usize,
        freshness: Option<&str>,
    ) -> RequestBuilder {
        let mut url = base_url.trim_end_matches('/').to_owned();
        url.push_str(SEARXNG_SEARCH_PATH);

        let mut params = vec![
            ("q".to_owned(), query.to_owned()),
            ("format".to_owned(), "json".to_owned()),
            ("pageno".to_owned(), "1".to_owned()),
            ("count".to_owned(), count.to_string()),
        ];
        if let Some(engines) = provider.searxng_engines.as_deref() {
            params.push(("engines".to_owned(), engines.to_owned()));
        }
        if let Some(categories) = provider.searxng_categories.as_deref() {
            params.push(("categories".to_owned(), categories.to_owned()));
        }
        if let Some(safesearch) = provider.searxng_safesearch {
            params.push(("safesearch".to_owned(), safesearch.to_string()));
        }
        if let Some(freshness) = freshness {
            let time_range = match freshness {
                "day" => Some("day"),
                "week" => Some("week"),
                "month" => Some("month"),
                "year" => Some("year"),
                _ => None,
            };
            if let Some(time_range) = time_range {
                params.push(("time_range".to_owned(), time_range.to_owned()));
            }
        }
        self.request_with_default_headers(self.http_client.get(url))
            .query(&params)
    }

    fn request_with_default_headers(&self, request: RequestBuilder) -> RequestBuilder {
        request
            .header(USER_AGENT, WEB_TOOL_USER_AGENT)
            .header(
                ACCEPT,
                "application/json,text/html,application/xhtml+xml,text/plain;q=0.9,*/*;q=0.8",
            )
            .header(ACCEPT_LANGUAGE, "en-US,en;q=0.9")
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

fn parse_optional_usize(
    value: Option<&Value>,
    default: usize,
    field_name: &str,
) -> Result<usize, String> {
    if let Some(value) = value {
        let raw = value
            .as_u64()
            .ok_or_else(|| format!("`{field_name}` must be a non-negative integer"))?;
        return usize::try_from(raw)
            .map_err(|_| format!("`{field_name}` is too large for this platform"));
    }
    Ok(default)
}

fn normalize_content_type(value: &str) -> String {
    value
        .split(';')
        .next()
        .unwrap_or(value)
        .trim()
        .to_ascii_lowercase()
}

fn is_html_content_type(content_type: &str) -> bool {
    matches!(content_type, "text/html" | "application/xhtml+xml")
}

fn is_text_content_type(content_type: &str) -> bool {
    if content_type.starts_with("text/") {
        return true;
    }
    if matches!(
        content_type,
        "application/json"
            | "application/xml"
            | "application/xhtml+xml"
            | "application/javascript"
            | "application/x-www-form-urlencoded"
            | "text/xml"
            | "text/javascript"
    ) {
        return true;
    }
    content_type.ends_with("+json") || content_type.ends_with("+xml")
}

fn is_binary_content_type(content_type: &str) -> bool {
    if is_text_content_type(content_type) {
        return false;
    }
    content_type.starts_with("image/")
        || content_type.starts_with("audio/")
        || content_type.starts_with("video/")
        || content_type.starts_with("application/")
}

fn plan_fetch_body_mode(
    requested: FetchOutputFormat,
    content_type: Option<&str>,
) -> Result<(FetchBodyMode, bool), String> {
    if requested == FetchOutputFormat::MetadataOnly {
        return Ok((FetchBodyMode::MetadataOnly, false));
    }

    if let Some(content_type) = content_type
        && is_binary_content_type(content_type)
    {
        if requested == FetchOutputFormat::Text {
            return Err("binary content is not supported for text output".to_owned());
        }
        if requested == FetchOutputFormat::Auto {
            return Ok((FetchBodyMode::MetadataOnly, false));
        }
    }

    if requested == FetchOutputFormat::Raw {
        return Ok((FetchBodyMode::Raw, false));
    }

    if let Some(content_type) = content_type
        && is_html_content_type(content_type)
    {
        return Ok((FetchBodyMode::Text, true));
    }

    if let Some(content_type) = content_type
        && is_text_content_type(content_type)
    {
        return Ok((FetchBodyMode::Text, false));
    }

    match requested {
        FetchOutputFormat::Text => Ok((FetchBodyMode::Text, false)),
        _ => Ok((FetchBodyMode::Raw, false)),
    }
}

async fn read_response_bytes(
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

fn build_web_fetch_response(payload: WebFetchResponsePayload) -> String {
    let mut payload = json!({
        "url": payload.url,
        "status": payload.status,
        "output_format": payload.output_format.as_str(),
        "mode": payload.mode.as_str(),
        "content_type": payload.content_type,
        "content_length": payload.content_length,
        "body": payload.body,
        "truncated": payload.truncated,
        "title": payload.title,
        "note": payload.note
    });
    if let Some(object) = payload.as_object_mut() {
        object.retain(|_, value| !value.is_null());
        if !object
            .get("truncated")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            object.remove("truncated");
        }
    }
    payload.to_string()
}

fn extract_html_text(html: &str) -> (String, Option<String>) {
    let title = extract_html_title(html);
    let without_script = strip_enclosed_html_tag(html, "script");
    let without_style = strip_enclosed_html_tag(&without_script, "style");
    let without_noscript = strip_enclosed_html_tag(&without_style, "noscript");
    let stripped = strip_html_tags_with_breaks(&without_noscript);
    let decoded = decode_html_entities(&stripped);
    (normalize_text(&decoded), title)
}

fn extract_html_title(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let start = lower.find("<title")?;
    let start_tag_end = lower[start..].find('>')? + start + 1;
    let end = lower[start_tag_end..].find("</title>")? + start_tag_end;
    let title = html[start_tag_end..end].trim();
    if title.is_empty() {
        None
    } else {
        Some(title.to_owned())
    }
}

fn strip_enclosed_html_tag(input: &str, tag_name: &str) -> String {
    let lower = input.to_ascii_lowercase();
    let open = format!("<{tag_name}");
    let close = format!("</{tag_name}>");
    let mut cursor = 0usize;
    let mut output = String::with_capacity(input.len());
    while let Some(start_rel) = lower[cursor..].find(&open) {
        let start = cursor + start_rel;
        output.push_str(&input[cursor..start]);
        let Some(end_rel) = lower[start..].find(&close) else {
            cursor = input.len();
            break;
        };
        cursor = start + end_rel + close.len();
    }
    output.push_str(&input[cursor..]);
    output
}

fn strip_html_tags_with_breaks(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut in_tag = false;
    let mut current_tag = String::new();
    for character in input.chars() {
        if in_tag {
            if character == '>' {
                let tag_name = current_tag
                    .trim_start_matches('/')
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if matches!(
                    tag_name.as_str(),
                    "br" | "p"
                        | "div"
                        | "li"
                        | "ul"
                        | "ol"
                        | "section"
                        | "article"
                        | "header"
                        | "footer"
                        | "h1"
                        | "h2"
                        | "h3"
                        | "h4"
                        | "h5"
                        | "h6"
                        | "tr"
                ) {
                    output.push('\n');
                }
                in_tag = false;
                current_tag.clear();
            } else {
                current_tag.push(character);
            }
            continue;
        }

        if character == '<' {
            in_tag = true;
            continue;
        }
        output.push(character);
    }
    output
}

fn decode_html_entities(value: &str) -> String {
    value
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

fn normalize_text(value: &str) -> String {
    let mut output = String::new();
    let mut last_blank = false;
    for line in value.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if !last_blank && !output.is_empty() {
                output.push('\n');
                output.push('\n');
                last_blank = true;
            }
            continue;
        }
        if !output.is_empty() && !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str(trimmed);
        output.push('\n');
        last_blank = false;
    }
    output.trim().to_owned()
}

fn truncate_text(value: &str, max_chars: usize) -> (String, bool) {
    if max_chars == 0 {
        return (String::new(), true);
    }

    let mut count = 0usize;
    let mut end = value.len();
    for (index, _) in value.char_indices() {
        if count == max_chars {
            end = index;
            break;
        }
        count += 1;
    }

    if count < max_chars || end == value.len() {
        return (value.to_owned(), false);
    }

    let mut output = value[..end].to_owned();
    output.push_str("\n\n[truncated]");
    (output, true)
}

fn collapse_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn clean_search_snippet(value: &str) -> String {
    let without_tags = strip_html_tags_with_breaks(value);
    let decoded = decode_html_entities(&without_tags);
    let collapsed = collapse_whitespace(&decoded);
    truncate_text(&collapsed, WEB_SEARCH_DEFAULT_MAX_SNIPPET_CHARS).0
}

fn search_provider_from_env() -> Result<SearchProviderConfig, String> {
    let provider_name = env_non_empty("OXYDRA_WEB_SEARCH_PROVIDER")
        .unwrap_or_else(|| "duckduckgo".to_owned())
        .to_ascii_lowercase();
    let allow_private_base_urls = env_flag("OXYDRA_WEB_SEARCH_ALLOW_PRIVATE_BASE_URLS");
    match provider_name.as_str() {
        "duckduckgo" => Ok(SearchProviderConfig {
            kind: SearchProviderKind::DuckDuckGo,
            base_urls: collect_base_urls(
                "OXYDRA_WEB_SEARCH_DUCKDUCKGO_BASE_URL",
                "OXYDRA_WEB_SEARCH_DUCKDUCKGO_BASE_URLS",
                Some(DUCKDUCKGO_SEARCH_BASE_URL),
            ),
            google_api_key: None,
            google_engine_id: None,
            searxng_engines: None,
            searxng_categories: None,
            searxng_safesearch: None,
            allow_private_base_urls,
        }),
        "google" => Ok(SearchProviderConfig {
            kind: SearchProviderKind::Google,
            base_urls: collect_base_urls(
                "OXYDRA_WEB_SEARCH_GOOGLE_BASE_URL",
                "OXYDRA_WEB_SEARCH_GOOGLE_BASE_URLS",
                Some(GOOGLE_SEARCH_BASE_URL),
            ),
            google_api_key: env_non_empty("OXYDRA_WEB_SEARCH_GOOGLE_API_KEY"),
            google_engine_id: env_non_empty("OXYDRA_WEB_SEARCH_GOOGLE_CX"),
            searxng_engines: None,
            searxng_categories: None,
            searxng_safesearch: None,
            allow_private_base_urls,
        }),
        "searxng" => {
            let base_urls = collect_base_urls(
                "OXYDRA_WEB_SEARCH_SEARXNG_BASE_URL",
                "OXYDRA_WEB_SEARCH_SEARXNG_BASE_URLS",
                None,
            );
            if base_urls.is_empty() {
                return Err(
                    "searxng search provider requires OXYDRA_WEB_SEARCH_SEARXNG_BASE_URL or OXYDRA_WEB_SEARCH_SEARXNG_BASE_URLS"
                        .to_owned(),
                );
            }
            let searxng_safesearch = env_non_empty("OXYDRA_WEB_SEARCH_SEARXNG_SAFESEARCH")
                .and_then(|value| value.parse::<u8>().ok());
            Ok(SearchProviderConfig {
                kind: SearchProviderKind::Searxng,
                base_urls,
                google_api_key: None,
                google_engine_id: None,
                searxng_engines: env_non_empty("OXYDRA_WEB_SEARCH_SEARXNG_ENGINES"),
                searxng_categories: env_non_empty("OXYDRA_WEB_SEARCH_SEARXNG_CATEGORIES"),
                searxng_safesearch,
                allow_private_base_urls,
            })
        }
        _ => Err(format!(
            "unsupported web search provider `{provider_name}`; expected duckduckgo, google, or searxng"
        )),
    }
}

fn collect_base_urls(single_env: &str, list_env: &str, default: Option<&str>) -> Vec<String> {
    let mut entries = Vec::new();
    if let Some(single) = env_non_empty(single_env) {
        entries.push(single);
    }
    if let Some(multiple) = env_non_empty(list_env) {
        for entry in multiple.split(',') {
            let trimmed = entry.trim();
            if !trimmed.is_empty() {
                entries.push(trimmed.to_owned());
            }
        }
    }
    if entries.is_empty()
        && let Some(default) = default
    {
        entries.push(default.to_owned());
    }

    let mut deduped = Vec::new();
    let mut seen = BTreeSet::new();
    for entry in entries {
        let normalized = entry.trim().trim_end_matches('/').to_owned();
        if normalized.is_empty() {
            continue;
        }
        if seen.insert(normalized.clone()) {
            deduped.push(normalized);
        }
    }
    deduped
}

fn env_non_empty(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn env_flag(name: &str) -> bool {
    env_non_empty(name)
        .map(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn parse_search_results(
    provider: SearchProviderKind,
    query: &str,
    payload: &Value,
    count: usize,
) -> Vec<Value> {
    match provider {
        SearchProviderKind::Google => parse_google_search_results(payload, count),
        SearchProviderKind::Searxng => parse_searxng_search_results(payload, count),
        SearchProviderKind::DuckDuckGo => parse_duckduckgo_search_results(query, payload, count),
    }
}

fn parse_google_search_results(payload: &Value, count: usize) -> Vec<Value> {
    let Some(items) = payload.get("items").and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .take(count)
        .enumerate()
        .map(|(index, item)| {
            let snippet = item
                .get("snippet")
                .and_then(Value::as_str)
                .or_else(|| item.get("htmlSnippet").and_then(Value::as_str))
                .map(clean_search_snippet)
                .unwrap_or_default();
            json!({
                "index": index + 1,
                "title": item.get("title").and_then(Value::as_str).unwrap_or(""),
                "url": item.get("link").and_then(Value::as_str).unwrap_or(""),
                "display_url": item.get("displayLink").and_then(Value::as_str),
                "snippet": snippet
            })
        })
        .collect()
}

fn parse_searxng_search_results(payload: &Value, count: usize) -> Vec<Value> {
    let Some(items) = payload.get("results").and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .take(count)
        .enumerate()
        .map(|(index, item)| {
            let snippet = item
                .get("content")
                .and_then(Value::as_str)
                .or_else(|| item.get("snippet").and_then(Value::as_str))
                .map(clean_search_snippet)
                .unwrap_or_default();
            json!({
                "index": index + 1,
                "title": item.get("title").and_then(Value::as_str).unwrap_or(""),
                "url": item.get("url").and_then(Value::as_str).unwrap_or(""),
                "display_url": item.get("pretty_url").and_then(Value::as_str),
                "snippet": snippet
            })
        })
        .collect()
}

fn parse_duckduckgo_search_results(query: &str, payload: &Value, count: usize) -> Vec<Value> {
    let mut rows = Vec::new();
    let mut seen_urls = BTreeSet::new();

    if let Some(url) = payload.get("AbstractURL").and_then(Value::as_str)
        && !url.trim().is_empty()
    {
        seen_urls.insert(url.to_owned());
        let heading = payload
            .get("Heading")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(query);
        let snippet = payload
            .get("AbstractText")
            .and_then(Value::as_str)
            .or_else(|| payload.get("Abstract").and_then(Value::as_str))
            .map(clean_search_snippet)
            .unwrap_or_default();
        rows.push(json!({
            "index": 0,
            "title": heading,
            "url": url,
            "display_url": payload.get("AbstractSource").and_then(Value::as_str),
            "snippet": snippet
        }));
    }

    if let Some(related_topics) = payload.get("RelatedTopics").and_then(Value::as_array) {
        collect_duckduckgo_related_topics(related_topics, &mut rows, &mut seen_urls, count);
    }

    rows.into_iter()
        .take(count)
        .enumerate()
        .map(|(index, mut row)| {
            if let Some(object) = row.as_object_mut() {
                object.insert("index".to_owned(), json!(index + 1));
            }
            row
        })
        .collect()
}

fn collect_duckduckgo_related_topics(
    topics: &[Value],
    rows: &mut Vec<Value>,
    seen_urls: &mut BTreeSet<String>,
    count: usize,
) {
    if rows.len() >= count {
        return;
    }

    for topic in topics {
        if rows.len() >= count {
            break;
        }

        if let Some(nested) = topic.get("Topics").and_then(Value::as_array) {
            collect_duckduckgo_related_topics(nested, rows, seen_urls, count);
            continue;
        }

        let Some(url) = topic.get("FirstURL").and_then(Value::as_str) else {
            continue;
        };
        if url.trim().is_empty() || !seen_urls.insert(url.to_owned()) {
            continue;
        }

        let text = topic
            .get("Text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_owned();
        let title = text.split(" - ").next().unwrap_or(&text).trim().to_owned();
        rows.push(json!({
            "index": 0,
            "title": if title.is_empty() { "DuckDuckGo result" } else { title.as_str() },
            "url": url,
            "display_url": Value::Null,
            "snippet": clean_search_snippet(&text)
        }));
    }
}

async fn parse_and_validate_web_url(raw_url: &str) -> Result<reqwest::Url, String> {
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
    let resolved_ips = resolve_host_ips(host, port).await?;
    enforce_web_target_ips(host, &resolved_ips)?;
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

fn enforce_web_target_ips(host: &str, resolved_ips: &[IpAddr]) -> Result<(), String> {
    for ip in resolved_ips {
        if let Some(reason) = blocked_ip_reason(*ip) {
            return Err(format!(
                "web target `{host}` resolved to blocked address `{ip}` ({reason})"
            ));
        }
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
        net::{IpAddr, Ipv4Addr},
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

    #[tokio::test]
    async fn web_fetch_blocks_metadata_endpoint_targets() {
        let workspace = unique_workspace("web-block-meta");
        let runner = HostWasmToolRunner::for_direct_workspace(&workspace);
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
        let workspace = unique_workspace("web-block-rfc1918");
        let runner = HostWasmToolRunner::for_direct_workspace(&workspace);
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
        let result = enforce_web_target_ips(
            "example.org",
            &[
                IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)),
                IpAddr::V4(Ipv4Addr::new(192, 168, 1, 8)),
            ],
        );
        assert!(matches!(
            result,
            Err(message)
                if message.contains("example.org")
                    && message.contains("192.168.1.8")
                    && message.contains("192.168.0.0/16")
        ));
    }

    #[test]
    fn fetch_body_mode_auto_binary_returns_metadata_only() {
        let (mode, extract_html) = plan_fetch_body_mode(FetchOutputFormat::Auto, Some("image/png"))
            .expect("binary auto mode should be supported");
        assert_eq!(mode, FetchBodyMode::MetadataOnly);
        assert!(!extract_html);
    }

    #[test]
    fn extract_html_text_removes_markup() {
        let html = "<html><head><title>Example</title><style>.x{}</style></head><body><h1>Hello</h1><script>secret()</script><p>World</p></body></html>";
        let (text, title) = extract_html_text(html);
        assert_eq!(title.as_deref(), Some("Example"));
        assert!(text.contains("Hello"));
        assert!(text.contains("World"));
        assert!(!text.contains("<h1>"));
        assert!(!text.contains("secret()"));
    }

    #[test]
    fn duckduckgo_parser_collects_abstract_and_related_topics() {
        let payload = json!({
            "Heading": "Oxydra",
            "AbstractURL": "https://example.com/oxydra",
            "AbstractText": "Main summary",
            "RelatedTopics": [
                {
                    "FirstURL": "https://example.com/topic-1",
                    "Text": "Topic One - Details"
                },
                {
                    "Name": "Group",
                    "Topics": [
                        {
                            "FirstURL": "https://example.com/topic-2",
                            "Text": "Topic Two - More"
                        }
                    ]
                }
            ]
        });
        let results = parse_duckduckgo_search_results("oxydra", &payload, 5);
        assert_eq!(results.len(), 3);
        assert_eq!(
            results[0].get("title").and_then(Value::as_str),
            Some("Oxydra")
        );
        assert_eq!(
            results[1].get("url").and_then(Value::as_str),
            Some("https://example.com/topic-1")
        );
        assert_eq!(
            results[2].get("url").and_then(Value::as_str),
            Some("https://example.com/topic-2")
        );
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
