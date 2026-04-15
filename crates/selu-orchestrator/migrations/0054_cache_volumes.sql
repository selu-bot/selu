-- User-scoped persistent cache volumes for capability containers.
-- Unlike session-scoped workspace volumes, cache volumes survive across
-- sessions so that package-manager caches (cargo, npm, go, pip) stay warm.
-- Management is manual via the web UI (no automatic TTL).

CREATE TABLE IF NOT EXISTS cache_volumes (
    id              TEXT PRIMARY KEY,
    user_id         TEXT NOT NULL,
    capability_id   TEXT NOT NULL,
    status          TEXT NOT NULL DEFAULT 'active'
                    CHECK (status IN ('active', 'destroyed')),
    created_at      TEXT NOT NULL DEFAULT (datetime('now')),
    last_active_at  TEXT
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_cache_volumes_user_cap
    ON cache_volumes(user_id, capability_id)
    WHERE status = 'active';
