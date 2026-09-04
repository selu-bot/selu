-- Durable, client-neutral state for the v1 conversation API.  Existing
-- threads and messages remain the source of truth during the compatibility
-- period; these tables add run identity and a replayable UI event journal.

CREATE TABLE IF NOT EXISTS conversation_runs (
    id                TEXT PRIMARY KEY,
    thread_id         TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    user_id           TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    client_message_id TEXT NOT NULL,
    status            TEXT NOT NULL CHECK (status IN (
        'queued', 'running', 'waiting_for_approval', 'cancelling',
        'completed', 'failed', 'cancelled', 'interrupted'
    )),
    error_code        TEXT,
    created_at        TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    started_at        TEXT,
    completed_at      TEXT,
    UNIQUE(user_id, client_message_id)
);

CREATE INDEX IF NOT EXISTS idx_conversation_runs_thread_active
    ON conversation_runs(thread_id, status, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_conversation_runs_user_created
    ON conversation_runs(user_id, created_at DESC);

CREATE TABLE IF NOT EXISTS conversation_events (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id          TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    thread_id        TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    run_id           TEXT REFERENCES conversation_runs(id) ON DELETE CASCADE,
    event_type       TEXT NOT NULL,
    entity_id        TEXT,
    payload_json     TEXT NOT NULL,
    created_at       TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_conversation_events_user_id
    ON conversation_events(user_id, id);
CREATE INDEX IF NOT EXISTS idx_conversation_events_thread_id
    ON conversation_events(thread_id, id);
