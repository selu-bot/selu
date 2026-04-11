-- Add explicit updater state-machine fields.

ALTER TABLE system_update_state
ADD COLUMN status TEXT NOT NULL DEFAULT 'idle'
    CHECK (status IN ('idle', 'checking', 'update_available', 'updating', 'rollback_available', 'failed'));

ALTER TABLE system_update_state
ADD COLUMN progress_key TEXT NOT NULL DEFAULT 'updates.progress.idle';

ALTER TABLE system_update_state
ADD COLUMN last_attempt_at TEXT;
