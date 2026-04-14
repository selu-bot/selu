-- Change push_notifications default to enabled
UPDATE system_update_settings SET push_notifications_enabled = 1;

-- Enable auto-update for selu itself (was off by default)
UPDATE system_update_settings SET auto_update = 1 WHERE id = 'global' AND auto_update = 0;

-- Enable auto-update for all agents (was off by default)
UPDATE agents SET auto_update = 1 WHERE auto_update = 0;
