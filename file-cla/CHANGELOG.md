# Changelog

All notable changes to `hardy-file-cla` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this crate adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed
- Adapted to the `hardy-bpa` deferred transfer-outcome CLA contract (new `Cla::forward` signature). Behaviour is unchanged: forwards remain terminal.

### Fixed
- An outbox file is consumed only when the BPA accepts the bundle (`Acceptance::Accepted`): a refused or failed dispatch now leaves the file in place for a later scan, where previously it was deleted regardless — destroying the bundle on a transient failure. A file larger than the BPA's `max_bundle_size` (learned at registration) is skipped with a warning instead of being read and offered to a certain refusal; the file is the operator's to clean up.

## [0.2.0]

### Changed
- **BREAKING:** raised the `hardy-bpa`/`hardy-bpv7`/`hardy-async` requirements to their incompatible releases. `Cla` implements `hardy_bpa::cla::Cla`, so consumers must move to `hardy-bpa` 0.2 in lockstep.
- Raised the minimum supported Rust version (MSRV) to 1.95.

### Fixed
- Map invalid-bundle ingress failures to `cla::Error::Internal` explicitly instead of relying on a blanket conversion.

Releases before this version predate this changelog; see the git history for details.
