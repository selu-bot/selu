CREATE TABLE IF NOT EXISTS thread_artifacts (
    id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    filename TEXT NOT NULL,
    mime_type TEXT NOT NULL,
    size_bytes INTEGER NOT NULL,
    file_path TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_thread_artifacts_thread
    ON thread_artifacts(thread_id);

CREATE INDEX IF NOT EXISTS idx_thread_artifacts_user
    ON thread_artifacts(user_id);
