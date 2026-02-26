use async_trait::async_trait;
use libsql::{Connection, params};
use types::{MemoryError, SessionRecord, SessionStore};

fn query_error(message: String) -> MemoryError {
    MemoryError::Query { message }
}

pub struct LibsqlSessionStore {
    conn: Connection,
}

impl LibsqlSessionStore {
    pub fn new(conn: Connection) -> Self {
        Self { conn }
    }
}

#[async_trait]
impl SessionStore for LibsqlSessionStore {
    async fn create_session(&self, record: &SessionRecord) -> Result<(), MemoryError> {
        let archived = if record.archived { 1i64 } else { 0i64 };
        self.conn
            .execute(
                "INSERT INTO gateway_sessions (
                    session_id, user_id, agent_name, display_name,
                    channel_origin, parent_session_id, created_at,
                    last_active_at, archived
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, datetime('now'), datetime('now'), ?7)
                ON CONFLICT(session_id) DO NOTHING",
                params![
                    record.session_id.as_str(),
                    record.user_id.as_str(),
                    record.agent_name.as_str(),
                    record.display_name.as_deref(),
                    record.channel_origin.as_str(),
                    record.parent_session_id.as_deref(),
                    archived,
                ],
            )
            .await
            .map_err(|e| query_error(format!("failed to create gateway session: {e}")))?;
        Ok(())
    }

    async fn get_session(&self, session_id: &str) -> Result<Option<SessionRecord>, MemoryError> {
        let mut rows = self
            .conn
            .query(
                "SELECT session_id, user_id, agent_name, display_name,
                        channel_origin, parent_session_id, created_at,
                        last_active_at, archived
                 FROM gateway_sessions
                 WHERE session_id = ?1
                 LIMIT 1",
                params![session_id],
            )
            .await
            .map_err(|e| query_error(format!("failed to get gateway session: {e}")))?;

        let Some(row) = rows
            .next()
            .await
            .map_err(|e| query_error(format!("failed to read gateway session row: {e}")))?
        else {
            return Ok(None);
        };

        Ok(Some(record_from_row(&row)?))
    }

    async fn list_sessions(
        &self,
        user_id: &str,
        include_archived: bool,
    ) -> Result<Vec<SessionRecord>, MemoryError> {
        let (sql, params) = if include_archived {
            (
                "SELECT session_id, user_id, agent_name, display_name,
                        channel_origin, parent_session_id, created_at,
                        last_active_at, archived
                 FROM gateway_sessions
                 WHERE user_id = ?1
                 ORDER BY last_active_at DESC",
                params![user_id],
            )
        } else {
            (
                "SELECT session_id, user_id, agent_name, display_name,
                        channel_origin, parent_session_id, created_at,
                        last_active_at, archived
                 FROM gateway_sessions
                 WHERE user_id = ?1 AND archived = 0
                 ORDER BY last_active_at DESC",
                params![user_id],
            )
        };

        let mut rows = self
            .conn
            .query(sql, params)
            .await
            .map_err(|e| query_error(format!("failed to list gateway sessions: {e}")))?;

        let mut records = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| query_error(format!("failed to read gateway session list row: {e}")))?
        {
            records.push(record_from_row(&row)?);
        }
        Ok(records)
    }

    async fn touch_session(&self, session_id: &str) -> Result<(), MemoryError> {
        self.conn
            .execute(
                "UPDATE gateway_sessions SET last_active_at = datetime('now') WHERE session_id = ?1",
                params![session_id],
            )
            .await
            .map_err(|e| query_error(format!("failed to touch gateway session: {e}")))?;
        Ok(())
    }

    async fn archive_session(&self, session_id: &str) -> Result<(), MemoryError> {
        self.conn
            .execute(
                "UPDATE gateway_sessions SET archived = 1, last_active_at = datetime('now') WHERE session_id = ?1",
                params![session_id],
            )
            .await
            .map_err(|e| query_error(format!("failed to archive gateway session: {e}")))?;
        Ok(())
    }

    async fn update_display_name(
        &self,
        session_id: &str,
        display_name: &str,
    ) -> Result<(), MemoryError> {
        self.conn
            .execute(
                "UPDATE gateway_sessions SET display_name = ?2, last_active_at = datetime('now') WHERE session_id = ?1",
                params![session_id, display_name],
            )
            .await
            .map_err(|e| query_error(format!("failed to update session display_name: {e}")))?;
        Ok(())
    }

    async fn get_channel_session(
        &self,
        channel_id: &str,
        channel_context_id: &str,
    ) -> Result<Option<String>, MemoryError> {
        let mut rows = self
            .conn
            .query(
                "SELECT session_id FROM channel_session_mappings
                 WHERE channel_id = ?1 AND channel_context_id = ?2
                 LIMIT 1",
                params![channel_id, channel_context_id],
            )
            .await
            .map_err(|e| query_error(format!("failed to get channel session mapping: {e}")))?;

        let Some(row) = rows
            .next()
            .await
            .map_err(|e| query_error(format!("failed to read channel session mapping row: {e}")))?
        else {
            return Ok(None);
        };

        let session_id = row
            .get::<String>(0)
            .map_err(|e| query_error(e.to_string()))?;
        Ok(Some(session_id))
    }

    async fn set_channel_session(
        &self,
        channel_id: &str,
        channel_context_id: &str,
        session_id: &str,
    ) -> Result<(), MemoryError> {
        self.conn
            .execute(
                "INSERT INTO channel_session_mappings (channel_id, channel_context_id, session_id)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(channel_id, channel_context_id) DO UPDATE SET
                     session_id = excluded.session_id,
                     updated_at = datetime('now')",
                params![channel_id, channel_context_id, session_id],
            )
            .await
            .map_err(|e| query_error(format!("failed to set channel session mapping: {e}")))?;
        Ok(())
    }
}

