-- Users
CREATE TABLE IF NOT EXISTS users (
    id          TEXT PRIMARY KEY,
    username    TEXT NOT NULL UNIQUE,
    display_name TEXT NOT NULL,
    password_hash TEXT NOT NULL,
    created_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Pipes
CREATE TABLE IF NOT EXISTS pipes (
    id              TEXT PRIMARY KEY,
    user_id         TEXT NOT NULL REFERENCES users(id),
    name            TEXT NOT NULL,
    transport       TEXT NOT NULL DEFAULT 'webhook',
    inbound_token   TEXT NOT NULL,
    outbound_url    TEXT NOT NULL,
    outbound_auth   TEXT,
    default_agent_id TEXT,
    active          INTEGER NOT NULL DEFAULT 1,
    created_at      TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Messages (full thread history)
CREATE TABLE IF NOT EXISTS messages (
    id          TEXT PRIMARY KEY,
    pipe_id     TEXT NOT NULL REFERENCES pipes(id),
    session_id  TEXT REFERENCES sessions(id),
    role        TEXT NOT NULL CHECK (role IN ('user', 'assistant', 'system', 'tool')),
    content     TEXT NOT NULL,
    created_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_messages_pipe_id ON messages(pipe_id);
CREATE INDEX IF NOT EXISTS idx_messages_session_id ON messages(session_id);
CREATE INDEX IF NOT EXISTS idx_messages_created_at ON messages(created_at);

-- Sessions
CREATE TABLE IF NOT EXISTS sessions (
    id              TEXT PRIMARY KEY,
    pipe_id         TEXT NOT NULL REFERENCES pipes(id),
    user_id         TEXT NOT NULL REFERENCES users(id),
    agent_id        TEXT NOT NULL,
    status          TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'idle', 'closed')),
    workspace_id    TEXT,
    created_at      TEXT NOT NULL DEFAULT (datetime('now')),
    last_active_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_sessions_pipe_id ON sessions(pipe_id);
CREATE INDEX IF NOT EXISTS idx_sessions_status ON sessions(status);

-- LLM providers (API keys stored encrypted)
CREATE TABLE IF NOT EXISTS llm_providers (
    id                  TEXT PRIMARY KEY,  -- 'openai', 'anthropic', 'ollama'
    display_name        TEXT NOT NULL,
    api_key_encrypted   TEXT,
    base_url            TEXT,
    active              INTEGER NOT NULL DEFAULT 1,
    created_at          TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Installed agents registry (agents/ dir is source of truth for definition)
CREATE TABLE IF NOT EXISTS agents (
    id              TEXT PRIMARY KEY,
    display_name    TEXT NOT NULL,
    version         TEXT NOT NULL DEFAULT '0.1.0',
    installed_at    TEXT NOT NULL DEFAULT (datetime('now'))
);

-- User credentials (per-user per-capability, e.g. Google OAuth tokens)
CREATE TABLE IF NOT EXISTS user_credentials (
    id                  TEXT PRIMARY KEY,
    user_id             TEXT NOT NULL REFERENCES users(id),
    capability_id       TEXT NOT NULL,
    credential_name     TEXT NOT NULL,
    encrypted_value     TEXT NOT NULL,
    expires_at          TEXT,
    created_at          TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(user_id, capability_id, credential_name)
);

-- System credentials (shared across users, set at install time)
CREATE TABLE IF NOT EXISTS system_credentials (
    id                  TEXT PRIMARY KEY,
    capability_id       TEXT NOT NULL,
    credential_name     TEXT NOT NULL,
    encrypted_value     TEXT NOT NULL,
    created_at          TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(capability_id, credential_name)
);

-- Workspaces (environment capability instances)
CREATE TABLE IF NOT EXISTS workspaces (
    id              TEXT PRIMARY KEY,
    session_id      TEXT NOT NULL REFERENCES sessions(id),
    capability_id   TEXT NOT NULL,
    status          TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'suspended', 'destroyed')),
    ttl_hours       INTEGER NOT NULL DEFAULT 24,
    created_at      TEXT NOT NULL DEFAULT (datetime('now')),
    suspended_at    TEXT
);

-- Agent events (emitted by agent sessions)
CREATE TABLE IF NOT EXISTS agent_events (
    id                  TEXT PRIMARY KEY,
    source_session_id   TEXT NOT NULL REFERENCES sessions(id),
    source_agent_id     TEXT NOT NULL,
    event_type          TEXT NOT NULL,
    payload_json        TEXT NOT NULL,
    chain_depth         INTEGER NOT NULL DEFAULT 0,
    emitted_at          TEXT NOT NULL DEFAULT (datetime('now')),
    processed_at        TEXT
);

CREATE INDEX IF NOT EXISTS idx_agent_events_type ON agent_events(event_type);
CREATE INDEX IF NOT EXISTS idx_agent_events_processed ON agent_events(processed_at);

-- Event subscriptions (per-user reactions to agent events)
CREATE TABLE IF NOT EXISTS event_subscriptions (
    id                  TEXT PRIMARY KEY,
    user_id             TEXT NOT NULL REFERENCES users(id),
    event_type          TEXT NOT NULL,
    reaction_type       TEXT NOT NULL CHECK (reaction_type IN ('pipe_notification', 'agent_invocation')),
    reaction_config_json TEXT NOT NULL DEFAULT '{}',
    filter_expression   TEXT,         -- CEL expression
    filter_description  TEXT,         -- natural language description of the filter
    active              INTEGER NOT NULL DEFAULT 1,
    created_at          TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Web UI sessions (cookie-based auth)
CREATE TABLE IF NOT EXISTS web_sessions (
    id          TEXT PRIMARY KEY,
    user_id     TEXT NOT NULL REFERENCES users(id),
    expires_at  TEXT NOT NULL,
    created_at  TEXT NOT NULL DEFAULT (datetime('now'))
);
