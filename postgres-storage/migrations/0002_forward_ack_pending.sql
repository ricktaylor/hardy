-- no-transaction
-- Runs outside sqlx's migration transaction: an enum value added by ALTER
-- TYPE cannot be referenced until the addition commits, and PostgreSQL runs
-- a multi-statement migration as one implicit transaction even without an
-- explicit BEGIN. This migration therefore holds only the ADD VALUE; the
-- index that references the value is migration 3. IF NOT EXISTS makes a
-- partial run re-apply cleanly.

-- Transfers accepted by a CLA that reports its outcome out-of-band are
-- retained in a distinct status until the outcome arrives.
ALTER TYPE bundle_status ADD VALUE IF NOT EXISTS 'forward_ack_pending';
