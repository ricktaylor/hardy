-- no-transaction
-- Runs outside sqlx's migration transaction: an enum value added by ALTER
-- TYPE cannot be referenced by the index below until the addition commits.
-- Both statements are IF NOT EXISTS so a partial run re-applies cleanly.

-- Transfers accepted by a CLA that reports its outcome out-of-band are
-- retained in a distinct status until the outcome arrives.
ALTER TYPE bundle_status ADD VALUE IF NOT EXISTS 'forward_ack_pending';

-- Partial index for reset_peer_ack_pending's sweep, mirroring
-- idx_metadata_forward_pending. The sweep is a single UPDATE by peer with no
-- keyset pagination, so peer_id alone is the whole key.
CREATE INDEX IF NOT EXISTS idx_metadata_forward_ack_pending
    ON metadata (peer_id)
    WHERE status = 'forward_ack_pending';
