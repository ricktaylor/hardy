-- The ForwardPending queue-assignment record carries the resolved adjacency
-- EID, so the egress channels' at-least-once recovery re-delivers the
-- routing decision intact. Nullable: only ForwardPending rows populate it,
-- and it is payload, not queue identity — never queried on its own.
ALTER TABLE metadata ADD COLUMN next_hop TEXT;
