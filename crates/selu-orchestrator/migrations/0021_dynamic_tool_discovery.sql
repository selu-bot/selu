-- Dynamic capability tool discovery cache and sync status.
--
-- Dynamic capabilities do not statically declare tool definitions in manifest.yaml.
-- Selu discovers their tools by invoking the capability's discovery tool
-- (default: list_tools) and stores the resulting tool metadata per
-- (agent_id, capability_id, tool_name).

CREATE TABLE IF NOT EXISTS discovered_tools (
    agent_id            TEXT NOT NULL,
    capability_id       TEXT NOT NULL,
    tool_name           TEXT NOT NULL,
    description         TEXT NOT NULL,
    input_schema_json   TEXT NOT NULL,
    recommended_policy  TEXT NULL CHECK(recommended_policy IN ('allow', 'ask', 'block')),
    discovered_at       DATETIME DEFAULT (datetime('now')),
    updated_at          DATETIME DEFAULT (datetime('now')),
    PRIMARY KEY (agent_id, capability_id, tool_name)
);

CREATE INDEX IF NOT EXISTS idx_discovered_tools_agent_cap
    ON discovered_tools(agent_id, capability_id);

CREATE TABLE IF NOT EXISTS tool_discovery_state (
    agent_id           TEXT NOT NULL,
    capability_id      TEXT NOT NULL,
    last_sync_at       DATETIME DEFAULT (datetime('now')),
    last_sync_status   TEXT NOT NULL CHECK(last_sync_status IN ('ok', 'error')),
    last_sync_error    TEXT NULL,
    PRIMARY KEY (agent_id, capability_id)
);

