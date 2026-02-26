use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::StartupStatusReport;
use crate::model::RuntimeProgressEvent;
use crate::{ChannelError, Response};

pub const GATEWAY_PROTOCOL_VERSION: u16 = 2;

// ---------------------------------------------------------------------------
// Channel capabilities — describes what a channel can do beyond plain text.
// ---------------------------------------------------------------------------

/// Describes the media-sending capabilities of the channel the user is
/// connected through. Used to inform the agent (via the system prompt) what
/// kinds of content it can deliver, and to gate the `send_media` tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ChannelCapabilities {
    /// Human-readable channel type name (e.g. "telegram", "discord", "tui").
    pub channel_type: String,
    /// Rich-media capabilities of the channel.
    #[serde(default)]
    pub media: MediaCapabilities,
}

/// Flags indicating which media types the channel supports sending.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct MediaCapabilities {
    /// Can send image files (JPEG, PNG, GIF, etc.).
    #[serde(default)]
    pub photo: bool,
    /// Can send audio files (MP3, OGG, etc.).
    #[serde(default)]
    pub audio: bool,
    /// Can send arbitrary document/file attachments.
    #[serde(default)]
    pub document: bool,
    /// Can send voice messages (OGG/OPUS).
    #[serde(default)]
    pub voice: bool,
    /// Can send video files (MP4, etc.).
    #[serde(default)]
    pub video: bool,
}

impl MediaCapabilities {
    /// Returns `true` if the channel supports sending at least one media type.
    pub fn any(&self) -> bool {
        self.photo || self.audio || self.document || self.voice || self.video
    }
}

impl ChannelCapabilities {
    /// Build capabilities for the TUI (text-only).
    pub fn tui() -> Self {
        Self {
            channel_type: "tui".to_owned(),
            media: MediaCapabilities::default(),
        }
    }

    /// Build capabilities for Telegram.
    pub fn telegram() -> Self {
        Self {
            channel_type: "telegram".to_owned(),
            media: MediaCapabilities {
                photo: true,
                audio: true,
                document: true,
                voice: true,
                video: true,
            },
        }
    }

    /// Map a channel origin string to its known capabilities.
    pub fn from_channel_origin(origin: &str) -> Self {
        match origin {
            "telegram" => Self::telegram(),
            _ => Self::tui(),
        }
    }

    /// Generate a human-readable description of media capabilities for
    /// inclusion in the system prompt.
    pub fn system_prompt_section(&self) -> Option<String> {
        if !self.media.any() {
            return None;
        }
        let mut types = Vec::new();
        if self.media.photo {
            types.push("photos/images (JPEG, PNG, GIF)");
        }
        if self.media.audio {
            types.push("audio files (MP3, OGG)");
        }
        if self.media.voice {
            types.push("voice messages (OGG/OPUS)");
        }
        if self.media.video {
            types.push("videos (MP4)");
        }
        if self.media.document {
            types.push("documents/files (PDF, CSV, TXT, etc.)");
        }
        let list = types.join(", ");
        Some(format!(
            "## Channel Media Capabilities\n\n\
             You are connected to the user via **{channel}** which supports sending rich media.\n\
             You can send the following media types directly to the user: {list}.\n\n\
             To send a file, first prepare it in the workspace (e.g. `/shared/output.png`) using \
             file tools or shell commands, then use the `send_media` tool to deliver it to the user.\n\
             The `send_media` tool takes a `path` (workspace file path), a `media_type` \
             (one of: photo, audio, document, voice, video), and an optional `caption`.\n\n\
             When the user asks for something that would be better as a file (charts, reports, \
             audio, images, etc.), prefer generating and sending the actual file rather than \
             just describing it in text.",
            channel = self.channel_type
        ))
    }
}

// ---------------------------------------------------------------------------
// Media attachment — a file to be delivered through the channel.
// ---------------------------------------------------------------------------

/// The type of media being sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaType {
    Photo,
    Audio,
    Document,
    Voice,
    Video,
}

/// A media attachment emitted by the `send_media` tool, to be delivered
/// through the connected channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaAttachment {
    /// Workspace path of the file (virtual, e.g. `/shared/photo.jpg`).
    pub file_path: String,
    /// What kind of media this is.
    pub media_type: MediaType,
    /// Optional caption/description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
    /// The raw file bytes (base64-encoded in JSON serialization).
    #[serde(with = "base64_bytes")]
    pub data: Vec<u8>,
    /// Original file name (for document attachments).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
}

/// Serde helper for encoding `Vec<u8>` as base64 strings in JSON.
pub(crate) mod base64_bytes {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let encoded = String::deserialize(d)?;
        STANDARD.decode(&encoded).map_err(serde::de::Error::custom)
    }
}

/// A gateway-level media attachment frame forwarded to channel adapters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayMediaAttachment {
    pub request_id: String,
    pub session: GatewaySession,
    pub attachment: MediaAttachment,
}

