-- Per-agent, per-user long-term memory (separate from user personality).
-- Indexed with SQLite FTS5 for BM25 retrieval.
CREATE TABLE IF NOT EXISTS agent_memories (
    id          TEXT PRIMARY KEY,
    agent_id    TEXT NOT NULL,
    user_id     TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    memory_text TEXT NOT NULL,
    tags        TEXT NOT NULL DEFAULT '',
    source      TEXT NOT NULL DEFAULT 'agent' CHECK (source IN ('agent', 'manual')),
    created_at  TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_agent_memories_agent_user
    ON agent_memories(agent_id, user_id, updated_at DESC);

-- External-content FTS table so we can query BM25 while keeping canonical rows
-- in `agent_memories`.
CREATE VIRTUAL TABLE IF NOT EXISTS agent_memories_fts USING fts5(
    memory_text,
    tags,
    content='agent_memories',
    content_rowid='rowid',
    tokenize='unicode61'
);

CREATE TRIGGER IF NOT EXISTS agent_memories_ai AFTER INSERT ON agent_memories BEGIN
    INSERT INTO agent_memories_fts(rowid, memory_text, tags)
    VALUES (new.rowid, new.memory_text, new.tags);
END;

CREATE TRIGGER IF NOT EXISTS agent_memories_ad AFTER DELETE ON agent_memories BEGIN
    INSERT INTO agent_memories_fts(agent_memories_fts, rowid, memory_text, tags)
    VALUES ('delete', old.rowid, old.memory_text, old.tags);
END;

CREATE TRIGGER IF NOT EXISTS agent_memories_au AFTER UPDATE ON agent_memories BEGIN
    INSERT INTO agent_memories_fts(agent_memories_fts, rowid, memory_text, tags)
    VALUES ('delete', old.rowid, old.memory_text, old.tags);
    INSERT INTO agent_memories_fts(rowid, memory_text, tags)
    VALUES (new.rowid, new.memory_text, new.tags);
END;
