CREATE TABLE bundles (
    id INTEGER PRIMARY KEY,
    bundle_id BLOB UNIQUE NOT NULL,
    expiry TEXT NOT NULL,
    status_code INTEGER,
    bundle BLOB 
) STRICT;

CREATE INDEX idx_bundles_expiry ON bundles(expiry ASC);
CREATE INDEX idx_bundles_status ON bundles(status_code);

CREATE TABLE unconfirmed_bundles (
    id INTEGER UNIQUE NOT NULL REFERENCES bundles(id) ON DELETE CASCADE
) STRICT;
