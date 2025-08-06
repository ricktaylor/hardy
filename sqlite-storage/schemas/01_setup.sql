CREATE TABLE bundles (
    id INTEGER PRIMARY KEY,
    bundle_id TEXT UNIQUE NOT NULL,
    bundle TEXT,
    expiry TEXT NOT NULL
) STRICT;

CREATE TABLE unconfirmed_bundles (
    id INTEGER UNIQUE NOT NULL REFERENCES bundles(id) ON DELETE CASCADE
) STRICT;

PRAGMA journal_mode=WAL;
