-- Separate from migration 2: an index predicate cannot reference an enum
-- value until the ALTER TYPE that added it has committed.

-- Partial index for reset_peer_ack_pending's sweep, mirroring
-- idx_metadata_forward_pending. The sweep is a single UPDATE by peer with no
-- keyset pagination, so peer_id alone is the whole key.
CREATE INDEX IF NOT EXISTS idx_metadata_forward_ack_pending
    ON metadata (peer_id)
    WHERE status = 'forward_ack_pending';
