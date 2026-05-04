-- Mark threads created by schedules so clients can keep them out of the main
-- conversation list, and reuse one thread per schedule + pipe.
ALTER TABLE threads ADD COLUMN thread_kind TEXT NOT NULL DEFAULT 'conversation';
ALTER TABLE threads ADD COLUMN schedule_id TEXT;

CREATE INDEX IF NOT EXISTS idx_threads_pipe_kind_activity
    ON threads(pipe_id, thread_kind, created_at);

CREATE UNIQUE INDEX IF NOT EXISTS idx_threads_pipe_schedule
    ON threads(pipe_id, schedule_id)
    WHERE schedule_id IS NOT NULL;
