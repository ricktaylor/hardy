CREATE TABLE bundles (
    id INTEGER PRIMARY KEY,
    status INTEGER NOT NULL DEFAULT(0),
    file_name TEXT UNIQUE NOT NULL,
    hash TEXT NOT NULL,
    received_at DATE NOT NULL DEFAULT(datetime('now','utc','subsecond')),
    flags INTEGER NOT NULL,
    destination BLOB NOT NULL,
    creation_time INTEGER NOT NULL,
    creation_seq_num INTEGER NOT NULL,
    lifetime INTEGER NOT NULL,
    source BLOB,
    report_to BLOB
);

CREATE TABLE bundle_blocks (
    id INTEGER PRIMARY KEY,
    bundle_id INTEGER NOT NULL REFERENCES bundles(id) ON DELETE CASCADE,
    block_type INTEGER NOT NULL,
    block_num INTEGER NOT NULL,
    block_flags INTEGER NOT NULL,
    data_offset INTEGER
);

CREATE TABLE bundle_fragments (
    id INTEGER PRIMARY KEY,
    bundle_id INTEGER NOT NULL REFERENCES bundles(id) ON DELETE CASCADE,
    offset INTEGER NOT NULL,
    total_len INTEGER NOT NULL
);

CREATE TABLE unconfirmed_bundles (
    id INTEGER PRIMARY KEY,
    bundle_id INTEGER UNIQUE NOT NULL REFERENCES bundles(id) ON DELETE CASCADE
);
