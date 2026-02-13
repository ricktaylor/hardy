PRAGMA foreign_keys = ON;
PRAGMA journal_mode = WAL;

CREATE TABLE bundles (
    id INTEGER PRIMARY KEY,
    bundle_id BLOB NOT NULL UNIQUE,
    expiry TEXT NOT NULL,
    received_at TEXT NOT NULL,
    status_code INTEGER,
    status_param1 INTEGER,
    status_param2 INTEGER,
    status_param3 TEXT,
    bundle BLOB 
) 
STRICT;

CREATE INDEX idx_bundles_expiry ON bundles(expiry ASC);
CREATE INDEX idx_bundles_status ON bundles(status_code);
CREATE INDEX idx_bundles_status_peer ON bundles(status_code, status_param1);
CREATE INDEX idx_bundles_received_at ON bundles(received_at ASC);

CREATE TABLE unconfirmed_bundles (
    id INTEGER UNIQUE NOT NULL REFERENCES bundles(id) ON DELETE CASCADE
) STRICT;

CREATE TABLE waiting_queue (
    id INTEGER UNIQUE NOT NULL REFERENCES bundles(id) ON DELETE CASCADE,
    received_at TEXT NOT NULL
) STRICT;

CREATE INDEX idx_waiting_queue_received_at ON waiting_queue(received_at ASC);
