-- Persist user-facing release metadata for system updates.

ALTER TABLE system_update_state
ADD COLUMN installed_release_version TEXT NOT NULL DEFAULT '';

ALTER TABLE system_update_state
ADD COLUMN installed_build_number TEXT NOT NULL DEFAULT '';

ALTER TABLE system_update_state
ADD COLUMN available_release_version TEXT NOT NULL DEFAULT '';

ALTER TABLE system_update_state
ADD COLUMN available_build_number TEXT NOT NULL DEFAULT '';

ALTER TABLE system_update_state
ADD COLUMN available_changelog_url TEXT NOT NULL DEFAULT '';

ALTER TABLE system_update_state
ADD COLUMN previous_release_version TEXT NOT NULL DEFAULT '';

ALTER TABLE system_update_state
ADD COLUMN previous_build_number TEXT NOT NULL DEFAULT '';
