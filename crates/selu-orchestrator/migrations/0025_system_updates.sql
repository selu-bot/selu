-- Server update settings and state for release channels (stable/dev/nightly).

CREATE TABLE IF NOT EXISTS system_update_settings (
    id                TEXT PRIMARY KEY,
    release_channel   TEXT NOT NULL DEFAULT 'stable' CHECK (release_channel IN ('stable', 'dev', 'nightly')),
    auto_check        INTEGER NOT NULL DEFAULT 1,
    auto_update       INTEGER NOT NULL DEFAULT 0,
    updated_at        TEXT NOT NULL DEFAULT (datetime('now'))
);

INSERT OR IGNORE INTO system_update_settings (id) VALUES ('global');

CREATE TABLE IF NOT EXISTS system_update_state (
    id                 TEXT PRIMARY KEY,
    installed_version  TEXT NOT NULL DEFAULT '',
    installed_digest   TEXT NOT NULL DEFAULT '',
    available_version  TEXT NOT NULL DEFAULT '',
    available_digest   TEXT NOT NULL DEFAULT '',
    previous_version   TEXT NOT NULL DEFAULT '',
    previous_digest    TEXT NOT NULL DEFAULT '',
    last_checked_at    TEXT,
    last_error         TEXT NOT NULL DEFAULT '',
    updated_at         TEXT NOT NULL DEFAULT (datetime('now'))
);

INSERT OR IGNORE INTO system_update_state (id) VALUES ('global');
