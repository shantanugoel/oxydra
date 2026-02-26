use crate::GatewayServerFrame;

/// Trait for sending proactive (non-request-driven) messages to a specific
/// channel context, e.g. scheduled task notifications to a Telegram chat.
pub trait ProactiveSender: Send + Sync {
    /// Send a server frame to the specified channel context.
    fn send_proactive(&self, channel_context_id: &str, frame: &GatewayServerFrame);
}
