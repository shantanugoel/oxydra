//! `send_media` tool — sends a workspace file as a media attachment through
//! the connected channel (Telegram, Discord, etc.).
//!
//! The tool reads the file from the workspace, emits a [`StreamItem::Media`]
//! through the `event_sender` on the [`ToolExecutionContext`], and returns a
//! confirmation message. The gateway forwards the stream item as a
//! [`GatewayServerFrame::MediaAttachment`] to the channel adapter, which
//! delivers it to the user.

use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use types::{
    FunctionDecl, MediaAttachment, MediaType, SafetyTier, StreamItem, Tool, ToolError,
    ToolExecutionContext,
};

pub const SEND_MEDIA_TOOL_NAME: &str = "send_media";

#[derive(Debug, Deserialize)]
struct SendMediaArgs {
    path: String,
    media_type: String,
    #[serde(default)]
    caption: Option<String>,
}

/// Tool that reads a file from the workspace and sends it as a media
/// attachment through the connected channel.
#[derive(Clone, Default)]
pub struct SendMediaTool;

impl SendMediaTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for SendMediaTool {
    fn schema(&self) -> FunctionDecl {
        FunctionDecl::new(
            SEND_MEDIA_TOOL_NAME,
            Some(
                "Send a file from the workspace as a media attachment to the user through the \
                 connected channel (e.g. Telegram). The file must exist in /shared, /tmp, or \
                 /vault. Only available when the channel supports rich media."
                    .to_owned(),
            ),
            json!({
                "type": "object",
                "required": ["path", "media_type"],
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Workspace file path (e.g. /shared/chart.png, /tmp/report.pdf)"
                    },
                    "media_type": {
                        "type": "string",
                        "enum": ["photo", "audio", "document", "voice", "video"],
                        "description": "Type of media: photo (images), audio (music/sounds), document (files/PDFs), voice (voice messages), video"
                    },
                    "caption": {
                        "type": "string",
                        "description": "Optional caption or description to accompany the media"
                    }
                }
            }),
        )
    }

    async fn execute(
        &self,
        args: &str,
        context: &ToolExecutionContext,
    ) -> Result<String, ToolError> {
        let request: SendMediaArgs =
            serde_json::from_str(args).map_err(|e| ToolError::InvalidArguments {
                tool: SEND_MEDIA_TOOL_NAME.to_owned(),
                message: e.to_string(),
            })?;

        // Validate channel supports media.
        let capabilities =
            context
                .channel_capabilities
                .as_ref()
                .ok_or_else(|| ToolError::ExecutionFailed {
                    tool: SEND_MEDIA_TOOL_NAME.to_owned(),
                    message: "send_media is not available: the current channel does not support \
                          rich media (text-only channel)"
                        .to_owned(),
                })?;

        let media_type = parse_media_type(&request.media_type)?;

        // Check that the specific media type is supported.
        let supported = match media_type {
            MediaType::Photo => capabilities.media.photo,
            MediaType::Audio => capabilities.media.audio,
            MediaType::Document => capabilities.media.document,
            MediaType::Voice => capabilities.media.voice,
            MediaType::Video => capabilities.media.video,
        };
        if !supported {
            return Err(ToolError::ExecutionFailed {
                tool: SEND_MEDIA_TOOL_NAME.to_owned(),
                message: format!(
                    "the connected channel ({}) does not support sending {} media",
                    capabilities.channel_type, request.media_type
                ),
            });
        }

        let event_sender =
            context
                .event_sender
                .as_ref()
                .ok_or_else(|| ToolError::ExecutionFailed {
                    tool: SEND_MEDIA_TOOL_NAME.to_owned(),
                    message: "send_media is not available: no event sender configured".to_owned(),
                })?;

        // Read the file contents using the WASM file-read tool. This ensures
        // path validation and sandboxing go through the same security policy
        // as other file tools.
        //
        // However, file_read returns text (UTF-8). For binary files we need
        // raw bytes. We'll use the WASM runner to invoke a raw-read that
        // returns bytes. For now, read the file at the workspace-resolved path.
        //
        // Since the `path` argument has already been translated from virtual to
        // host paths by the runtime's tool execution layer (translate_tool_arg_paths),
        // we can read directly.
        let file_data =
            tokio::fs::read(&request.path)
                .await
                .map_err(|e| ToolError::ExecutionFailed {
                    tool: SEND_MEDIA_TOOL_NAME.to_owned(),
                    message: format!("failed to read file `{}`: {}", request.path, e),
                })?;

        if file_data.is_empty() {
            return Err(ToolError::ExecutionFailed {
                tool: SEND_MEDIA_TOOL_NAME.to_owned(),
                message: format!("file `{}` is empty", request.path),
            });
        }

        // Extract file name from path.
        let file_name = std::path::Path::new(&request.path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned());

        let attachment = MediaAttachment {
            file_path: request.path.clone(),
            media_type,
            caption: request.caption.clone(),
            data: file_data,
            file_name,
        };

        // Emit through the event stream — the gateway will forward this to
        // the channel adapter.
        let _ = event_sender.send(StreamItem::Media(attachment));

        let caption_info = request
            .caption
            .as_deref()
            .map(|c| format!(" with caption \"{c}\""))
            .unwrap_or_default();

        Ok(format!(
            "Successfully sent {} `{}`{caption_info} to the user via {}.",
            request.media_type, request.path, capabilities.channel_type
        ))
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(30)
    }

    fn safety_tier(&self) -> SafetyTier {
        // ReadOnly because it reads a file and sends it — no workspace mutation.
        SafetyTier::ReadOnly
    }
}

