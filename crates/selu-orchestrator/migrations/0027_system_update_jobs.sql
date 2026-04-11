-- Track explicit updater jobs and active/last-good server image references.

CREATE TABLE IF NOT EXISTS system_update_jobs (
    id               TEXT PRIMARY KEY,
    action           TEXT NOT NULL CHECK (action IN ('check', 'apply', 'rollback')),
    status           TEXT NOT NULL,
    channel          TEXT NOT NULL,
    requested_tag    TEXT NOT NULL DEFAULT '',
    requested_digest TEXT NOT NULL DEFAULT '',
    progress_key     TEXT NOT NULL DEFAULT '',
    error_message    TEXT NOT NULL DEFAULT '',
    initiator        TEXT NOT NULL DEFAULT 'system',
    started_at       TEXT NOT NULL DEFAULT (datetime('now')),
    finished_at      TEXT
);

CREATE INDEX IF NOT EXISTS idx_system_update_jobs_started_at
    ON system_update_jobs(started_at DESC);

ALTER TABLE system_update_state
ADD COLUMN active_job_id TEXT;

ALTER TABLE system_update_state
ADD COLUMN last_good_tag TEXT NOT NULL DEFAULT '';

ALTER TABLE system_update_state
ADD COLUMN last_good_digest TEXT NOT NULL DEFAULT '';
