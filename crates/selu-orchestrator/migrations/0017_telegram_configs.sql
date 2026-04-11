-- Telegram bot adapter configuration
CREATE TABLE telegram_configs (
    id              TEXT PRIMARY KEY,
    name            TEXT NOT NULL,
    bot_token       TEXT NOT NULL,
    chat_id         TEXT NOT NULL,           -- Telegram chat ID to monitor (group or private chat)
    pipe_id         TEXT NOT NULL REFERENCES pipes(id),
    active          INTEGER NOT NULL DEFAULT 1,
    created_at      TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX idx_telegram_configs_pipe_id ON telegram_configs(pipe_id);
