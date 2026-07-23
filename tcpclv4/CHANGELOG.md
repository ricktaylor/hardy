# Changelog

All notable changes to `hardy-tcpclv4` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this crate adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed
- Bundle dispatch to the BPA moved off the session reader loop into a per-session ordered ingest task: segments, acknowledgments of outbound transfers, and keepalives keep flowing while a received bundle is being stored. The final `XFER_ACK` of a transfer is still only sent after dispatch completes, and acknowledgments are emitted in segment-arrival order.
- Outbound segments are queued to the writer task without a per-segment completion round-trip, and the writer flushes when its command queue runs dry. The session never awaits a blocked write without concurrently processing inbound messages, removing a mutual-stall risk when both peers send large transfers simultaneously.

### Fixed
- A mid-transfer `XFER_REFUSE` now clears all outstanding acknowledgment expectations for the refused transfer (RFC 9174 Section 5.2.2 sends no further `XFER_ACK` messages for it), where previously stale expectations desynchronised the acknowledgment matcher and tore down the session.
- A dial timeout no longer stalls a forward behind repeated connection attempts when a session to the peer already exists: the forward falls back to queueing on the busy session after one attempt. The TCP connect itself is now bounded by `contact-timeout` (previously unbounded — around two minutes at Linux defaults for a silently dropped SYN). Concurrent forwards coalesce on a per-peer dial lock instead of racing parallel dials past the pool's capacity bound.
- A rejection whose write never reached the peer (writer already closed) now terminates the session instead of being reported as success; unknown message types are rejected-and-tolerated in every session state rather than being fatal only when idle; and desync rejections use the `Unexpected` reason (the message type is known), with a dispatch-dead session terminating as `ResourceExhaustion` rather than `Unknown`.
- An out-of-order `XFER_SEGMENT` with the START flag now also drops the in-progress reassembly buffer, which could otherwise splice two transfers into one dispatched bundle.
- A failed or panicked ingest task (BPA dispatch failure) now cancels its session promptly instead of stalling until the next inbound segment; dispatch failures log at `warn!` and an ingest panic at `error!`.
- The writer closes on any transport write failure (previously only `Feed` failures closed it) and remains cancellable under sustained feeds: every socket write and flush is raced against the cancellation token, so shutdown cannot hang behind a peer that stops reading.
- The connection pool is bounded at its documented `max-idle-connections` capacity (previously effectively one over), and `max-idle-connections = 0` no longer retains an idle connection.
- Oversized inbound transfers are refused with `XFER_REFUSE (Not Acceptable)` and their remaining segments swallowed, instead of `MSG_REJECT` followed by a rejection per segment.
- Session termination follows RFC 9174 Section 6.1: no new outgoing transfers start once the session is Ending (queued forwards return to the pool for retry on another session), and a crossing `SESS_TERM` is not acknowledged with a second one.
- Peer-supplied `segment_mru` and configured `transfer_mru` are clamped rather than truncated on 32-bit targets, where truncation to a zero segment MTU turned the segmentation loop into an infinite empty-segment spin.
- The connection pool now dials a new connection when every pooled session is busy and the pool is under capacity, matching the documented pooling strategy — previously a forward queued behind a busy session and parallel connections were only established when none existed at all. If the peer cannot accept a new connection, the forward falls back to queueing on a busy session, preserving delivery to peers with asymmetric reachability.

## [0.4.0]

### Changed
- **BREAKING:** raised the `hardy-bpa`/`hardy-bpv7`/`hardy-async` requirements to their incompatible 0.2/0.6/0.2 releases. `Cla` implements `hardy_bpa::cla::Cla`, so consumers must move to `hardy-bpa` 0.2 in lockstep.
- Raised the minimum supported Rust version (MSRV) to 1.95.

### Fixed
- Bound the connection-pool forward retry loop so a flapping peer (sessions that accept then fail while the pool stays above `max_idle`) can no longer wedge a forward indefinitely.
- Use pointer identity (`Arc::ptr_eq`) when removing an emptied pool, so a concurrently re-created pool for the same peer is not erroneously dropped.

Releases before this version predate this changelog; see the git history for details.
