-- Preserve each automation's wall-clock intent independently of later profile changes.
ALTER TABLE schedules ADD COLUMN timezone TEXT NOT NULL DEFAULT 'UTC';

-- Existing schedules previously inherited their owner's current timezone at execution time.
-- Freeze that effective value during migration so their next recurrence does not shift.
UPDATE schedules
SET timezone = COALESCE(
    (SELECT NULLIF(TRIM(users.timezone), '') FROM users WHERE users.id = schedules.user_id),
    'UTC'
);
