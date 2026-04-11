-- Track real workspace activity so TTL cleanup is based on last use
-- rather than initial creation time.

ALTER TABLE workspaces
    ADD COLUMN last_active_at TEXT;

-- Backfill historical rows to preserve existing TTL behavior for old data.
UPDATE workspaces
SET last_active_at = created_at
WHERE last_active_at IS NULL OR last_active_at = '';

CREATE INDEX IF NOT EXISTS idx_workspaces_status_last_active
    ON workspaces(status, last_active_at);

CREATE INDEX IF NOT EXISTS idx_workspaces_session_status
    ON workspaces(session_id, status);
