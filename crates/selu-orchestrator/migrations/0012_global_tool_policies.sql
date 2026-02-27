-- Global (agent-wide) tool policies.
--
-- These are the default policies that apply to ALL users unless a user
-- has set a personal override in the existing `tool_policies` table.
--
-- The admin sets global policies during agent setup.  Other users inherit
-- them automatically — no web UI visit required.

CREATE TABLE global_tool_policies (
    id            TEXT PRIMARY KEY,
    agent_id      TEXT NOT NULL,
    capability_id TEXT NOT NULL,
    tool_name     TEXT NOT NULL,
    policy        TEXT NOT NULL CHECK(policy IN ('allow', 'ask', 'block')),
    created_at    DATETIME DEFAULT (datetime('now')),
    updated_at    DATETIME DEFAULT (datetime('now')),
    UNIQUE(agent_id, capability_id, tool_name)
);

CREATE INDEX idx_global_tool_policies_agent ON global_tool_policies(agent_id);

-- Seed global policies from existing per-user policies.
-- For each (agent_id, capability_id, tool_name) group, pick the policy
-- from the earliest-created row (the admin who ran the setup wizard).
INSERT INTO global_tool_policies (id, agent_id, capability_id, tool_name, policy, created_at, updated_at)
    SELECT
        id,
        agent_id,
        capability_id,
        tool_name,
        policy,
        created_at,
        updated_at
    FROM tool_policies
    WHERE rowid IN (
        SELECT rowid FROM (
            SELECT rowid,
                   ROW_NUMBER() OVER (
                       PARTITION BY agent_id, capability_id, tool_name
                       ORDER BY created_at ASC
                   ) AS rn
            FROM tool_policies
        )
        WHERE rn = 1
    );

-- Remove the seeded rows from tool_policies so they don't act as
-- redundant per-user overrides.  We only remove rows that were copied
-- into global_tool_policies (same agent_id, capability_id, tool_name,
-- belonging to the admin who set them up).
DELETE FROM tool_policies
WHERE id IN (SELECT id FROM global_tool_policies);
