CREATE TABLE bundles (
    id INTEGER PRIMARY KEY,
    status INTEGER NOT NULL DEFAULT(0),
    storage_name TEXT UNIQUE NOT NULL,
    hash TEXT NOT NULL,
    received_at TEXT NOT NULL DEFAULT(datetime('now','utc','subsecond')),
    flags INTEGER NOT NULL,
    crc_type INTEGER NOT NULL,
    source BLOB NOT NULL,
    destination BLOB NOT NULL,
    report_to BLOB NOT NULL,
    creation_time INTEGER NOT NULL,
    creation_seq_num INTEGER NOT NULL,
    lifetime INTEGER NOT NULL,
    fragment_offset INTEGER NOT NULL DEFAULT(-1),
    fragment_total_len INTEGER NOT NULL DEFAULT(-1),

    UNIQUE(source,creation_time,creation_seq_num,fragment_offset,fragment_total_len)
) STRICT;

CREATE TABLE bundle_blocks (
    id INTEGER PRIMARY KEY,
    bundle_id INTEGER NOT NULL REFERENCES bundles(id) ON DELETE CASCADE,
    block_type INTEGER NOT NULL,
    block_num INTEGER NOT NULL,
    block_flags INTEGER NOT NULL,
    block_crc_type INTEGER NOT NULL,
    data_offset INTEGER NOT NULL,
    data_len INTEGER NOT NULL
) STRICT;

CREATE TABLE unconfirmed_bundles (
    bundle_id INTEGER UNIQUE NOT NULL REFERENCES bundles(id) ON DELETE CASCADE
) STRICT;
