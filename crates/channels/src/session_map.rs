use std::sync::Arc;

use types::{MemoryError, SessionStore};

/// Thin wrapper around [`SessionStore`] for channel-adapter session resolution.
///
/// Given a `(channel_id, channel_context_id)` pair (e.g. `("telegram", "12345:42")`),
/// the map resolves or updates the corresponding gateway session ID. The
/// underlying persistence is handled by the injected `SessionStore`.
pub struct ChannelSessionMap {
    store: Arc<dyn SessionStore>,
}

impl ChannelSessionMap {
    /// Create a new channel session map backed by the given store.
    pub fn new(store: Arc<dyn SessionStore>) -> Self {
        Self { store }
    }

    /// Look up the gateway session mapped to a channel context.
    ///
    /// Returns `None` if no mapping exists yet (first message from this context).
    pub async fn get_session_id(
        &self,
        channel_id: &str,
        channel_context_id: &str,
    ) -> Result<Option<String>, MemoryError> {
        self.store
            .get_channel_session(channel_id, channel_context_id)
            .await
    }

    /// Set (or update) the mapping from a channel context to a gateway session.
    ///
    /// Called when:
    /// - A new session is created for a first-time channel context.
    /// - The user issues `/new` to start a fresh session in the same context.
    /// - The user issues `/switch` to point the context at a different session.
    pub async fn set_session_id(
        &self,
        channel_id: &str,
        channel_context_id: &str,
        session_id: &str,
    ) -> Result<(), MemoryError> {
        self.store
            .set_channel_session(channel_id, channel_context_id, session_id)
            .await
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use types::{MemoryError, SessionRecord, SessionStore};

    use super::*;

    /// Minimal in-memory session store for unit testing the map wrapper.
    struct InMemorySessionStore {
        mappings: tokio::sync::Mutex<std::collections::HashMap<(String, String), String>>,
    }

    impl InMemorySessionStore {
        fn new() -> Self {
            Self {
                mappings: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            }
        }
    }

    #[async_trait]
    impl SessionStore for InMemorySessionStore {
        async fn create_session(&self, _record: &SessionRecord) -> Result<(), MemoryError> {
            Ok(())
        }
        async fn get_session(
            &self,
            _session_id: &str,
        ) -> Result<Option<SessionRecord>, MemoryError> {
            Ok(None)
        }
        async fn list_sessions(
            &self,
            _user_id: &str,
            _include_archived: bool,
        ) -> Result<Vec<SessionRecord>, MemoryError> {
            Ok(Vec::new())
        }
        async fn touch_session(&self, _session_id: &str) -> Result<(), MemoryError> {
            Ok(())
        }
        async fn update_display_name(
            &self,
            _session_id: &str,
            _display_name: &str,
        ) -> Result<(), MemoryError> {
            Ok(())
        }
        async fn archive_session(&self, _session_id: &str) -> Result<(), MemoryError> {
            Ok(())
        }
        async fn get_channel_session(
            &self,
            channel_id: &str,
            channel_context_id: &str,
        ) -> Result<Option<String>, MemoryError> {
            let mappings = self.mappings.lock().await;
            Ok(mappings
                .get(&(channel_id.to_owned(), channel_context_id.to_owned()))
                .cloned())
        }
        async fn set_channel_session(
            &self,
            channel_id: &str,
            channel_context_id: &str,
            session_id: &str,
        ) -> Result<(), MemoryError> {
            let mut mappings = self.mappings.lock().await;
            mappings.insert(
                (channel_id.to_owned(), channel_context_id.to_owned()),
                session_id.to_owned(),
            );
            Ok(())
        }
    }

    #[tokio::test]
    async fn get_returns_none_for_unknown_context() {
        let store = Arc::new(InMemorySessionStore::new());
        let map = ChannelSessionMap::new(store);
        let result = map.get_session_id("telegram", "12345").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn set_then_get_returns_session_id() {
        let store = Arc::new(InMemorySessionStore::new());
        let map = ChannelSessionMap::new(store);
        map.set_session_id("telegram", "12345", "ses-abc")
            .await
            .unwrap();
        let result = map.get_session_id("telegram", "12345").await.unwrap();
        assert_eq!(result.as_deref(), Some("ses-abc"));
    }

    #[tokio::test]
    async fn set_overwrites_previous_mapping() {
        let store = Arc::new(InMemorySessionStore::new());
        let map = ChannelSessionMap::new(store);
        map.set_session_id("telegram", "12345", "ses-old")
            .await
            .unwrap();
        map.set_session_id("telegram", "12345", "ses-new")
            .await
            .unwrap();
        let result = map.get_session_id("telegram", "12345").await.unwrap();
        assert_eq!(result.as_deref(), Some("ses-new"));
    }

    #[tokio::test]
    async fn different_channels_are_independent() {
        let store = Arc::new(InMemorySessionStore::new());
        let map = ChannelSessionMap::new(store);
        map.set_session_id("telegram", "ctx-1", "ses-tg")
            .await
            .unwrap();
        map.set_session_id("discord", "ctx-1", "ses-dc")
            .await
            .unwrap();
        assert_eq!(
            map.get_session_id("telegram", "ctx-1")
                .await
                .unwrap()
                .as_deref(),
            Some("ses-tg")
        );
        assert_eq!(
            map.get_session_id("discord", "ctx-1")
                .await
                .unwrap()
                .as_deref(),
            Some("ses-dc")
        );
    }
}
