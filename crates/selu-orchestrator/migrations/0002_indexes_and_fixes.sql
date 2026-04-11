-- Additional indexes for production performance

-- Pipes per user
CREATE INDEX IF NOT EXISTS idx_pipes_user_id ON pipes(user_id);

-- Event subscriptions: fast fanout lookups
CREATE INDEX IF NOT EXISTS idx_event_subs_user_event ON event_subscriptions(user_id, event_type);

-- Web sessions: fast session lookup by user and expiry check
CREATE INDEX IF NOT EXISTS idx_web_sessions_user_id ON web_sessions(user_id);
CREATE INDEX IF NOT EXISTS idx_web_sessions_expires ON web_sessions(expires_at);

-- User credentials: lookup by user + capability
CREATE INDEX IF NOT EXISTS idx_user_creds_lookup ON user_credentials(user_id, capability_id);

-- Sessions: lookup by user
CREATE INDEX IF NOT EXISTS idx_sessions_user_id ON sessions(user_id);

-- Workspaces: lookup by session + status for TTL cleanup
CREATE INDEX IF NOT EXISTS idx_workspaces_session ON workspaces(session_id);
CREATE INDEX IF NOT EXISTS idx_workspaces_status ON workspaces(status);
