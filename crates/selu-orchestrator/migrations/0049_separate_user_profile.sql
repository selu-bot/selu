-- Separate user profile from agent memory.
--
-- Migration 0048 merged personality facts into agent_memories, gating them
-- behind BM25 keyword search. This broke the core contract: the agent must
-- ALWAYS know who the user is, regardless of what the user just typed.
--
-- This migration restores personality as a first-class system:
--   1. Create `user_profile` table (always-injected, no search gate).
--   2. Move personality facts (source='auto'|'manual' with a category) back out.
--   3. Clean up agent_memories and rebuild FTS index.

-- ── 1. Create user_profile table ─────────────────────────────────────────────
CREATE TABLE user_profile (
    id          TEXT PRIMARY KEY,
    user_id     TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    fact_text   TEXT NOT NULL,
    category    TEXT NOT NULL DEFAULT 'other',
    source      TEXT NOT NULL DEFAULT 'manual',   -- 'auto' | 'manual' | 'agent'
    agent_id    TEXT NOT NULL DEFAULT 'system',    -- who contributed this fact
    created_at  TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX idx_user_profile_user ON user_profile(user_id, updated_at DESC);

-- ── 2. Migrate personality facts from agent_memories ─────────────────────────
INSERT INTO user_profile (id, user_id, fact_text, category, source, agent_id, created_at, updated_at)
SELECT id, user_id, memory_text, category, source, agent_id, created_at, updated_at
FROM agent_memories
WHERE source IN ('auto', 'manual')
  AND category != '';

-- ── 3. Remove migrated rows from agent_memories ─────────────────────────────
DELETE FROM agent_memories
WHERE source IN ('auto', 'manual')
  AND category != '';

-- ── 4. Rebuild FTS index after row removal ───────────────────────────────────
INSERT INTO agent_memories_fts(agent_memories_fts) VALUES('rebuild');
