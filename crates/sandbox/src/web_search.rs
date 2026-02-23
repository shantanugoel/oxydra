use std::{
    collections::{BTreeMap, BTreeSet},
    env,
};

use reqwest::{Client, RequestBuilder, header::CONTENT_TYPE};
use serde_json::{Value, json};

use crate::wasm_runner::{
    parse_and_validate_web_url, read_response_bytes, request_with_default_headers,
    truncate_web_body,
};

const WEB_FETCH_ERROR_PREVIEW_BYTES: usize = 8 * 1024;
const WEB_SEARCH_DEFAULT_COUNT: usize = 5;
const WEB_SEARCH_MAX_COUNT: usize = 10;
const WEB_SEARCH_DEFAULT_MAX_SNIPPET_CHARS: usize = 2_000;
const WEB_SEARCH_RESPONSE_BYTES: usize = 1_024 * 1_024;
const GOOGLE_SEARCH_BASE_URL: &str = "https://www.googleapis.com/customsearch/v1";
const DUCKDUCKGO_SEARCH_BASE_URL: &str = "https://api.duckduckgo.com/";
const SEARXNG_SEARCH_PATH: &str = "/search";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchProviderKind {
    DuckDuckGo,
    Google,
    Searxng,
}

impl SearchProviderKind {
    fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "duckduckgo" => Ok(Self::DuckDuckGo),
            "google" => Ok(Self::Google),
            "searxng" => Ok(Self::Searxng),
            _ => Err(format!(
                "unsupported web search provider `{raw}`; expected duckduckgo, google, or searxng"
            )),
        }
    }

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
    query_params: BTreeMap<String, String>,
}

enum SearchPayload {
    Json(Value),
    Text(String),
}

pub(crate) async fn execute(http_client: &Client, arguments: &Value) -> Result<String, String> {
    let query = required_string(arguments, "query", "web_search")?;
    let query = query.trim().to_owned();
    if query.is_empty() {
        return Err("query must not be empty".to_owned());
    }
    let count = parse_optional_usize(arguments.get("count"), WEB_SEARCH_DEFAULT_COUNT, "count")?
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
    let (base_url, payload) = fetch_search_payload_with_fallbacks(
        http_client,
        &provider,
        &query,
        count,
        freshness.as_deref(),
    )
    .await?;
    let results = parse_search_results(&provider, &query, payload, count)?;
    Ok(json!({
        "query": query,
        "provider": provider.kind.as_str(),
        "base_url": base_url,
        "result_count": results.len(),
        "results": results
    })
    .to_string())
}

fn required_string(arguments: &Value, field: &str, tool_name: &str) -> Result<String, String> {
    arguments
        .get(field)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("tool `{tool_name}` requires string argument `{field}`"))
}

fn optional_string(arguments: &Value, field: &str) -> Option<String> {
    arguments
        .get(field)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn parse_optional_usize(
    value: Option<&Value>,
    default: usize,
    field_name: &str,
) -> Result<usize, String> {
    match value {
        // absent or explicitly null both mean "use the default"
        None | Some(Value::Null) => Ok(default),
        Some(value) => {
            let raw = value
                .as_u64()
                .ok_or_else(|| format!("`{field_name}` must be a non-negative integer"))?;
            usize::try_from(raw)
                .map_err(|_| format!("`{field_name}` is too large for this platform"))
        }
    }
}

fn search_provider_from_env() -> Result<SearchProviderConfig, String> {
    let provider_name =
        env_non_empty("OXYDRA_WEB_SEARCH_PROVIDER").unwrap_or_else(|| "duckduckgo".to_owned());
    let kind = SearchProviderKind::parse(&provider_name)?;
    let base_urls = provider_base_urls_from_env(kind);
    if kind == SearchProviderKind::Searxng && base_urls.is_empty() {
        return Err(
            "searxng search provider requires OXYDRA_WEB_SEARCH_SEARXNG_BASE_URL or OXYDRA_WEB_SEARCH_SEARXNG_BASE_URLS"
                .to_owned(),
        );
    }

    let mut query_params = BTreeMap::new();
    if let Some(raw_query_params) = env_non_empty("OXYDRA_WEB_SEARCH_QUERY_PARAMS") {
        for entry in raw_query_params.split('&') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            let mut segments = entry.splitn(2, '=');
            let key = segments.next().unwrap_or_default().trim();
            let value = segments.next().unwrap_or_default().trim();
            if !key.is_empty() {
                query_params.insert(key.to_owned(), value.to_owned());
            }
        }
    }

    Ok(SearchProviderConfig {
        kind,
        base_urls,
        google_api_key: env_non_empty("OXYDRA_WEB_SEARCH_GOOGLE_API_KEY"),
        google_engine_id: env_non_empty("OXYDRA_WEB_SEARCH_GOOGLE_CX"),
        searxng_engines: env_non_empty("OXYDRA_WEB_SEARCH_SEARXNG_ENGINES"),
        searxng_categories: env_non_empty("OXYDRA_WEB_SEARCH_SEARXNG_CATEGORIES"),
        searxng_safesearch: env_non_empty("OXYDRA_WEB_SEARCH_SEARXNG_SAFESEARCH")
            .and_then(|value| value.parse::<u8>().ok()),
        query_params,
    })
}

