-- Serve the poll_pending page query in index order: without this index every
-- drain page top-K-sorts the entire matching backlog (a TEMP B-TREE per
-- page — O(B^2) row visits to drain B bundles), in exactly the overload
-- regime the storage poller exists for.
--
-- Column order: received_at precedes status_param3 so the sort is in-index
-- immediately after the (status_code, status_param1, status_param2)
-- equality prefix; status_param3 is still checked in-index past
-- received_at. This order also stays optimal when a poll shape leaves
-- status_param3 unconstrained (the forwarding-queue drain planned in the
-- egress rework, where param3 carries each row's own payload).
--
-- Both dropped indexes are strict leading prefixes of the new one, so every
-- other status-keyed query keeps an equal-or-better plan.
CREATE INDEX idx_bundles_status_received ON bundles(status_code, status_param1, status_param2, received_at ASC, status_param3);

DROP INDEX idx_bundles_status;
DROP INDEX idx_bundles_status_peer;