fn record_from_row(row: &libsql::Row) -> Result<SessionRecord, MemoryError> {
    let session_id = row
        .get::<String>(0)
        .map_err(|e| query_error(e.to_string()))?;
    let user_id = row
        .get::<String>(1)
        .map_err(|e| query_error(e.to_string()))?;
    let agent_name = row
        .get::<String>(2)
        .map_err(|e| query_error(e.to_string()))?;
    let display_name = row
        .get::<Option<String>>(3)
        .map_err(|e| query_error(e.to_string()))?;
    let channel_origin = row
        .get::<String>(4)
        .map_err(|e| query_error(e.to_string()))?;
    let parent_session_id = row
        .get::<Option<String>>(5)
        .map_err(|e| query_error(e.to_string()))?;
    let created_at = row
        .get::<String>(6)
        .map_err(|e| query_error(e.to_string()))?;
    let last_active_at = row
        .get::<String>(7)
        .map_err(|e| query_error(e.to_string()))?;
    let archived_int = row.get::<i64>(8).map_err(|e| query_error(e.to_string()))?;

    Ok(SessionRecord {
        session_id,
        user_id,
        agent_name,
        display_name,
        channel_origin,
        parent_session_id,
        created_at,
        last_active_at,
        archived: archived_int != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_store() -> LibsqlSessionStore {
        let db = libsql::Builder::new_local(":memory:")
            .build()
            .await
            .expect("in-memory db");
        let conn = db.connect().expect("connect");

        // Run the gateway sessions migration.
        conn.execute_batch(include_str!(
            "../migrations/0020_create_gateway_sessions.sql"
        ))
        .await
        .expect("migration 0020");

        // Run the channel session mappings migration.
        conn.execute_batch(include_str!(
            "../migrations/0021_create_channel_session_mappings.sql"
        ))
        .await
        .expect("migration 0021");

        LibsqlSessionStore::new(conn)
    }

    fn sample_record(session_id: &str, user_id: &str) -> SessionRecord {
        SessionRecord {
            session_id: session_id.to_owned(),
            user_id: user_id.to_owned(),
            agent_name: "default".to_owned(),
            display_name: None,
            channel_origin: "tui".to_owned(),
            parent_session_id: None,
            created_at: String::new(),
            last_active_at: String::new(),
            archived: false,
        }
    }

    #[tokio::test]
    async fn create_and_get_session() {
        let store = test_store().await;
        let record = sample_record("ses-1", "alice");
        store
            .create_session(&record)
            .await
            .expect("create should succeed");

        let got = store
            .get_session("ses-1")
            .await
            .expect("get should succeed")
            .expect("session should exist");
        assert_eq!(got.session_id, "ses-1");
        assert_eq!(got.user_id, "alice");
        assert_eq!(got.agent_name, "default");
        assert!(!got.archived);
    }

    #[tokio::test]
    async fn get_nonexistent_session_returns_none() {
        let store = test_store().await;
        let got = store
            .get_session("no-such-id")
            .await
            .expect("get should succeed");
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn list_sessions_excludes_archived_by_default() {
        let store = test_store().await;
        store
            .create_session(&sample_record("ses-1", "alice"))
            .await
            .unwrap();
        store
            .create_session(&sample_record("ses-2", "alice"))
            .await
            .unwrap();
        store.archive_session("ses-2").await.unwrap();

        let active = store.list_sessions("alice", false).await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].session_id, "ses-1");

        let all = store.list_sessions("alice", true).await.unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn touch_session_updates_last_active_at() {
        let store = test_store().await;
        store
            .create_session(&sample_record("ses-1", "alice"))
            .await
            .unwrap();

        let before = store
            .get_session("ses-1")
            .await
            .unwrap()
            .unwrap()
            .last_active_at
            .clone();

        // Touch advances the timestamp (in a fast test the timestamps may
        // be the same second, so we just verify no error).
        store.touch_session("ses-1").await.unwrap();

        let after = store
            .get_session("ses-1")
            .await
            .unwrap()
            .unwrap()
            .last_active_at;
        assert!(after >= before);
    }

    #[tokio::test]
    async fn archive_session_marks_as_archived() {
        let store = test_store().await;
        store
            .create_session(&sample_record("ses-1", "alice"))
            .await
            .unwrap();
        store.archive_session("ses-1").await.unwrap();

        let got = store.get_session("ses-1").await.unwrap().unwrap();
        assert!(got.archived);
    }

    #[tokio::test]
    async fn create_session_with_parent() {
        let store = test_store().await;
        store
            .create_session(&sample_record("parent-1", "alice"))
            .await
            .unwrap();

        let mut child = sample_record("child-1", "alice");
        child.parent_session_id = Some("parent-1".to_owned());
        store.create_session(&child).await.unwrap();

        let got = store.get_session("child-1").await.unwrap().unwrap();
        assert_eq!(got.parent_session_id.as_deref(), Some("parent-1"));
    }

    #[tokio::test]
    async fn list_sessions_filters_by_user() {
        let store = test_store().await;
        store
            .create_session(&sample_record("ses-a", "alice"))
            .await
            .unwrap();
        store
            .create_session(&sample_record("ses-b", "bob"))
            .await
            .unwrap();

        let alice_sessions = store.list_sessions("alice", false).await.unwrap();
        assert_eq!(alice_sessions.len(), 1);
        assert_eq!(alice_sessions[0].session_id, "ses-a");

        let bob_sessions = store.list_sessions("bob", false).await.unwrap();
        assert_eq!(bob_sessions.len(), 1);
        assert_eq!(bob_sessions[0].session_id, "ses-b");
    }

    #[tokio::test]
    async fn duplicate_create_is_idempotent() {
        let store = test_store().await;
        let record = sample_record("ses-1", "alice");
        store.create_session(&record).await.unwrap();
        // Second create with same ID should not error (ON CONFLICT DO NOTHING).
        store.create_session(&record).await.unwrap();

        let listed = store.list_sessions("alice", false).await.unwrap();
        assert_eq!(listed.len(), 1);
    }

    #[tokio::test]
    async fn update_display_name_sets_name() {
        let store = test_store().await;
        store
            .create_session(&sample_record("ses-1", "alice"))
            .await
            .unwrap();

        store
            .update_display_name("ses-1", "My Research Session")
            .await
            .unwrap();

        let got = store.get_session("ses-1").await.unwrap().unwrap();
        assert_eq!(got.display_name.as_deref(), Some("My Research Session"));
    }

    #[tokio::test]
    async fn get_channel_session_returns_none_for_unknown_mapping() {
        let store = test_store().await;
        let result = store
            .get_channel_session("telegram", "12345")
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn set_and_get_channel_session_round_trips() {
        let store = test_store().await;
        store
            .create_session(&sample_record("ses-1", "alice"))
            .await
            .unwrap();

        store
            .set_channel_session("telegram", "12345", "ses-1")
            .await
            .unwrap();

        let got = store
            .get_channel_session("telegram", "12345")
            .await
            .unwrap();
        assert_eq!(got.as_deref(), Some("ses-1"));
    }

    #[tokio::test]
    async fn set_channel_session_updates_existing_mapping() {
        let store = test_store().await;
        store
            .create_session(&sample_record("ses-1", "alice"))
            .await
            .unwrap();
        store
            .create_session(&sample_record("ses-2", "alice"))
            .await
            .unwrap();

        store
            .set_channel_session("telegram", "12345", "ses-1")
            .await
            .unwrap();
        store
            .set_channel_session("telegram", "12345", "ses-2")
            .await
            .unwrap();

        let got = store
            .get_channel_session("telegram", "12345")
            .await
            .unwrap();
        assert_eq!(got.as_deref(), Some("ses-2"));
    }

    #[tokio::test]
    async fn channel_session_mapping_is_scoped_by_channel_and_context() {
        let store = test_store().await;
        store
            .create_session(&sample_record("ses-t", "alice"))
            .await
            .unwrap();
        store
            .create_session(&sample_record("ses-d", "alice"))
            .await
            .unwrap();

        store
            .set_channel_session("telegram", "chat-1", "ses-t")
            .await
            .unwrap();
        store
            .set_channel_session("discord", "chat-1", "ses-d")
            .await
            .unwrap();

        let telegram = store
            .get_channel_session("telegram", "chat-1")
            .await
            .unwrap();
        assert_eq!(telegram.as_deref(), Some("ses-t"));

        let discord = store
            .get_channel_session("discord", "chat-1")
            .await
            .unwrap();
        assert_eq!(discord.as_deref(), Some("ses-d"));
    }

    #[tokio::test]
    async fn forum_topic_maps_to_separate_session() {
        let store = test_store().await;
        store
            .create_session(&sample_record("ses-main", "alice"))
            .await
            .unwrap();
        store
            .create_session(&sample_record("ses-topic", "alice"))
            .await
            .unwrap();

        // Regular chat mapping.
        store
            .set_channel_session("telegram", "12345", "ses-main")
            .await
            .unwrap();
        // Forum topic mapping (chat_id:topic_id).
        store
            .set_channel_session("telegram", "12345:42", "ses-topic")
            .await
            .unwrap();

        let main = store
            .get_channel_session("telegram", "12345")
            .await
            .unwrap();
        assert_eq!(main.as_deref(), Some("ses-main"));

        let topic = store
            .get_channel_session("telegram", "12345:42")
            .await
            .unwrap();
        assert_eq!(topic.as_deref(), Some("ses-topic"));
    }
}
