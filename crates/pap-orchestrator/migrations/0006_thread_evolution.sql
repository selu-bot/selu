-- Evolve threads from one-shot per-message to long-lived conversations.
--
-- Threads become the "conversation" concept: each thread keeps its own
-- conversation history, can accumulate multiple exchanges, and has a
-- human-readable title for the UI sidebar.
--
-- BlueBubbles reply-to threading: `last_reply_guid` stores PAP's outbound
-- message GUID so incoming iMessage replies can be matched to the correct
-- thread.

ALTER TABLE threads ADD COLUMN title TEXT;
ALTER TABLE threads ADD COLUMN last_reply_guid TEXT;

CREATE INDEX IF NOT EXISTS idx_threads_reply_guid ON threads(last_reply_guid);
CREATE INDEX IF NOT EXISTS idx_threads_origin_ref ON threads(origin_message_ref);
