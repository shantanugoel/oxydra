use reqwest::{Client, header::CONTENT_TYPE};
use serde_json::{Value, json};

use crate::wasm_runner::{
    parse_and_validate_web_url, read_response_bytes, request_with_default_headers,
    truncate_web_body,
};

const WEB_FETCH_MAX_RESPONSE_BYTES: usize = 5 * 1024 * 1024;
const WEB_FETCH_DEFAULT_MAX_BODY_CHARS: usize = 50_000;
const WEB_FETCH_ERROR_PREVIEW_BYTES: usize = 8 * 1024;
const HTML_TEXT_WIDTH: usize = 120;

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

pub(crate) async fn execute(http_client: &Client, arguments: &Value) -> Result<String, String> {
    let url = required_string(arguments, "url", "web_fetch")?;
    let output_format = FetchOutputFormat::parse(arguments.get("output_format"))?;
    let max_body_chars = parse_optional_usize(
        arguments.get("max_body_chars"),
        WEB_FETCH_DEFAULT_MAX_BODY_CHARS,
        "max_body_chars",
    )?;
    let parsed_url = parse_and_validate_web_url(&url).await?;
    let target_url = parsed_url.as_str().to_owned();
    let response = request_with_default_headers(http_client.get(parsed_url))
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
        let (error_body, _) = read_response_bytes(response, WEB_FETCH_ERROR_PREVIEW_BYTES).await?;
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
            url: target_url,
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
        extract_html_content(&body_bytes)
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

fn extract_html_content(bytes: &[u8]) -> (String, Option<String>) {
    let html = String::from_utf8_lossy(bytes);
    let title = extract_html_title(html.as_ref());
    let text = html2text::from_read(bytes, HTML_TEXT_WIDTH).unwrap_or_else(|_| html.to_string());
    (normalize_text(&text), title)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fetch_body_mode_auto_binary_returns_metadata_only() {
        let (mode, extract_html) = plan_fetch_body_mode(FetchOutputFormat::Auto, Some("image/png"))
            .expect("binary auto mode should be supported");
        assert_eq!(mode, FetchBodyMode::MetadataOnly);
        assert!(!extract_html);
    }

    #[test]
    fn extract_html_content_uses_html2text() {
        let html = b"<html><head><title>Example</title></head><body><h1>Hello</h1><p>World</p></body></html>";
        let (text, title) = extract_html_content(html);
        assert_eq!(title.as_deref(), Some("Example"));
        assert!(text.contains("Hello"));
        assert!(text.contains("World"));
        assert!(!text.contains("<h1>"));
    }
}
