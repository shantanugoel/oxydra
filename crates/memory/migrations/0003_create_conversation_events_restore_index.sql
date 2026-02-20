CREATE INDEX IF NOT EXISTS idx_conversation_events_session_sequence
ON conversation_events (session_id, sequence);
