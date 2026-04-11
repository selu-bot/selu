-- Increase medium autonomy delegation budget.
UPDATE agents
SET max_delegation_hops = CASE
    WHEN autonomy_level = 'low' THEN 1
    WHEN autonomy_level = 'medium' THEN 5
    WHEN autonomy_level = 'high' THEN 6
    ELSE max_delegation_hops
END;
