-- Transfers accepted by a CLA that reports its outcome out-of-band are
-- retained in a distinct status until the outcome arrives.
ALTER TYPE bundle_status ADD VALUE IF NOT EXISTS 'forward_ack_pending';
