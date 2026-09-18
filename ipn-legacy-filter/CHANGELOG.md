# Changelog

All notable changes to `hardy-ipn-legacy-filter` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this crate adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Deprecated

- The filter is inert against the current `hardy-bpa`: nothing populates the `BundleMetadata::next_hop` field it reads, so every bundle passes through unchanged. Configure the BPA's built-in per-hop re-encode via `BpaBuilder::ipn_legacy_peers` instead (`hardy-bpa-server` reads the same `ipn-legacy-nodes` config key it always did). The crate is retired by the filter subsystem rework.

## [0.2.0]

### Changed
- **BREAKING:** raised the `hardy-bpa`/`hardy-bpv7`/`hardy-eid-patterns` requirements to their incompatible releases. `IpnLegacyFilter::new` takes `Vec<hardy_eid_patterns::EidPattern>`, so consumers must move to the new core versions in lockstep.
- Adopted the chunked zero-copy `Editor` output (`Chunk::flatten`) and tracked the `hardy_bpa` filter API rename (`filters` → `filter`, `RewriteResult` → `WriteResult`). The crate's own `Config`/`new` signatures are otherwise unchanged.
- Raised the minimum supported Rust version (MSRV) to 1.95.

Releases before this version predate this changelog; see the git history for details.
