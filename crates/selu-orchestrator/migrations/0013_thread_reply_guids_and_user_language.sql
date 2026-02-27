-- Fix iMessage thread context loss: store ALL outbound message GUIDs
-- so that replying to any Selu message routes back to the correct thread.
--
-- Previously, only the single most-recent reply GUID was stored in
-- threads.last_reply_guid, so replying to an older message would create
-- a new thread and lose conversation context.

CREATE TABLE IF NOT EXISTS thread_reply_guids (
    id          TEXT PRIMARY KEY,
    thread_id   TEXT NOT NULL REFERENCES threads(id),
    pipe_id     TEXT NOT NULL REFERENCES pipes(id),
    reply_guid  TEXT NOT NULL,
    created_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_thread_reply_guids_lookup
    ON thread_reply_guids(pipe_id, reply_guid);
CREATE INDEX IF NOT EXISTS idx_thread_reply_guids_thread
    ON thread_reply_guids(thread_id);

-- Add language preference to users for server-side i18n on non-web
-- channels (iMessage, webhooks). Defaults to 'de' for German.
ALTER TABLE users ADD COLUMN language TEXT NOT NULL DEFAULT 'de';
