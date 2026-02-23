use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::model::RuntimeProgressEvent;
use crate::StartupStatusReport;
use crate::{ChannelError, Response};

pub const GATEWAY_PROTOCOL_VERSION: u16 = 1;

pub type ChannelListenStream = mpsc::Receiver<Result<ChannelInboundEvent, ChannelError>>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewaySession {
    pub user_id: String,
    pub runtime_session_id: String,
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
    pub runtime_session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewaySendTurn {
    pub request_id: String,
    pub runtime_session_id: String,
    pub turn_id: String,
    pub prompt: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayCancelActiveTurn {
    pub request_id: String,
    pub runtime_session_id: String,
    pub turn_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayHealthCheck {
    pub request_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum GatewayClientFrame {
    Hello(GatewayClientHello),
    SendTurn(GatewaySendTurn),
    CancelActiveTurn(GatewayCancelActiveTurn),
    HealthCheck(GatewayHealthCheck),
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
