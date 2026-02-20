CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts
USING fts5(
    chunk_id UNINDEXED,
    session_id UNINDEXED,
    chunk_text,
    tokenize = 'unicode61'
);
