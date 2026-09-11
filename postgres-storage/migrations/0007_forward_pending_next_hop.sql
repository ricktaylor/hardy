-- The ForwardPending queue-assignment record carries the resolved adjacency
-- EID, so the egress channels' at-least-once recovery re-delivers the
-- routing decision intact. Nullable: only ForwardPending rows populate it,
-- and it is payload, not queue identity — never queried on its own.
ALTER TABLE metadata ADD COLUMN next_hop TEXT;

-- Rows written before this migration predate the persisted adjacency: their
-- queue assignment cannot be recovered (an adjacency-less forward_pending no
-- longer decodes, and the restart consistency sweep would destroy the
-- bundle), so re-park them to waiting for the next dispatch pass to re-route.
UPDATE metadata SET status = 'waiting', peer_id = NULL, queue_id = NULL
    WHERE status = 'forward_pending';
