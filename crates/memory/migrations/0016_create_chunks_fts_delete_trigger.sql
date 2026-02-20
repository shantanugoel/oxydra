CREATE TRIGGER IF NOT EXISTS trg_chunks_fts_ad
AFTER DELETE ON chunks
BEGIN
    DELETE FROM chunks_fts WHERE rowid = old.rowid;
END;
