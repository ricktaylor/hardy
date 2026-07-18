# Changelog

All notable changes to `hardy-tcpclv4` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this crate adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed
- Bundle dispatch to the BPA moved off the session reader loop into a per-session ordered ingest task: segments, acknowledgments of outbound transfers, and keepalives keep flowing while a received bundle is being stored. The final `XFER_ACK` of a transfer is still only sent after dispatch completes, and acknowledgments are emitted in segment-arrival order.
- Outbound segments are queued to the writer task without a per-segment completion round-trip, and the writer flushes when its command queue runs dry. The session never awaits a blocked write without concurrently processing inbound messages, removing a mutual-stall risk when both peers send large transfers simultaneously.

### Fixed
- A mid-transfer `XFER_REFUSE` now clears all outstanding acknowledgment expectations for the refused transfer (RFC 9174 Section 5.2.2 sends no further `XFER_ACK` messages for it), where previously stale expectations desynchronised the acknowledgment matcher and tore down the session.
- The connection pool now dials a new connection when every pooled session is busy and the pool is under capacity, matching the documented pooling strategy — previously a forward queued behind a busy session and parallel connections were only established when none existed at all. If the peer cannot accept a new connection, the forward falls back to queueing on a busy session, preserving delivery to peers with asymmetric reachability.

## [0.4.0]

### Changed
- **BREAKING:** raised the `hardy-bpa`/`hardy-bpv7`/`hardy-async` requirements to their incompatible 0.2/0.6/0.2 releases. `Cla` implements `hardy_bpa::cla::Cla`, so consumers must move to `hardy-bpa` 0.2 in lockstep.
- Raised the minimum supported Rust version (MSRV) to 1.95.

### Fixed
- Bound the connection-pool forward retry loop so a flapping peer (sessions that accept then fail while the pool stays above `max_idle`) can no longer wedge a forward indefinitely.
- Use pointer identity (`Arc::ptr_eq`) when removing an emptied pool, so a concurrently re-created pool for the same peer is not erroneously dropped.

Releases before this version predate this changelog; see the git history for details.
