-- Separate from migration 4: an index predicate cannot reference an enum
-- value until the ALTER TYPE that added it has committed.

-- Partial index for the per-service delivery channel, mirroring
-- idx_metadata_waiting_for_service: the channel poller's poll_pending pages
-- by (received_at, id) within one service_eid, and reset_service_queue's
-- sweep is a single UPDATE by service_eid using the same prefix. No index
-- for 'delivery_ack_pending': nothing polls or sweeps it.
CREATE INDEX IF NOT EXISTS idx_metadata_deliver_pending
    ON metadata (service_eid, received_at, id)
    WHERE status = 'deliver_pending';
