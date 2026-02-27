-- Drop the message_embeddings system (replaced by user_personality)
DROP TABLE IF EXISTS message_embeddings;

-- User personality: persistent facts about users, extracted from conversations
-- or added manually. Shared across all agents (global per-user).
CREATE TABLE IF NOT EXISTS user_personality (
    id          TEXT PRIMARY KEY,
    user_id     TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    category    TEXT NOT NULL CHECK (category IN ('personal', 'preferences', 'location', 'work', 'other')),
    fact        TEXT NOT NULL,
    source      TEXT NOT NULL DEFAULT 'auto' CHECK (source IN ('auto', 'manual')),
    created_at  TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX idx_personality_user ON user_personality(user_id);
CREATE INDEX idx_personality_user_category ON user_personality(user_id, category);
