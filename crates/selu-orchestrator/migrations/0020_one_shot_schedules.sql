-- Support one-shot reminders alongside recurring cron schedules.
-- When one_shot = 1, the schedule fires once at next_run_at and then
-- deactivates instead of computing the next cron occurrence.
ALTER TABLE schedules ADD COLUMN one_shot INTEGER NOT NULL DEFAULT 0;