pub type ChannelListenStream = mpsc::Receiver<Result<ChannelInboundEvent, ChannelError>>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewaySession {
    pub user_id: String,
    pub session_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GatewayTurnState {
    Running,
    Completed,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayTurnStatus {
    pub turn_id: String,
    pub state: GatewayTurnState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayClientHello {
    pub request_id: String,
    pub protocol_version: u16,
    pub user_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default)]
    pub create_new_session: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewaySendTurn {
    pub request_id: String,
    pub session_id: String,
    pub turn_id: String,
    pub prompt: String,
    /// Inline media attachments sent by the user (images, audio, etc.).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<crate::model::InlineMedia>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayCancelActiveTurn {
    pub request_id: String,
    pub session_id: String,
    pub turn_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayHealthCheck {
    pub request_id: String,
}

/// Request creation of a new session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayCreateSession {
    pub request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Agent name for the session. `None` or absent means "default".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
}

/// Request a listing of the user's sessions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayListSessions {
    pub request_id: String,
    #[serde(default)]
    pub include_archived: bool,
    /// When false (the default), subagent sessions (those with a
    /// `parent_session_id`) are excluded from the listing.
    #[serde(default)]
    pub include_subagent_sessions: bool,
}

/// Request switching the connection to a different existing session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewaySwitchSession {
    pub request_id: String,
    pub session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum GatewayClientFrame {
    Hello(GatewayClientHello),
    SendTurn(GatewaySendTurn),
    CancelActiveTurn(GatewayCancelActiveTurn),
    HealthCheck(GatewayHealthCheck),
    CreateSession(GatewayCreateSession),
    ListSessions(GatewayListSessions),
    SwitchSession(GatewaySwitchSession),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayHelloAck {
    pub request_id: String,
    pub protocol_version: u16,
    pub session: GatewaySession,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_turn: Option<GatewayTurnStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayTurnStarted {
    pub request_id: String,
    pub session: GatewaySession,
    pub turn: GatewayTurnStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayAssistantDelta {
    pub request_id: String,
    pub session: GatewaySession,
    pub turn: GatewayTurnStatus,
    pub delta: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GatewayTurnCompleted {
    pub request_id: String,
    pub session: GatewaySession,
    pub turn: GatewayTurnStatus,
    pub response: Response,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayTurnCancelled {
    pub request_id: String,
    pub session: GatewaySession,
    pub turn: GatewayTurnStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayErrorFrame {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<GatewaySession>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<GatewayTurnStatus>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayHealthStatus {
    pub request_id: String,
    pub healthy: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<GatewaySession>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_turn: Option<GatewayTurnStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_status: Option<StartupStatusReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// A runtime progress notification forwarded through the gateway to connected
/// channels during an active turn.
///
/// Channels decide independently how to render these (the TUI shows them in
/// the input bar title; future API clients can expose them as a status stream).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayTurnProgress {
    pub request_id: String,
    pub session: GatewaySession,
    pub turn: GatewayTurnStatus,
    pub progress: RuntimeProgressEvent,
}

/// Confirmation that a new session was created.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewaySessionCreated {
    pub request_id: String,
    pub session: GatewaySession,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub agent_name: String,
}

/// A summary of a session returned in a [`GatewaySessionList`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewaySessionSummary {
    pub session_id: String,
    pub agent_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub channel_origin: String,
    pub created_at: String,
    pub last_active_at: String,
    pub archived: bool,
}

/// Response containing the user's session listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewaySessionList {
    pub request_id: String,
    pub sessions: Vec<GatewaySessionSummary>,
}

/// Confirmation that the connection was switched to a different session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewaySessionSwitched {
    pub request_id: String,
    pub session: GatewaySession,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_turn: Option<GatewayTurnStatus>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum GatewayServerFrame {
    HelloAck(GatewayHelloAck),
    TurnStarted(GatewayTurnStarted),
    AssistantDelta(GatewayAssistantDelta),
    TurnCompleted(GatewayTurnCompleted),
    TurnCancelled(GatewayTurnCancelled),
    Error(GatewayErrorFrame),
    HealthStatus(GatewayHealthStatus),
    /// Runtime progress notification — emitted during tool execution and
    /// provider calls within a multi-step turn.
    TurnProgress(GatewayTurnProgress),
    /// Notification from a completed scheduled task.
    ScheduledNotification(GatewayScheduledNotification),
    /// Confirmation that a new session was created.
    SessionCreated(GatewaySessionCreated),
    /// Listing of the user's sessions.
    SessionList(GatewaySessionList),
    /// Confirmation that the connection was switched to a different session.
    SessionSwitched(GatewaySessionSwitched),
    /// A media attachment to be delivered through the channel (photo, audio,
    /// document, etc.). Emitted when the agent uses the `send_media` tool.
    MediaAttachment(GatewayMediaAttachment),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayScheduledNotification {
    pub schedule_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule_name: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelInboundEvent {
    pub channel_id: String,
    pub connection_id: String,
    pub frame: GatewayClientFrame,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelOutboundEvent {
    pub channel_id: String,
    pub connection_id: String,
    pub frame: GatewayServerFrame,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelHealthStatus {
    pub healthy: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[async_trait]
pub trait Channel: Send + Sync {
    async fn send(&self, event: ChannelOutboundEvent) -> Result<(), ChannelError>;

    async fn listen(&self, buffer_size: usize) -> Result<ChannelListenStream, ChannelError>;

    async fn health_check(&self) -> Result<ChannelHealthStatus, ChannelError>;
}
