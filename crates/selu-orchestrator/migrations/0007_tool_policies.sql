-- Per-user, per-tool permission policies for capability tool calls.
--
-- Before every gRPC tool invocation the orchestrator checks the user's
-- policy for that tool:
--   allow  -- invoke immediately
--   ask    -- prompt for confirmation (interactive channel) or send
--             approval request via threaded reply (non-interactive channel)
--   block  -- deny with a user-visible message
--
-- Tools without an explicit policy are blocked by default.  Agent
-- installation includes a mandatory step where the user reviews and
-- sets a policy for every tool the agent provides.

CREATE TABLE IF NOT EXISTS tool_policies (
    id            TEXT PRIMARY KEY,
    user_id       TEXT NOT NULL,
    agent_id      TEXT NOT NULL,
    capability_id TEXT NOT NULL,
    tool_name     TEXT NOT NULL,
    policy        TEXT NOT NULL CHECK(policy IN ('allow', 'ask', 'block')),
    created_at    DATETIME DEFAULT (datetime('now')),
    updated_at    DATETIME DEFAULT (datetime('now')),
    UNIQUE(user_id, capability_id, tool_name)
);

CREATE INDEX IF NOT EXISTS idx_tool_policies_user_agent
    ON tool_policies(user_id, agent_id);

-- Pending asynchronous tool-call approvals.
--
-- When an "ask" policy fires on a non-interactive threaded channel
-- (e.g. iMessage), the tool loop suspends and an approval prompt is
-- sent to the user.  This table tracks the pending request so that
-- the user's reply (an inline-reply to the prompt message) can be
-- intercepted and routed back to the waiting task.

CREATE TABLE IF NOT EXISTS pending_tool_approvals (
    id              TEXT PRIMARY KEY,
    user_id         TEXT NOT NULL,
    session_id      TEXT NOT NULL,
    thread_id       TEXT NOT NULL,
    pipe_id         TEXT NOT NULL,
    agent_id        TEXT NOT NULL,
    capability_id   TEXT NOT NULL,
    tool_name       TEXT NOT NULL,
    args_json       TEXT NOT NULL,
    tool_call_id    TEXT NOT NULL,
    status          TEXT NOT NULL CHECK(status IN ('pending', 'approved', 'denied', 'expired')),
    created_at      DATETIME DEFAULT (datetime('now')),
    expires_at      DATETIME NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_pending_approvals_thread
    ON pending_tool_approvals(thread_id, status);

CREATE INDEX IF NOT EXISTS idx_pending_approvals_expiry
    ON pending_tool_approvals(expires_at);

CREATE INDEX IF NOT EXISTS idx_pending_approvals_user
    ON pending_tool_approvals(user_id, status);
