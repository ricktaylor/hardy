# Changelog

All notable changes to `hardy-bibe` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this crate adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `Error::Refused` — the BPA refused the encapsulating (outer) bundle at dispatch; surfaced from the forward path as a per-bundle failure.

### Changed

- An encapsulated outer bundle that exceeds the size cap negotiated at registration is rejected deterministically at `forward` with `PayloadTooLarge` (a per-bundle error, so the peer queue is not swept), instead of being dispatched into a certain rejection. When no cap was negotiated, no pre-check applies.
- Adapted to the `hardy-bpa` `Acceptance` verdict contract, answering per bundle rather than per link. Forwarding: an outer bundle the BPA refuses fails that bundle alone (`Error::Refused`) instead of signalling `NoNeighbour`, which would sweep the whole peer queue back to `Waiting`; a disconnected sink remains the one link-scoped outcome. Receiving: a decapsulated inner bundle the BPA refuses is deterministic for those bytes (the inner cannot outgrow the outer), so the outer bundle is consumed rather than parked for a retry that could never succeed; a transient dispatch fault still parks the outer for retry.

Releases before this version predate this changelog; see the git history for details.
