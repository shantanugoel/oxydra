CREATE INDEX IF NOT EXISTS idx_chunks_session_content_hash
ON chunks (session_id, content_hash);
