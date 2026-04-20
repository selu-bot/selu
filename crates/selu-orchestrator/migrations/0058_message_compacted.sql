-- Soft-delete for compacted messages: instead of deleting non-delegation tool
-- messages after a turn completes, mark them compacted so clients can still
-- display them (collapsed) while the LLM context excludes them.
ALTER TABLE messages ADD COLUMN compacted INTEGER NOT NULL DEFAULT 0;
