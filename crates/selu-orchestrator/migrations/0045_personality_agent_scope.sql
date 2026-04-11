-- Allow personality facts to be scoped to a specific agent.
-- Facts with agent_id = NULL remain global (visible to all agents).
-- Facts with an agent_id are only injected into that agent's context.
ALTER TABLE user_personality ADD COLUMN agent_id TEXT REFERENCES agents(id) ON DELETE CASCADE;

CREATE INDEX idx_personality_agent ON user_personality(agent_id);
