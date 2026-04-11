-- WhatsApp adapter configuration (Baileys webhook bridge)
CREATE TABLE whatsapp_configs (
    id              TEXT PRIMARY KEY,
    name            TEXT NOT NULL,
    bridge_url      TEXT NOT NULL,          -- Where Selu POSTs outbound replies (Baileys bridge)
    pipe_id         TEXT NOT NULL REFERENCES pipes(id),
    active          INTEGER NOT NULL DEFAULT 1,
    created_at      TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX idx_whatsapp_configs_pipe_id ON whatsapp_configs(pipe_id);
