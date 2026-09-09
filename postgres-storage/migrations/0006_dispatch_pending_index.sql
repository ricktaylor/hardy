-- Partial index for the dispatch queue's storage poller: poll_pending pages
-- by (received_at, id) within status = 'dispatch_pending' — the successor of
-- the retired 'dispatching' poll target.
CREATE INDEX IF NOT EXISTS idx_metadata_dispatch_pending
    ON metadata (received_at ASC, id ASC)
    WHERE status = 'dispatch_pending';

-- Nothing polls 'dispatching' any more (bundles in that status are claimed,
-- in-flight; recovery re-enqueues them from the bundle-store walk), so its
-- index is read-dead: drop it to stop paying maintenance on every
-- claim/unclaim transition.
DROP INDEX IF EXISTS idx_metadata_dispatching;
