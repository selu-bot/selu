-- Add is_admin flag to users table.
--
-- The first user created via /setup is the administrator.  Admin users
-- can manage global tool policies; non-admin users can only set personal
-- overrides.
--
-- SQLite allows ALTER TABLE ... ADD COLUMN with a DEFAULT, so we don't
-- need to recreate the table.  We then mark the earliest-created user
-- as admin.

ALTER TABLE users ADD COLUMN is_admin INTEGER NOT NULL DEFAULT 0;

-- Mark the first user (by created_at) as admin
UPDATE users
SET is_admin = 1
WHERE id = (SELECT id FROM users ORDER BY created_at ASC LIMIT 1);
