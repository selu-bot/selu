-- Agent self-improvement: turn signals, behavioral insights, and improvement metrics.
--
-- turn_signals: per-turn implicit + explicit quality signals.
-- agent_insights: behavioral lessons extracted from signal patterns.
-- improvement_metrics: daily aggregated metrics for trend display.

-- ── Turn signals ─────────────────────────────────────────────────────────────
CREATE TABLE turn_signals (
    id                   TEXT PRIMARY KEY NOT NULL,
    agent_id             TEXT NOT NULL,
    user_id              TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    thread_id            TEXT,
    turn_index           INTEGER NOT NULL DEFAULT 0,

    -- Explicit feedback (null = not rated, -1 = negative, 1 = positive)
    user_rating          INTEGER,

    -- Implicit signals (auto-collected from the tool loop)
    tool_calls_count     INTEGER NOT NULL DEFAULT 0,
    tool_failures_count  INTEGER NOT NULL DEFAULT 0,
    tool_loop_iterations INTEGER NOT NULL DEFAULT 0,
    was_retry            INTEGER NOT NULL DEFAULT 0,
    was_abandoned        INTEGER NOT NULL DEFAULT 0,
    agent_switched       INTEGER NOT NULL DEFAULT 0,

    -- Context for LLM insight extraction (first 200 chars only)
    user_message_preview TEXT,
    agent_tools_used     TEXT,

    created_at           TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f', 'now'))
);

CREATE INDEX idx_turn_signals_agent_user
    ON turn_signals(agent_id, user_id, created_at);

CREATE INDEX idx_turn_signals_thread
    ON turn_signals(thread_id)
    WHERE thread_id IS NOT NULL;

-- ── Behavioral insights ──────────────────────────────────────────────────────
CREATE TABLE agent_insights (
    id                   TEXT PRIMARY KEY NOT NULL,
    agent_id             TEXT NOT NULL,
    user_id              TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,

    lesson_text          TEXT NOT NULL,
    insight_type         TEXT NOT NULL DEFAULT 'workflow_pattern'
                         CHECK (insight_type IN (
                             'tool_usage',
                             'communication_style',
                             'workflow_pattern',
                             'error_prevention'
                         )),

    status               TEXT NOT NULL DEFAULT 'candidate'
                         CHECK (status IN (
                             'candidate',
                             'active',
                             'paused',
                             'rejected',
                             'superseded'
                         )),

    confidence           REAL NOT NULL DEFAULT 0.0,
    supporting_signals   INTEGER NOT NULL DEFAULT 1,
    promotion_threshold  INTEGER NOT NULL DEFAULT 3,

    auto_paused          INTEGER NOT NULL DEFAULT 0,

    created_at           TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f', 'now')),
    activated_at         TEXT,
    updated_at           TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f', 'now'))
);

CREATE INDEX idx_agent_insights_active
    ON agent_insights(agent_id, user_id, status)
    WHERE status = 'active';

CREATE INDEX idx_agent_insights_agent_user
    ON agent_insights(agent_id, user_id, updated_at DESC);

-- ── Improvement metrics (daily aggregates) ───────────────────────────────────
CREATE TABLE improvement_metrics (
    id                   TEXT PRIMARY KEY NOT NULL,
    agent_id             TEXT NOT NULL,
    user_id              TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    date                 TEXT NOT NULL,

    total_turns          INTEGER NOT NULL DEFAULT 0,
    positive_ratings     INTEGER NOT NULL DEFAULT 0,
    negative_ratings     INTEGER NOT NULL DEFAULT 0,
    tool_failures        INTEGER NOT NULL DEFAULT 0,
    avg_tool_iterations  REAL NOT NULL DEFAULT 0.0,
    retry_count          INTEGER NOT NULL DEFAULT 0,

    UNIQUE(agent_id, user_id, date)
);
