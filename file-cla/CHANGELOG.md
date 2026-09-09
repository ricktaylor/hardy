# Changelog

All notable changes to `hardy-file-cla` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this crate adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed
- A file larger than the size cap negotiated at registration (when one exists, via the new `Cla::on_register` cap) is skipped with a warning instead of being read and offered to a certain rejection; the file is the operator's to clean up.
- Adapted to the `hardy-bpa` deferred transfer-outcome CLA contract (new `Cla::forward` signature). Behaviour is unchanged: forwards remain terminal.

### Fixed
- Files already sitting in the outbox when the CLA starts are dispatched instead of ignored until something touches them again. Filesystem notifications only cover files created after the watch is installed, so a bundle queued while the CLA was down (removable media, a restart) stayed there indefinitely. The watcher now scans the outbox once, after installing the watch so nothing arriving mid-scan is missed, and resolves symlinks so the scan and the notification path agree about the same directory entry.

## [0.2.0]

### Changed
- **BREAKING:** raised the `hardy-bpa`/`hardy-bpv7`/`hardy-async` requirements to their incompatible releases. `Cla` implements `hardy_bpa::cla::Cla`, so consumers must move to `hardy-bpa` 0.2 in lockstep.
- Raised the minimum supported Rust version (MSRV) to 1.95.

### Fixed
- Map invalid-bundle ingress failures to `cla::Error::Internal` explicitly instead of relying on a blanket conversion.

Releases before this version predate this changelog; see the git history for details.
