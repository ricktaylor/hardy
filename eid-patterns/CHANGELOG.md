# Changelog

All notable changes to `hardy-eid-patterns` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this crate adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.0]

### Added
- `EidPattern::expand_local_node(&self, &IpnNodeId) -> Option<EidPattern>`: replaces the `ipn:!.*` LocalNode sentinel with a concrete `IpnNodeId`, returning `None` when no LocalNode item is present.

### Changed
- **BREAKING:** raised the `hardy-bpv7` requirement to the incompatible 0.6 release. `EidPattern::matches` takes `&hardy_bpv7::Eid`, so consumers must move to `hardy-bpv7` 0.6 in lockstep.
- Raised the minimum supported Rust version (MSRV) to 1.95.

Releases before this version predate this changelog; see the git history for details.
