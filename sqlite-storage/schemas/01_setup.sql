CREATE TABLE bundles (
    id INTEGER PRIMARY KEY,
    bundle_id BLOB UNIQUE NOT NULL,
    bundle BLOB,
    expiry TEXT NOT NULL
) STRICT;

CREATE TABLE unconfirmed_bundles (
    id INTEGER UNIQUE NOT NULL REFERENCES bundles(id) ON DELETE CASCADE
) STRICT;