fn provider_base_urls_from_env(kind: SearchProviderKind) -> Vec<String> {
    match kind {
        SearchProviderKind::DuckDuckGo => collect_base_urls(
            "OXYDRA_WEB_SEARCH_DUCKDUCKGO_BASE_URL",
            "OXYDRA_WEB_SEARCH_DUCKDUCKGO_BASE_URLS",
            Some(DUCKDUCKGO_SEARCH_BASE_URL),
        ),
        SearchProviderKind::Google => collect_base_urls(
            "OXYDRA_WEB_SEARCH_GOOGLE_BASE_URL",
            "OXYDRA_WEB_SEARCH_GOOGLE_BASE_URLS",
            Some(GOOGLE_SEARCH_BASE_URL),
        ),
        SearchProviderKind::Searxng => collect_base_urls(
            "OXYDRA_WEB_SEARCH_SEARXNG_BASE_URL",
            "OXYDRA_WEB_SEARCH_SEARXNG_BASE_URLS",
            None,
        ),
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
    dedupe_base_urls(entries)
}

fn dedupe_base_urls(base_urls: Vec<String>) -> Vec<String> {
    let mut deduped = Vec::new();
    let mut seen = BTreeSet::new();
    for entry in base_urls {
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

fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

async fn fetch_search_payload_with_fallbacks(
    http_client: &Client,
    provider: &SearchProviderConfig,
    query: &str,
    count: usize,
    freshness: Option<&str>,
) -> Result<(String, SearchPayload), String> {
    let mut last_error = None;
    for base_url in &provider.base_urls {
        match fetch_search_payload_from_base_url(
            http_client,
            provider,
            base_url,
            query,
            count,
            freshness,
        )
        .await
        {
            Ok(payload) => return Ok((base_url.clone(), payload)),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| "web search failed".to_owned()))
}

async fn fetch_search_payload_from_base_url(
    http_client: &Client,
    provider: &SearchProviderConfig,
    base_url: &str,
    query: &str,
    count: usize,
    freshness: Option<&str>,
) -> Result<SearchPayload, String> {
    let request = match provider.kind {
        SearchProviderKind::DuckDuckGo => {
            build_duckduckgo_search_request(http_client, provider, base_url, query)
        }
        SearchProviderKind::Google => {
            build_google_search_request(http_client, provider, base_url, query, count, freshness)?
        }
        SearchProviderKind::Searxng => {
            build_searxng_search_request(http_client, provider, base_url, query, count, freshness)
        }
    };
    let request_url = request
        .try_clone()
        .and_then(|candidate| candidate.build().ok())
        .map(|request| request.url().to_string())
        .unwrap_or_else(|| base_url.to_owned());

    parse_and_validate_web_url(&request_url).await?;

    let response = request
        .send()
        .await
        .map_err(|error| format!("web search request failed against `{base_url}`: {error}"))?;
    if !response.status().is_success() {
        let status = response.status();
        let (error_bytes, _) = read_response_bytes(response, WEB_FETCH_ERROR_PREVIEW_BYTES).await?;
        let preview = truncate_text(&String::from_utf8_lossy(&error_bytes), 512).0;
        return Err(format!(
            "web search request to `{base_url}` failed with status {status}: {}",
            truncate_web_body(&preview)
        ));
    }

    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(normalize_content_type);
    let (body_bytes, _) = read_response_bytes(response, WEB_SEARCH_RESPONSE_BYTES).await?;

    if is_json_content_type(content_type.as_deref()) || body_looks_like_json(&body_bytes) {
        let payload = serde_json::from_slice(&body_bytes).map_err(|error| {
            format!("web search response from `{base_url}` is not valid JSON: {error}")
        })?;
        return Ok(SearchPayload::Json(payload));
    }

    Ok(SearchPayload::Text(
        String::from_utf8_lossy(&body_bytes).to_string(),
    ))
}

fn is_json_content_type(content_type: Option<&str>) -> bool {
    content_type.is_some_and(|value| value == "application/json" || value.ends_with("+json"))
}

fn body_looks_like_json(bytes: &[u8]) -> bool {
    let first = bytes.iter().find(|byte| !byte.is_ascii_whitespace());
    matches!(first, Some(b'{') | Some(b'['))
}

fn build_duckduckgo_search_request(
    http_client: &Client,
    provider: &SearchProviderConfig,
    base_url: &str,
    query: &str,
) -> RequestBuilder {
    let mut params = vec![
        ("q".to_owned(), query.to_owned()),
        ("format".to_owned(), "json".to_owned()),
        ("no_html".to_owned(), "1".to_owned()),
        ("no_redirect".to_owned(), "1".to_owned()),
        ("skip_disambig".to_owned(), "1".to_owned()),
        ("t".to_owned(), "oxydra".to_owned()),
    ];
    merge_query_params(&mut params, &provider.query_params);
    request_with_default_headers(http_client.get(base_url)).query(&params)
}

fn build_google_search_request(
    http_client: &Client,
    provider: &SearchProviderConfig,
    base_url: &str,
    query: &str,
    count: usize,
    freshness: Option<&str>,
) -> Result<RequestBuilder, String> {
    let api_key = provider.google_api_key.as_ref().ok_or_else(|| {
        "google search provider requires OXYDRA_WEB_SEARCH_GOOGLE_API_KEY".to_owned()
    })?;
    let engine_id = provider
        .google_engine_id
        .as_ref()
        .ok_or_else(|| "google search provider requires OXYDRA_WEB_SEARCH_GOOGLE_CX".to_owned())?;
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
    merge_query_params(&mut params, &provider.query_params);
    Ok(request_with_default_headers(http_client.get(base_url)).query(&params))
}

fn build_searxng_search_request(
    http_client: &Client,
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
    merge_query_params(&mut params, &provider.query_params);
    request_with_default_headers(http_client.get(url)).query(&params)
}

fn merge_query_params(params: &mut Vec<(String, String)>, extras: &BTreeMap<String, String>) {
    for (key, value) in extras {
        if let Some(existing) = params
            .iter_mut()
            .find(|(existing_key, _)| existing_key == key)
        {
            existing.1 = value.clone();
        } else {
            params.push((key.clone(), value.clone()));
        }
    }
}

fn parse_search_results(
    provider: &SearchProviderConfig,
    query: &str,
    payload: SearchPayload,
    count: usize,
) -> Result<Vec<Value>, String> {
    match provider.kind {
        SearchProviderKind::Google => match payload {
            SearchPayload::Json(payload) => Ok(parse_google_search_results(&payload, count)),
            SearchPayload::Text(_) => Err(
                "google search did not return JSON response; verify provider endpoint".to_owned(),
            ),
        },
        SearchProviderKind::DuckDuckGo => match payload {
            SearchPayload::Json(payload) => {
                Ok(parse_duckduckgo_search_results(query, &payload, count))
            }
            SearchPayload::Text(_) => Err(
                "duckduckgo search did not return JSON response; verify provider endpoint"
                    .to_owned(),
            ),
        },
        SearchProviderKind::Searxng => match payload {
            SearchPayload::Json(payload) => Ok(parse_searxng_search_results(&payload, count)),
            SearchPayload::Text(payload) => {
                let results = parse_searxng_html_results(&payload, count);
                if results.is_empty() {
                    Err("searxng returned non-JSON response with no parseable link results; set config.query_params.format=json".to_owned())
                } else {
                    Ok(results)
                }
            }
        },
    }
}

fn normalize_content_type(value: &str) -> String {
    value
        .split(';')
        .next()
        .unwrap_or(value)
        .trim()
        .to_ascii_lowercase()
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

fn parse_searxng_html_results(html: &str, count: usize) -> Vec<Value> {
    let mut results = Vec::new();
    let mut seen_urls = BTreeSet::new();
    let lower = html.to_ascii_lowercase();
    let mut cursor = 0usize;

    while let Some(anchor_start_rel) = lower[cursor..].find("<a") {
        if results.len() >= count {
            break;
        }
        let anchor_start = cursor + anchor_start_rel;
        let Some(tag_end_rel) = lower[anchor_start..].find('>') else {
            break;
        };
        let tag_end = anchor_start + tag_end_rel;
        let tag = &html[anchor_start..=tag_end];
        let Some(href) = extract_html_attribute(tag, "href") else {
            cursor = tag_end + 1;
            continue;
        };
        if !href.starts_with("http://") && !href.starts_with("https://") {
            cursor = tag_end + 1;
            continue;
        }
        let Some(close_rel) = lower[tag_end + 1..].find("</a>") else {
            break;
        };
        let close_start = tag_end + 1 + close_rel;
        let anchor_text = strip_html_tags(&html[tag_end + 1..close_start]);
        let title = collapse_whitespace(&decode_html_entities(anchor_text.trim()));
        if title.is_empty() || !seen_urls.insert(href.clone()) {
            cursor = close_start + 4;
            continue;
        }
        results.push(json!({
            "index": results.len() + 1,
            "title": title,
            "url": href,
            "display_url": Value::Null,
            "snippet": clean_search_snippet(&anchor_text)
        }));
        cursor = close_start + 4;
    }
    results
}

fn extract_html_attribute(tag: &str, attribute: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let needle = format!("{attribute}=");
    let start = lower.find(&needle)? + needle.len();
    if start >= tag.len() {
        return None;
    }

    let bytes = tag.as_bytes();
    let quote = bytes[start] as char;
    if quote == '"' || quote == '\'' {
        let value_start = start + 1;
        let remainder = &tag[value_start..];
        let value_end = remainder.find(quote)?;
        return non_empty(Some(remainder[..value_end].trim().to_owned()));
    }

    let remainder = &tag[start..];
    let value_end = remainder
        .find(|character: char| character.is_whitespace() || character == '>')
        .unwrap_or(remainder.len());
    non_empty(Some(remainder[..value_end].trim().to_owned()))
}

fn strip_html_tags(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut in_tag = false;
    for character in value.chars() {
        match character {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => output.push(character),
            _ => {}
        }
    }
    output
}

fn clean_search_snippet(value: &str) -> String {
    let without_tags = strip_html_tags(value);
    let decoded = decode_html_entities(&without_tags);
    let collapsed = collapse_whitespace(&decoded);
    truncate_text(&collapsed, WEB_SEARCH_DEFAULT_MAX_SNIPPET_CHARS).0
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

fn collapse_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
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

#[cfg(test)]
mod tests {
    use super::*;

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
            results[1].get("url").and_then(Value::as_str),
            Some("https://example.com/topic-1")
        );
        assert_eq!(
            results[2].get("url").and_then(Value::as_str),
            Some("https://example.com/topic-2")
        );
    }

    #[test]
    fn searxng_html_parser_extracts_anchor_results() {
        let html = r#"
            <html><body>
              <a href="https://example.com/one">Result One</a>
              <div><a href="https://example.com/two">Result <b>Two</b></a></div>
            </body></html>
        "#;
        let results = parse_searxng_html_results(html, 10);
        assert_eq!(results.len(), 2);
        assert_eq!(
            results[0].get("url").and_then(Value::as_str),
            Some("https://example.com/one")
        );
        assert_eq!(
            results[1].get("title").and_then(Value::as_str),
            Some("Result Two")
        );
    }

    #[test]
    fn parse_optional_usize_treats_null_as_absent() {
        // LLMs (especially Gemini) often send `null` for optional parameters.
        // `null` must be treated the same as absent — use the default — not
        // rejected as "must be a non-negative integer".
        assert_eq!(
            parse_optional_usize(None, 5, "count").unwrap(),
            5,
            "absent field uses default"
        );
        assert_eq!(
            parse_optional_usize(Some(&Value::Null), 5, "count").unwrap(),
            5,
            "explicit null uses default"
        );
        assert_eq!(
            parse_optional_usize(Some(&json!(3u64)), 5, "count").unwrap(),
            3,
            "explicit integer is used as-is"
        );
        assert!(
            parse_optional_usize(Some(&json!(-1i64)), 5, "count").is_err(),
            "negative number is rejected"
        );
        assert!(
            parse_optional_usize(Some(&json!("three")), 5, "count").is_err(),
            "string is rejected"
        );
    }
}
