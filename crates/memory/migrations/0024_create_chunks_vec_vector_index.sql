CREATE INDEX IF NOT EXISTS idx_chunks_vec_embedding_ann
ON chunks_vec(libsql_vector_idx(embedding_blob));
