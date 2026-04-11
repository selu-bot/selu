-- Store changelog body text for inline display.

ALTER TABLE system_update_state
ADD COLUMN available_changelog_body TEXT NOT NULL DEFAULT '';
