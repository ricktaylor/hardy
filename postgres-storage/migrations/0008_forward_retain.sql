-- Persist the ForwardPending / ForwardAckPending retain flag so
-- opportunistic replication survives route removal mid-flight.
ALTER TABLE metadata ADD COLUMN retain BOOLEAN NOT NULL DEFAULT FALSE;
