ALTER TABLE agents
    ADD COLUMN autonomy_level TEXT NOT NULL DEFAULT 'medium';

ALTER TABLE agents
    ADD COLUMN max_delegation_hops INTEGER NOT NULL DEFAULT 2;

ALTER TABLE user_agent_runtime_settings
    ADD COLUMN autonomy_level TEXT NOT NULL DEFAULT 'medium';

ALTER TABLE user_agent_runtime_settings
    ADD COLUMN use_advanced_limits INTEGER NOT NULL DEFAULT 0;

ALTER TABLE user_agent_runtime_settings
    ADD COLUMN max_delegation_hops INTEGER NOT NULL DEFAULT 2;

-- Map existing numeric step settings to easy-mode autonomy where possible.
UPDATE user_agent_runtime_settings
SET autonomy_level = 'low',
    use_advanced_limits = 0
WHERE max_tool_loop_iterations = 8;

UPDATE user_agent_runtime_settings
SET autonomy_level = 'medium',
    use_advanced_limits = 0
WHERE max_tool_loop_iterations = 20;

UPDATE user_agent_runtime_settings
SET autonomy_level = 'high',
    use_advanced_limits = 0
WHERE max_tool_loop_iterations = 40;

-- Keep custom historic values as advanced overrides.
UPDATE user_agent_runtime_settings
SET autonomy_level = 'medium',
    use_advanced_limits = 1
WHERE max_tool_loop_iterations NOT IN (8, 20, 40);

-- Agent defaults: derive easy autonomy from the existing default step setting.
UPDATE agents
SET autonomy_level = CASE
    WHEN max_tool_loop_iterations = 8 THEN 'low'
    WHEN max_tool_loop_iterations = 40 THEN 'high'
    ELSE 'medium'
END;

-- Agent defaults: derive delegation hop defaults from autonomy.
UPDATE agents
SET max_delegation_hops = CASE
    WHEN autonomy_level = 'low' THEN 1
    WHEN autonomy_level = 'high' THEN 3
    ELSE 2
END;
