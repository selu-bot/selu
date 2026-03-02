CREATE TABLE IF NOT EXISTS instance_meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

INSERT OR IGNORE INTO instance_meta (key, value)
VALUES ('instance_id', lower(hex(randomblob(16))));
