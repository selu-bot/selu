-- Per-user agent visibility.
-- If a user has NO rows in this table, they can access ALL agents (opt-out model).
-- Once any row exists for a user, only agents listed here are accessible.
-- The "default" agent is always accessible regardless of this table.
CREATE TABLE IF NOT EXISTS user_agent_access (
    user_id     TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    agent_id    TEXT NOT NULL,
    PRIMARY KEY (user_id, agent_id)
);
