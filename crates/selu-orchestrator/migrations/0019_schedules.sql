-- User timezone for schedule evaluation
ALTER TABLE users ADD COLUMN timezone TEXT NOT NULL DEFAULT 'UTC';

-- Schedule definitions
CREATE TABLE schedules (
    id               TEXT PRIMARY KEY,
    user_id          TEXT NOT NULL,
    name             TEXT NOT NULL,
    prompt           TEXT NOT NULL,
    cron_expression  TEXT NOT NULL,
    cron_description TEXT NOT NULL DEFAULT '',
    active           INTEGER NOT NULL DEFAULT 1,
    last_run_at      TEXT,
    next_run_at      TEXT NOT NULL,
    created_at       TEXT NOT NULL DEFAULT (datetime('now')),
    FOREIGN KEY (user_id) REFERENCES users(id)
);

-- Junction: one schedule can fire on multiple pipes
CREATE TABLE schedule_pipes (
    schedule_id TEXT NOT NULL,
    pipe_id     TEXT NOT NULL,
    PRIMARY KEY (schedule_id, pipe_id),
    FOREIGN KEY (schedule_id) REFERENCES schedules(id) ON DELETE CASCADE,
    FOREIGN KEY (pipe_id)     REFERENCES pipes(id)     ON DELETE CASCADE
);

CREATE INDEX idx_schedules_user ON schedules(user_id);
CREATE INDEX idx_schedules_next_run ON schedules(active, next_run_at);
