# Changelog

All notable changes to `hardy-tcpclv4` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this crate adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.0]

### Changed
- **BREAKING:** raised the `hardy-bpa`/`hardy-bpv7`/`hardy-async` requirements to their incompatible 0.2/0.6/0.2 releases. `Cla` implements `hardy_bpa::cla::Cla`, so consumers must move to `hardy-bpa` 0.2 in lockstep.

### Fixed
- Bound the connection-pool forward retry loop so a flapping peer (sessions that accept then fail while the pool stays above `max_idle`) can no longer wedge a forward indefinitely.
- Use pointer identity (`Arc::ptr_eq`) when removing an emptied pool, so a concurrently re-created pool for the same peer is not erroneously dropped.

Releases before this version predate this changelog; see the git history for details.
