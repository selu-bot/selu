ALTER TABLE system_update_settings
ADD COLUMN push_notifications_enabled INTEGER NOT NULL DEFAULT 1;

-- Enable auto-update for selu itself (was off by default)
UPDATE system_update_settings SET auto_update = 1 WHERE id = 'global' AND auto_update = 0;

-- Enable auto-update for all agents (was off by default)
UPDATE agents SET auto_update = 1 WHERE auto_update = 0;
