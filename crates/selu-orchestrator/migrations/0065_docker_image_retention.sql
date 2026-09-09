-- Durable ownership and retention metadata for Selu-managed Docker images.
-- Image deletion is always driven by exact immutable image IDs; tags are kept
-- only as display/resolution metadata and are never passed to cleanup.
CREATE TABLE agent_package_revisions (
    id              TEXT PRIMARY KEY,
    agent_id        TEXT NOT NULL,
    version         TEXT NOT NULL DEFAULT '',
    package_path    TEXT,
    state           TEXT NOT NULL CHECK (state IN ('staged', 'current', 'previous', 'failed', 'uninstalled')),
    retain_until    TEXT,
    created_at      TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at      TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE UNIQUE INDEX one_current_agent_package_revision
    ON agent_package_revisions(agent_id)
    WHERE state = 'current';
CREATE INDEX agent_package_revisions_retention
    ON agent_package_revisions(state, retain_until);

CREATE TABLE managed_docker_images (
    image_id        TEXT PRIMARY KEY,
    size_bytes      INTEGER NOT NULL CHECK (size_bytes >= 0),
    display_name    TEXT NOT NULL,
    first_seen_at   TEXT NOT NULL DEFAULT (datetime('now')),
    last_seen_at    TEXT NOT NULL DEFAULT (datetime('now'))
);


CREATE TABLE managed_docker_image_repositories (
    image_id        TEXT NOT NULL REFERENCES managed_docker_images(image_id) ON DELETE CASCADE,
    repository      TEXT NOT NULL,
    created_at      TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at      TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY(image_id, repository)
);

CREATE TABLE managed_docker_image_refs (
    id                  TEXT PRIMARY KEY,
    agent_revision_id   TEXT NOT NULL REFERENCES agent_package_revisions(id) ON DELETE CASCADE,
    image_id            TEXT NOT NULL REFERENCES managed_docker_images(image_id) ON DELETE CASCADE,
    image_ref           TEXT NOT NULL,
    repo_digest         TEXT,
    created_at          TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at          TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(agent_revision_id, image_ref, image_id)
);

CREATE INDEX managed_docker_image_refs_image
    ON managed_docker_image_refs(image_id);
CREATE INDEX managed_docker_image_refs_revision
    ON managed_docker_image_refs(agent_revision_id);

CREATE TABLE docker_storage_state (
    id                  TEXT PRIMARY KEY CHECK (id = 'global'),
    bootstrap_status    TEXT NOT NULL DEFAULT 'pending'
        CHECK (bootstrap_status IN ('pending', 'ready', 'blocked')),
    blocked_reason      TEXT NOT NULL DEFAULT '',
    bootstrapped_at     TEXT,
    last_cleanup_at     TEXT NOT NULL DEFAULT '',
    updated_at          TEXT NOT NULL DEFAULT (datetime('now'))
);

INSERT INTO docker_storage_state (id) VALUES ('global');

CREATE TABLE managed_docker_system_refs (
    id              TEXT PRIMARY KEY,
    owner           TEXT NOT NULL CHECK (owner IN ('selu', 'updater', 'whatsapp')),
    state           TEXT NOT NULL CHECK (state IN ('current', 'previous', 'superseded')),
    image_id        TEXT NOT NULL REFERENCES managed_docker_images(image_id) ON DELETE CASCADE,
    image_ref       TEXT NOT NULL,
    retain_until    TEXT,
    created_at      TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at      TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(owner, image_ref, image_id)
);

CREATE INDEX managed_docker_system_refs_image
    ON managed_docker_system_refs(image_id);
CREATE INDEX managed_docker_system_refs_retention
    ON managed_docker_system_refs(state, retain_until);
