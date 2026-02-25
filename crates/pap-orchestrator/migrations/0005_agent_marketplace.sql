-- Agent marketplace: extend agents table + global default model config
--
-- The agents table becomes the source of truth for installed agents.
-- Agent definitions (YAML/MD) live on the filesystem (installed_agents_dir)
-- but the DB tracks what is installed, model assignments, and setup state.

-- Per-agent model assignment (NULL = use global default)
ALTER TABLE agents ADD COLUMN provider_id TEXT;
ALTER TABLE agents ADD COLUMN model_id TEXT;
ALTER TABLE agents ADD COLUMN temperature REAL NOT NULL DEFAULT 0.7;

-- Marketplace tracking
ALTER TABLE agents ADD COLUMN source_url TEXT;          -- archive_url it was installed from
ALTER TABLE agents ADD COLUMN is_bundled INTEGER NOT NULL DEFAULT 0;  -- 1 for the default agent
ALTER TABLE agents ADD COLUMN setup_complete INTEGER NOT NULL DEFAULT 1;

-- Global default model configuration
CREATE TABLE IF NOT EXISTS default_model_config (
    id              TEXT PRIMARY KEY DEFAULT 'global',
    provider_id     TEXT NOT NULL DEFAULT '',
    model_id        TEXT NOT NULL DEFAULT '',
    temperature     REAL NOT NULL DEFAULT 0.7,
    updated_at      TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Seed a placeholder row (user configures via settings after first login)
INSERT OR IGNORE INTO default_model_config (id) VALUES ('global');
