use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::MemoryError;

/// Metadata record for a gateway session, persisted across restarts.
///
/// Active session state (broadcast channels, turn locks) lives in memory;
/// this struct captures the durable metadata that survives gateway restarts
/// and supports session listing, resumption, and archival.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub session_id: String,
    pub user_id: String,
    pub agent_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub channel_origin: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    pub created_at: String,
    pub last_active_at: String,
    pub archived: bool,
}

/// Durable session persistence backend.
///
/// Defined in `types` (boundary-safe). Implemented in `memory` crate
/// (`LibsqlSessionStore`) and injected into the gateway at bootstrap.
///
/// In-memory session state is authoritative for active sessions; the store
/// is for listing, resumption after restart, and archival.
#[async_trait]
pub trait SessionStore: Send + Sync {
    /// Persist a new session record.
    async fn create_session(&self, record: &SessionRecord) -> Result<(), MemoryError>;

    /// Look up a session by its ID.
    async fn get_session(&self, session_id: &str) -> Result<Option<SessionRecord>, MemoryError>;

    /// List sessions for a user, optionally including archived ones.
    async fn list_sessions(
        &self,
        user_id: &str,
        include_archived: bool,
    ) -> Result<Vec<SessionRecord>, MemoryError>;

    /// Update the `last_active_at` timestamp for a session.
    async fn touch_session(&self, session_id: &str) -> Result<(), MemoryError>;

    /// Mark a session as archived (soft-delete).
    async fn archive_session(&self, session_id: &str) -> Result<(), MemoryError>;
}
