-- Upgrade messages.created_at from second-precision datetime('now') to
-- millisecond-precision strftime('%Y-%m-%dT%H:%M:%f','now').  This prevents
-- tool-loop messages persisted in the same second from having undefined
-- ordering when loaded via ORDER BY created_at.
--
-- SQLite cannot ALTER COLUMN defaults, but new inserts will use the
-- application-supplied value.  We add a trigger as a safety net so any
-- INSERT that relies on the old default gets millisecond precision instead.

-- Safety-net trigger: if an INSERT leaves created_at at second precision
-- (length 19, e.g. "2026-03-30 12:00:00"), upgrade it in-place.
CREATE TRIGGER IF NOT EXISTS trg_messages_ms_precision
AFTER INSERT ON messages
WHEN length(NEW.created_at) <= 19
BEGIN
    UPDATE messages
       SET created_at = strftime('%Y-%m-%dT%H:%M:%f', NEW.created_at)
     WHERE id = NEW.id;
END;
