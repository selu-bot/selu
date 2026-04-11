ALTER TABLE system_update_settings
    ADD COLUMN installation_telemetry_opt_out INTEGER NOT NULL DEFAULT 0;
