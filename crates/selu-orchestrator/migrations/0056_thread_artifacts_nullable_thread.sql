-- Allow artifacts to be persisted without a thread context so that
-- event-fanout and other non-threaded turns don't lose attachments on restart.
--
-- SQLite doesn't support ALTER COLUMN, so we recreate the table.

CREATE TABLE IF NOT EXISTS thread_artifacts_new (
    id TEXT PRIMARY KEY,
    thread_id TEXT,
    user_id TEXT NOT NULL,
    filename TEXT NOT NULL,
    mime_type TEXT NOT NULL,
    size_bytes INTEGER NOT NULL,
    file_path TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

INSERT INTO thread_artifacts_new
    SELECT * FROM thread_artifacts;

DROP TABLE thread_artifacts;

ALTER TABLE thread_artifacts_new RENAME TO thread_artifacts;

CREATE INDEX IF NOT EXISTS idx_thread_artifacts_thread
    ON thread_artifacts(thread_id);

CREATE INDEX IF NOT EXISTS idx_thread_artifacts_user
    ON thread_artifacts(user_id);
