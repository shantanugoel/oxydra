CREATE INDEX IF NOT EXISTS idx_files_session_source_uri
ON files (session_id, source_uri);
