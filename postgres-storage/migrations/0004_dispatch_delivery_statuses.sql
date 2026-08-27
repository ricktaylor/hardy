-- no-transaction
-- Runs outside sqlx's migration transaction: an enum value added by ALTER
-- TYPE cannot be referenced until the addition commits, and PostgreSQL runs
-- a multi-statement migration as one implicit transaction even without an
-- explicit BEGIN. IF NOT EXISTS makes a partial run re-apply cleanly.

-- Bundles queued in the dispatch channel are held in a distinct status so
-- the channel's storage poller only recovers queued bundles, never one the
-- consumer has already claimed to 'dispatching' for processing.
ALTER TYPE bundle_status ADD VALUE IF NOT EXISTS 'dispatch_pending';

-- Local delivery mirrors forwarding: bundles queued in a service's delivery
-- channel ('deliver_pending', analogue of 'forward_pending') are claimed to
-- 'delivery_ack_pending' (analogue of 'forward_ack_pending') by the queue
-- consumer before on_deliver, so the channel's storage poller only ever
-- recovers genuinely queued bundles.
ALTER TYPE bundle_status ADD VALUE IF NOT EXISTS 'deliver_pending';
ALTER TYPE bundle_status ADD VALUE IF NOT EXISTS 'delivery_ack_pending';
