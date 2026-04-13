-- Mobile app setup tokens: one-time tokens generated from the web UI
-- that allow the iOS app to authenticate without entering credentials.

CREATE TABLE IF NOT EXISTS mobile_setup_tokens (
    token     TEXT PRIMARY KEY,
    user_id   TEXT NOT NULL REFERENCES users(id),
    used      INTEGER NOT NULL DEFAULT 0,
    expires_at TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
