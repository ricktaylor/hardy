# Changelog

All notable changes to `hardy-file-cla` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this crate adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0]

### Changed
- **BREAKING:** raised the `hardy-bpa`/`hardy-bpv7`/`hardy-async` requirements to their incompatible releases. `Cla` implements `hardy_bpa::cla::Cla`, so consumers must move to `hardy-bpa` 0.2 in lockstep.

### Fixed
- Map invalid-bundle ingress failures to `cla::Error::Internal` explicitly instead of relying on a blanket conversion.

Releases before this version predate this changelog; see the git history for details.
