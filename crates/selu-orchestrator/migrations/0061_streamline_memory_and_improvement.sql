-- Streamline Selu's persistence model around three clear concepts:
--   user_profile: always-available facts about the user
--   agent_memories: shared, searchable notes (agent_id is provenance only)
--   agent_insights: per-agent behavioral lessons
--
-- The removed columns and table were never populated or consumed by a
-- user-facing capability. Historical migrations remain unchanged so existing
-- installations upgrade safely.

ALTER TABLE agent_memories DROP COLUMN category;

ALTER TABLE turn_signals DROP COLUMN was_retry;
ALTER TABLE turn_signals DROP COLUMN was_abandoned;
ALTER TABLE turn_signals DROP COLUMN agent_switched;

ALTER TABLE agent_insights DROP COLUMN promotion_threshold;
ALTER TABLE agent_insights DROP COLUMN auto_paused;

DROP TABLE improvement_metrics;
