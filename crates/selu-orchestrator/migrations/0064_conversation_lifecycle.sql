-- Add presentation lifecycle metadata without changing thread origin semantics.
-- Existing threads stay visible; newly-created empty conversations remain
-- provisional until their first message is accepted.
ALTER TABLE threads ADD COLUMN started_at TEXT;
ALTER TABLE threads ADD COLUMN saved_at TEXT;

UPDATE threads
SET started_at = created_at
WHERE started_at IS NULL;

CREATE INDEX IF NOT EXISTS idx_threads_user_started_activity
    ON threads(user_id, started_at, created_at)
    WHERE started_at IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_threads_user_saved_activity
    ON threads(user_id, saved_at, created_at)
    WHERE saved_at IS NOT NULL;
