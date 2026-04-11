-- Optional image model configuration.
--
-- Text LLM defaults stay in `default_model_config`.
-- Image generation/editing uses a separate optional configuration so image
-- features can remain disabled unless explicitly configured.

-- Per-agent image model override (NULL = use global image default)
ALTER TABLE agents ADD COLUMN image_provider_id TEXT;
ALTER TABLE agents ADD COLUMN image_model_id TEXT;

-- Global default image model configuration
CREATE TABLE IF NOT EXISTS default_image_model_config (
    id              TEXT PRIMARY KEY DEFAULT 'global',
    provider_id     TEXT NOT NULL DEFAULT '',
    model_id        TEXT NOT NULL DEFAULT '',
    updated_at      TEXT NOT NULL DEFAULT (datetime('now'))
);

INSERT OR IGNORE INTO default_image_model_config (id) VALUES ('global');