fn parse_media_type(s: &str) -> Result<MediaType, ToolError> {
    match s {
        "photo" => Ok(MediaType::Photo),
        "audio" => Ok(MediaType::Audio),
        "document" => Ok(MediaType::Document),
        "voice" => Ok(MediaType::Voice),
        "video" => Ok(MediaType::Video),
        _ => Err(ToolError::InvalidArguments {
            tool: SEND_MEDIA_TOOL_NAME.to_owned(),
            message: format!(
                "invalid media_type `{s}`; must be one of: photo, audio, document, voice, video"
            ),
        }),
    }
}

/// Register the `send_media` tool in the given registry.
pub fn register_media_tools(registry: &mut crate::ToolRegistry) {
    registry.register(SEND_MEDIA_TOOL_NAME, SendMediaTool::new());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_media_type_valid() {
        assert_eq!(parse_media_type("photo").unwrap(), MediaType::Photo);
        assert_eq!(parse_media_type("audio").unwrap(), MediaType::Audio);
        assert_eq!(parse_media_type("document").unwrap(), MediaType::Document);
        assert_eq!(parse_media_type("voice").unwrap(), MediaType::Voice);
        assert_eq!(parse_media_type("video").unwrap(), MediaType::Video);
    }

    #[test]
    fn parse_media_type_invalid() {
        assert!(parse_media_type("image").is_err());
        assert!(parse_media_type("").is_err());
    }

    #[test]
    fn send_media_tool_schema_has_required_fields() {
        let tool = SendMediaTool::new();
        let schema = tool.schema();
        assert_eq!(schema.name, SEND_MEDIA_TOOL_NAME);
        let params = schema.parameters.as_object().unwrap();
        let required = params["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "path"));
        assert!(required.iter().any(|v| v == "media_type"));
    }

    #[tokio::test]
    async fn send_media_rejects_without_channel_capabilities() {
        let tool = SendMediaTool::new();
        let result = tool
            .execute(
                r#"{"path": "/shared/test.jpg", "media_type": "photo"}"#,
                &ToolExecutionContext::default(),
            )
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        match err {
            ToolError::ExecutionFailed { message, .. } => {
                assert!(message.contains("does not support rich media"));
            }
            _ => panic!("expected ExecutionFailed, got {err:?}"),
        }
    }

    #[tokio::test]
    async fn send_media_rejects_unsupported_media_type() {
        let tool = SendMediaTool::new();
        // Channel with no media capabilities.
        let ctx = ToolExecutionContext {
            channel_capabilities: Some(types::ChannelCapabilities::tui()),
            ..Default::default()
        };
        let result = tool
            .execute(
                r#"{"path": "/shared/test.jpg", "media_type": "photo"}"#,
                &ctx,
            )
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        match err {
            ToolError::ExecutionFailed { message, .. } => {
                assert!(message.contains("does not support sending photo"));
            }
            _ => panic!("expected ExecutionFailed, got {err:?}"),
        }
    }
}
