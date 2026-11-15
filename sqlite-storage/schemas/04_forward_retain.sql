-- Persist the ForwardPending / ForwardAckPending retain flag so
-- opportunistic replication survives route removal mid-flight.
ALTER TABLE bundles ADD COLUMN retain INTEGER NOT NULL DEFAULT 0;
