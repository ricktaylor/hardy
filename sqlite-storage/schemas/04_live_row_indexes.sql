-- Keep tombstones out of the secondary indexes. A tombstone is a row whose
-- bundle and status columns are NULL: it stays in the table so that its
-- bundle_id still refuses a replay, but no query selects it by expiry or
-- by status. Left in the indexes, each one costs two entries for as long
-- as its row lives, completing a bundle moves its status entry into the
-- run of NULLs at the front of the status index, and the expiry poll walks
-- every tombstone that sorts ahead of the first live bundle.
--
-- Both predicates are ones the planner proves from the queries themselves:
-- every status-keyed query compares status_code with =, which a NULL never
-- satisfies, and the expiry poll and the recovery scan carry the term
-- `bundle IS NOT NULL` word for word.
DROP INDEX idx_bundles_expiry;
CREATE INDEX idx_bundles_expiry ON bundles(expiry ASC) WHERE bundle IS NOT NULL;

DROP INDEX idx_bundles_status_received;
CREATE INDEX idx_bundles_status_received ON bundles(status_code, status_param1, status_param2, received_at ASC, status_param3) WHERE status_code IS NOT NULL;

-- No query orders by received_at alone: the queue polls sort within
-- idx_bundles_status_received, and the waiting-queue refresh reads
-- received_at from that index.
DROP INDEX idx_bundles_received_at;
