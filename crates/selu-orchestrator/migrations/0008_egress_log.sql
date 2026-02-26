-- Egress proxy request log.
--
-- Every HTTP/HTTPS request routed through the egress proxy is recorded
-- here so users can see exactly what network calls their capability
-- containers make, whether they were allowed or denied, and which host
-- they targeted.

CREATE TABLE IF NOT EXISTS egress_log (
    id              TEXT PRIMARY KEY,
    capability_id   TEXT NOT NULL,
    method          TEXT NOT NULL,     -- "CONNECT" or HTTP method
    host            TEXT NOT NULL,     -- target hostname
    port            INTEGER NOT NULL,
    allowed         INTEGER NOT NULL,  -- 1 = allowed, 0 = denied
    created_at      DATETIME DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_egress_log_capability
    ON egress_log(capability_id, created_at);

CREATE INDEX IF NOT EXISTS idx_egress_log_created
    ON egress_log(created_at);
