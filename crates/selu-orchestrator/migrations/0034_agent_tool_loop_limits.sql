ALTER TABLE agents
    ADD COLUMN max_tool_loop_iterations INTEGER NOT NULL DEFAULT 32;

CREATE TABLE IF NOT EXISTS user_agent_runtime_settings (
    user_id                  TEXT NOT NULL,
    agent_id                 TEXT NOT NULL,
    max_tool_loop_iterations INTEGER NOT NULL,
    updated_at               TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (user_id, agent_id)
);

CREATE INDEX IF NOT EXISTS idx_user_agent_runtime_settings_agent
    ON user_agent_runtime_settings(agent_id);
