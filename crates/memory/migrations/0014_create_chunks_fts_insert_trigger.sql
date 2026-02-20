CREATE TRIGGER IF NOT EXISTS trg_chunks_fts_ai
AFTER INSERT ON chunks
BEGIN
    INSERT INTO chunks_fts(rowid, chunk_id, session_id, chunk_text)
    VALUES (new.rowid, new.chunk_id, new.session_id, new.chunk_text);
END;
