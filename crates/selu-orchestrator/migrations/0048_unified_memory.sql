-- Unified memory: merge user_personality into agent_memories.
--
-- Changes:
--   1. Recreate agent_memories without the restrictive CHECK on `source`
--      (was: 'agent','manual' — needs 'auto' for personality extraction).
--   2. Add `category` column for personality-style categories.
--   3. Add user-scoped index (reads no longer filter by agent_id).
--   4. Migrate existing user_personality rows into agent_memories.
--   5. Rebuild FTS index.
--
-- The `agent_id` column is kept but its semantic role changes from "owner"
-- to "source" — it records which agent wrote the memory, but reads are
-- user-scoped (all agents see all memories for a given user).
--
-- user_personality is NOT dropped here — cleanup happens in a follow-up
-- migration after the code change is verified in production.

-- ── 1. Recreate agent_memories with relaxed constraints ─────────────────────
CREATE TABLE agent_memories_new (
    id          TEXT PRIMARY KEY,
    agent_id    TEXT NOT NULL,
    user_id     TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    memory_text TEXT NOT NULL,
    tags        TEXT NOT NULL DEFAULT '',
    source      TEXT NOT NULL DEFAULT 'agent',
    category    TEXT NOT NULL DEFAULT '',
    created_at  TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

INSERT INTO agent_memories_new (id, agent_id, user_id, memory_text, tags, source, category, created_at, updated_at)
SELECT id, agent_id, user_id, memory_text, tags, source, '', created_at, updated_at
FROM agent_memories;

-- Drop old table (also removes old triggers and indexes)
DROP TABLE agent_memories;
ALTER TABLE agent_memories_new RENAME TO agent_memories;

-- ── 2. Recreate indexes ─────────────────────────────────────────────────────
CREATE INDEX idx_agent_memories_agent_user
    ON agent_memories(agent_id, user_id, updated_at DESC);

-- New: user-scoped index for unified reads
CREATE INDEX idx_agent_memories_user
    ON agent_memories(user_id, updated_at DESC);

-- ── 3. Recreate FTS5 external-content table + sync triggers ─────────────────
DROP TABLE IF EXISTS agent_memories_fts;

CREATE VIRTUAL TABLE agent_memories_fts USING fts5(
    memory_text,
    tags,
    content='agent_memories',
    content_rowid='rowid',
    tokenize='unicode61'
);

CREATE TRIGGER agent_memories_ai AFTER INSERT ON agent_memories BEGIN
    INSERT INTO agent_memories_fts(rowid, memory_text, tags)
    VALUES (new.rowid, new.memory_text, new.tags);
END;

CREATE TRIGGER agent_memories_ad AFTER DELETE ON agent_memories BEGIN
    INSERT INTO agent_memories_fts(agent_memories_fts, rowid, memory_text, tags)
    VALUES ('delete', old.rowid, old.memory_text, old.tags);
END;

CREATE TRIGGER agent_memories_au AFTER UPDATE ON agent_memories BEGIN
    INSERT INTO agent_memories_fts(agent_memories_fts, rowid, memory_text, tags)
    VALUES ('delete', old.rowid, old.memory_text, old.tags);
    INSERT INTO agent_memories_fts(rowid, memory_text, tags)
    VALUES (new.rowid, new.memory_text, new.tags);
END;

-- ── 4. Migrate personality facts into memory ────────────────────────────────
INSERT INTO agent_memories (id, agent_id, user_id, memory_text, tags, source, category, created_at, updated_at)
SELECT
    id,
    COALESCE(agent_id, 'system'),
    user_id,
    fact,
    category,       -- category goes into tags for BM25 searchability
    'auto',         -- personality extraction source
    category,       -- preserve category for UI grouping
    created_at,
    updated_at
FROM user_personality;

-- ── 5. Rebuild FTS to include all rows (existing + migrated) ────────────────
INSERT INTO agent_memories_fts(agent_memories_fts) VALUES('rebuild');
