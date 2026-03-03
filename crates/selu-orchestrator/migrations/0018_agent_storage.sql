-- Agent storage: persistent key-value store scoped per agent + user.
--
-- Allows agents to persist state across sessions and container restarts.
-- Common use cases: last sync timestamps, user preferences per agent,
-- checkpoint data for polling workflows.
CREATE TABLE agent_storage (
    id         TEXT PRIMARY KEY NOT NULL,
    agent_id   TEXT NOT NULL,
    user_id    TEXT NOT NULL,
    key        TEXT NOT NULL,
    value      TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(agent_id, user_id, key)
);
