ALTER TABLE schedules ADD COLUMN agent_id TEXT;
CREATE INDEX idx_schedules_agent_id ON schedules(agent_id);
