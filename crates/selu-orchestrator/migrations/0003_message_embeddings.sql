-- Message embeddings table for semantic memory retrieval.
-- Embeddings are stored as JSON arrays of floats (portable, no native extension).
-- The embedding dimension depends on the provider (e.g. 1536 for OpenAI, 1024 for Anthropic).
CREATE TABLE IF NOT EXISTS message_embeddings (
    id              TEXT PRIMARY KEY,
    message_id      TEXT NOT NULL REFERENCES messages(id),
    session_id      TEXT NOT NULL,
    pipe_id         TEXT NOT NULL,
    content_hash    TEXT NOT NULL, -- SHA256 of the content, for dedup
    embedding       BLOB NOT NULL, -- f32 little-endian bytes
    dimension       INTEGER NOT NULL,
    created_at      TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_embeddings_session ON message_embeddings(session_id);
CREATE INDEX IF NOT EXISTS idx_embeddings_pipe ON message_embeddings(pipe_id);
CREATE INDEX IF NOT EXISTS idx_embeddings_hash ON message_embeddings(content_hash);
