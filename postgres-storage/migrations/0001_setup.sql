CREATE TYPE bundle_status AS ENUM (
    'new',
    'waiting',
    'dispatching',
    'forward_pending',
    'adu_fragment',
    'waiting_for_service'
);

-- Identity anchor and tombstone guard.
-- A row exists here for every bundle identity ever seen — active or tombstoned.
-- The UNIQUE constraint on bundle_id is the deduplication and resurrection-prevention mechanism.
CREATE TABLE bundles (
    id          BIGSERIAL   PRIMARY KEY,
    bundle_id   TEXT        NOT NULL UNIQUE,  -- CBOR+base64url via bundle::Id::to_key()
    received_at TIMESTAMPTZ NOT NULL
);

-- Owns all lifecycle state. Absent for tombstoned bundles.
-- received_at is denormalized from bundles so poll queries with keyset pagination
-- remain single-table without a join.
CREATE TABLE metadata (
    id          BIGINT          PRIMARY KEY REFERENCES bundles (id),
    expiry      TIMESTAMPTZ     NOT NULL,
    received_at TIMESTAMPTZ     NOT NULL,
    status      bundle_status   NOT NULL,

    -- Status parameters (NULL when not applicable to the current status)
    peer_id     INTEGER,        -- ForwardPending.peer
    queue_id    INTEGER,        -- ForwardPending.queue
    adu_source  TEXT,           -- AduFragment.source (EID string)
    adu_ts_ms   BIGINT,         -- AduFragment.timestamp (milliseconds, 0 = no DTN clock)
    adu_ts_seq  BIGINT,         -- AduFragment.sequence_number
    service_eid TEXT,           -- WaitingForService.service (EID string)

    -- Full hardy_bpa::bundle::Bundle as serde_json bytes stored in BYTEA.
    -- Typed columns above are projections for indexing; this is the authoritative source.
    -- BYTEA avoids PostgreSQL's JSON parser, which rejects \u0000 and lone surrogates.
    bundle      BYTEA           NOT NULL
);

-- Partial indexes for polling queries — each targets exactly one query pattern.
-- id is included in all keyset-paginated indexes so the (col, id) composite cursor
-- can be satisfied entirely from the index without a sort step.
CREATE INDEX idx_metadata_expiry
    ON metadata (expiry ASC, id ASC)
    WHERE status != 'new';

CREATE INDEX idx_metadata_waiting
    ON metadata (received_at ASC, id ASC)
    WHERE status = 'waiting';

CREATE INDEX idx_metadata_forward_pending
    ON metadata (peer_id, received_at ASC, id ASC)
    WHERE status = 'forward_pending';

CREATE INDEX idx_metadata_adu_fragment
    ON metadata (adu_source, adu_ts_ms, adu_ts_seq, received_at ASC, id ASC)
    WHERE status = 'adu_fragment';

CREATE INDEX idx_metadata_service_waiting
    ON metadata (service_eid, received_at ASC, id ASC)
    WHERE status = 'waiting_for_service';

CREATE INDEX idx_metadata_dispatching
    ON metadata (received_at ASC, id ASC)
    WHERE status = 'dispatching';

-- Tracks metadata rows not yet confirmed during startup recovery.
-- ON DELETE CASCADE cleans up automatically when its metadata row is tombstoned.
CREATE TABLE unconfirmed (
    id  BIGINT  NOT NULL UNIQUE
                REFERENCES metadata (id) ON DELETE CASCADE
);
