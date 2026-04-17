-- Store APNs device tokens per user for server-initiated push notifications
-- (e.g. schedule completions).  A user may have multiple devices.
CREATE TABLE IF NOT EXISTS mobile_device_tokens (
    user_id      TEXT NOT NULL REFERENCES users(id),
    device_token TEXT NOT NULL,
    updated_at   TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (user_id, device_token)
);
