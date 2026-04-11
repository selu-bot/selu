-- Per-user network controls for agent capabilities.
--
-- Users can:
--   - allow or deny network access per capability
--   - override host policy entries (allow/deny) per capability
--
-- Resolution happens at runtime with this precedence:
--   1) user capability access override (deny/allow)
--   2) user host overrides (allow/deny)
--   3) capability manifest defaults

CREATE TABLE IF NOT EXISTS user_network_access_policies (
    id            TEXT PRIMARY KEY,
    user_id       TEXT NOT NULL,
    agent_id      TEXT NOT NULL,
    capability_id TEXT NOT NULL,
    access        TEXT NOT NULL CHECK(access IN ('allow', 'deny')),
    created_at    DATETIME DEFAULT (datetime('now')),
    updated_at    DATETIME DEFAULT (datetime('now')),
    UNIQUE(user_id, agent_id, capability_id)
);

CREATE INDEX IF NOT EXISTS idx_user_network_access_user_agent
    ON user_network_access_policies(user_id, agent_id);

CREATE TABLE IF NOT EXISTS user_network_host_policies (
    id            TEXT PRIMARY KEY,
    user_id       TEXT NOT NULL,
    agent_id      TEXT NOT NULL,
    capability_id TEXT NOT NULL,
    host          TEXT NOT NULL,
    policy        TEXT NOT NULL CHECK(policy IN ('allow', 'deny')),
    created_at    DATETIME DEFAULT (datetime('now')),
    updated_at    DATETIME DEFAULT (datetime('now')),
    UNIQUE(user_id, agent_id, capability_id, host)
);

CREATE INDEX IF NOT EXISTS idx_user_network_hosts_user_agent
    ON user_network_host_policies(user_id, agent_id);
