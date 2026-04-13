-- Add optional callback_base_url to bluebubbles_configs.
-- When NULL, the adapter defaults to http://localhost:{port} instead of the
-- public URL, avoiding an unnecessary internet round trip for the local
-- BlueBubbles server.
ALTER TABLE bluebubbles_configs ADD COLUMN callback_base_url TEXT;
