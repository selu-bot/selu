-- Agent auto-update support
--
-- Tracks whether each agent should be automatically updated when
-- a newer version is available in the marketplace.
-- 0 = manual updates only (default), 1 = auto-update enabled.

ALTER TABLE agents ADD COLUMN auto_update INTEGER NOT NULL DEFAULT 0;
