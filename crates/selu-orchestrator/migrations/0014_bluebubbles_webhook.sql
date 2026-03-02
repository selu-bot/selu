-- Switch BlueBubbles integration from polling to webhooks.
-- Store the BB-assigned webhook ID so we can deregister on teardown.
-- poll_interval_ms is kept for backward compat but no longer used.
ALTER TABLE bluebubbles_configs ADD COLUMN bb_webhook_id TEXT;
