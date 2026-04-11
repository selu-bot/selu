-- Allow any channel name (not just stable/dev/nightly).
-- SQLite cannot ALTER CHECK constraints, so recreate the table.

CREATE TABLE system_update_settings_new (
    id                TEXT PRIMARY KEY,
    release_channel   TEXT NOT NULL DEFAULT 'stable',
    auto_check        INTEGER NOT NULL DEFAULT 1,
    auto_update       INTEGER NOT NULL DEFAULT 0,
    updated_at        TEXT NOT NULL DEFAULT (datetime('now'))
);

INSERT INTO system_update_settings_new
    SELECT id, release_channel, auto_check, auto_update, updated_at
    FROM system_update_settings;

DROP TABLE system_update_settings;

ALTER TABLE system_update_settings_new RENAME TO system_update_settings;
