-- Threads: sub-conversations within a pipe+session.
-- When a user sends a message while an agent is still processing,
-- a new thread is created so conversations don't interleave.
CREATE TABLE IF NOT EXISTS threads (
    id              TEXT PRIMARY KEY,
    pipe_id         TEXT NOT NULL REFERENCES pipes(id),
    session_id      TEXT NOT NULL REFERENCES sessions(id),
    user_id         TEXT NOT NULL REFERENCES users(id),
    status          TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'completed', 'failed')),
    -- The original inbound message GUID from the adapter (e.g. BlueBubbles message GUID)
    -- Used for reply-to-message correlation on outbound.
    origin_message_ref TEXT,
    created_at      TEXT NOT NULL DEFAULT (datetime('now')),
    completed_at    TEXT
);

CREATE INDEX IF NOT EXISTS idx_threads_pipe_session ON threads(pipe_id, session_id);
CREATE INDEX IF NOT EXISTS idx_threads_user_status ON threads(user_id, status);
CREATE INDEX IF NOT EXISTS idx_threads_session_status ON threads(session_id, status);

-- Add thread_id to messages so each message belongs to a specific thread.
ALTER TABLE messages ADD COLUMN thread_id TEXT REFERENCES threads(id);

CREATE INDEX IF NOT EXISTS idx_messages_thread_id ON messages(thread_id);

-- Sender reference mappings: maps external sender identities (phone numbers, etc.)
-- to Selu users on a per-pipe basis.
-- This is the gatekeeper: only mapped senders get responses.
CREATE TABLE IF NOT EXISTS user_sender_refs (
    id              TEXT PRIMARY KEY,
    user_id         TEXT NOT NULL REFERENCES users(id),
    pipe_id         TEXT NOT NULL REFERENCES pipes(id),
    sender_ref      TEXT NOT NULL,  -- e.g. "+14155551234", "user@example.com"
    created_at      TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(pipe_id, sender_ref)
);

CREATE INDEX IF NOT EXISTS idx_sender_refs_lookup ON user_sender_refs(pipe_id, sender_ref);
CREATE INDEX IF NOT EXISTS idx_sender_refs_user ON user_sender_refs(user_id);

-- BlueBubbles adapter configuration (stored in DB for web-based management).
-- One row per BlueBubbles server instance.
CREATE TABLE IF NOT EXISTS bluebubbles_configs (
    id              TEXT PRIMARY KEY,
    name            TEXT NOT NULL,
    -- BlueBubbles server connection
    server_url      TEXT NOT NULL,       -- e.g. "http://localhost:1234"
    server_password  TEXT NOT NULL,       -- BB server password (stored encrypted? TBD)
    -- Chat GUID to monitor (the iMessage group/DM this adapter watches)
    chat_guid       TEXT NOT NULL,
    -- The pipe this adapter feeds into
    pipe_id         TEXT NOT NULL REFERENCES pipes(id),
    -- Polling config
    poll_interval_ms INTEGER NOT NULL DEFAULT 2000,
    active          INTEGER NOT NULL DEFAULT 1,
    created_at      TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_bb_configs_pipe ON bluebubbles_configs(pipe_id);
