-- Rebalance autonomy presets so medium/high are useful for complex workflows.
-- Keep existing user overrides intact; only update agent defaults.
UPDATE agents
SET max_tool_loop_iterations = CASE
    WHEN autonomy_level = 'low' THEN 24
    WHEN autonomy_level = 'medium' THEN 64
    WHEN autonomy_level = 'high' THEN 0
    ELSE max_tool_loop_iterations
END;

UPDATE agents
SET max_delegation_hops = CASE
    WHEN autonomy_level = 'low' THEN 1
    WHEN autonomy_level = 'medium' THEN 3
    WHEN autonomy_level = 'high' THEN 6
    ELSE max_delegation_hops
END;
