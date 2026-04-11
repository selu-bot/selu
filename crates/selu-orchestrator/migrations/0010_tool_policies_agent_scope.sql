-- Fix tool_policies unique constraint to include agent_id.
--
-- The original constraint UNIQUE(user_id, capability_id, tool_name) caused
-- policies to collide across agents sharing the same capability_id — most
-- notably the built-in tools (__builtin__/delegate_to_agent, __builtin__/emit_event)
-- which are present in every agent.  An upsert for agent B would silently
-- update the row belonging to agent A (without changing agent_id), making
-- the policy invisible when loading agent B's detail page.
--
-- SQLite cannot ALTER a UNIQUE constraint, so we recreate the table.

CREATE TABLE tool_policies_new (
    id            TEXT PRIMARY KEY,
    user_id       TEXT NOT NULL,
    agent_id      TEXT NOT NULL,
    capability_id TEXT NOT NULL,
    tool_name     TEXT NOT NULL,
    policy        TEXT NOT NULL CHECK(policy IN ('allow', 'ask', 'block')),
    created_at    DATETIME DEFAULT (datetime('now')),
    updated_at    DATETIME DEFAULT (datetime('now')),
    UNIQUE(user_id, agent_id, capability_id, tool_name)
);

INSERT INTO tool_policies_new (id, user_id, agent_id, capability_id, tool_name, policy, created_at, updated_at)
    SELECT id, user_id, agent_id, capability_id, tool_name, policy, created_at, updated_at
    FROM tool_policies;

DROP TABLE tool_policies;
ALTER TABLE tool_policies_new RENAME TO tool_policies;

CREATE INDEX IF NOT EXISTS idx_tool_policies_user_agent
    ON tool_policies(user_id, agent_id);
