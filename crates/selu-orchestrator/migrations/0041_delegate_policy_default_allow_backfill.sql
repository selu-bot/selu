-- Backfill explicit global policy rows so delegation is allowed by default.
-- This aligns DB state with the runtime fallback for __builtin__/delegate_to_agent.
INSERT OR IGNORE INTO global_tool_policies (id, agent_id, capability_id, tool_name, policy)
SELECT
    lower(hex(randomblob(16))) AS id,
    a.id AS agent_id,
    '__builtin__' AS capability_id,
    'delegate_to_agent' AS tool_name,
    'allow' AS policy
FROM agents a;
