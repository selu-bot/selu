-- Durable, generic background jobs shared by API and service features.

CREATE TABLE jobs (
    id               TEXT PRIMARY KEY,
    owner_user_id    TEXT REFERENCES users(id) ON DELETE SET NULL,
    kind             TEXT NOT NULL,
    resource_id      TEXT NOT NULL,
    status           TEXT NOT NULL CHECK (status IN (
        'queued', 'running', 'waiting_for_input', 'succeeded', 'failed',
        'cancelling', 'cancelled', 'interrupted'
    )),
    progress         INTEGER NOT NULL DEFAULT 0 CHECK (progress BETWEEN 0 AND 100),
    message_code     TEXT,
    result_json      TEXT CHECK (result_json IS NULL OR json_valid(result_json)),
    error_json       TEXT CHECK (error_json IS NULL OR json_valid(error_json)),
    idempotency_key  TEXT,
    created_at       TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at       TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    started_at       TEXT,
    completed_at     TEXT
);

CREATE UNIQUE INDEX idx_jobs_user_idempotency
    ON jobs(owner_user_id, kind, idempotency_key)
    WHERE owner_user_id IS NOT NULL AND idempotency_key IS NOT NULL;
CREATE UNIQUE INDEX idx_jobs_system_idempotency
    ON jobs(kind, idempotency_key)
    WHERE owner_user_id IS NULL AND idempotency_key IS NOT NULL;
CREATE INDEX idx_jobs_owner_status_updated
    ON jobs(owner_user_id, status, updated_at DESC);
CREATE INDEX idx_jobs_kind_resource
    ON jobs(kind, resource_id, created_at DESC);
CREATE INDEX idx_jobs_reconcile
    ON jobs(status, updated_at)
    WHERE status IN ('running', 'cancelling');
