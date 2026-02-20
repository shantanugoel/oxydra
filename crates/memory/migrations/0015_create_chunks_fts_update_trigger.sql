CREATE TRIGGER IF NOT EXISTS trg_chunks_fts_au
AFTER UPDATE ON chunks
BEGIN
    DELETE FROM chunks_fts WHERE rowid = old.rowid;
    INSERT INTO chunks_fts(rowid, chunk_id, session_id, chunk_text)
    VALUES (new.rowid, new.chunk_id, new.session_id, new.chunk_text);
END;
