CREATE TABLE bundles (
    id TEXT PRIMARY KEY,
    bundle TEXT,
    expiry TEXT NOT NULL
) STRICT;

CREATE TABLE unconfirmed_bundles (
    bundle_id TEXT UNIQUE NOT NULL REFERENCES bundles(id) ON DELETE CASCADE
) STRICT;

PRAGMA journal_mode=WAL;
