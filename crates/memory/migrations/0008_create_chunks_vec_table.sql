CREATE TABLE IF NOT EXISTS chunks_vec (
    chunk_id TEXT PRIMARY KEY,
    embedding_blob FLOAT32(512) NOT NULL,
    embedding_model TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (chunk_id) REFERENCES chunks(chunk_id) ON DELETE CASCADE
);
