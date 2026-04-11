-- Per-thread per-agent session bindings.
--
-- Threads keep a legacy/default session_id for compatibility, but delegated
-- specialists need their own stable session per thread to avoid container
-- churn. This table maps (thread, agent) -> session.
CREATE TABLE IF NOT EXISTS thread_agent_sessions (
    id          TEXT PRIMARY KEY,
    thread_id   TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    agent_id    TEXT NOT NULL,
    session_id  TEXT NOT NULL REFERENCES sessions(id),
    created_at  TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at  TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(thread_id, agent_id)
);

CREATE INDEX IF NOT EXISTS idx_thread_agent_sessions_session
    ON thread_agent_sessions(session_id);
