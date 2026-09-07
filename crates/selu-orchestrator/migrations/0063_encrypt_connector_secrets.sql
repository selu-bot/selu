-- Connector secrets are encrypted at application startup with the existing
-- AES-256-GCM CredentialStore, then the legacy plaintext columns are cleared.
-- The legacy NOT NULL columns remain as empty compatibility placeholders so
-- existing SQLite installations can migrate without rebuilding core tables.
ALTER TABLE pipes ADD COLUMN inbound_token_encrypted TEXT;
ALTER TABLE pipes ADD COLUMN outbound_auth_encrypted TEXT;
ALTER TABLE telegram_configs ADD COLUMN bot_token_encrypted TEXT;
ALTER TABLE bluebubbles_configs ADD COLUMN server_password_encrypted TEXT;
