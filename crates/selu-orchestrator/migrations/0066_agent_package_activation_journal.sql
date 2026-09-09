-- Durable recovery record for the non-transactional filesystem portion of an
-- agent package update. The row is inserted before the first rename and deleted
-- only by the transaction that either commits activation or records rollback.
CREATE TABLE agent_package_activation_journal (
    agent_id        TEXT PRIMARY KEY REFERENCES agents(id) ON DELETE RESTRICT,
    old_revision_id TEXT NOT NULL REFERENCES agent_package_revisions(id) ON DELETE RESTRICT,
    new_revision_id TEXT NOT NULL REFERENCES agent_package_revisions(id) ON DELETE RESTRICT,
    active_path     TEXT NOT NULL,
    retained_path   TEXT NOT NULL,
    staged_path     TEXT NOT NULL,
    phase           TEXT NOT NULL CHECK (phase IN (
        'prepared',
        'old_retained',
        'new_active',
        'rollback_new_staged',
        'rollback_old_active'
    )),
    created_at      TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at      TEXT NOT NULL DEFAULT (datetime('now')),
    CHECK (old_revision_id <> new_revision_id)
);

CREATE UNIQUE INDEX agent_package_activation_journal_new_revision
    ON agent_package_activation_journal(new_revision_id);
