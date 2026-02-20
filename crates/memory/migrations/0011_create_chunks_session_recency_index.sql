CREATE INDEX IF NOT EXISTS idx_chunks_session_created_at
ON chunks (session_id, created_at DESC);
