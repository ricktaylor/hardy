# Changelog

All notable changes to `hardy-file-cla` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this crate adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed
- Adapted to the `hardy-bpa` deferred transfer-outcome CLA contract (new `Cla::forward` signature). Behaviour is unchanged: forwards remain terminal.

### Fixed
- Files already sitting in the outbox when the CLA starts are dispatched instead of ignored until something touches them again. Filesystem notifications only cover files created after the watch is installed, so a bundle queued while the CLA was down (removable media, a restart) stayed there indefinitely. The watcher now scans the outbox once, after installing the watch so nothing arriving mid-scan is missed, and resolves symlinks so the scan and the notification path agree about the same directory entry.
- An outbox file is consumed only when the BPA accepts the bundle (`Acceptance::Accepted`): a refused or failed dispatch now leaves the file in place for a later scan, where previously it was deleted regardless — destroying the bundle on a transient failure.
- The "later scan" above is the startup scan of the previous bullet, and dispositions now match their failure class: a refusal is deterministic (re-offering the same bytes would refuse forever), so the file is quarantined to the `outbox/refused/` subdirectory (outside the non-recursive watch and scan) for the operator to inspect or move back to retry, while a dispatch failure is transient, so the file stays in place for the next startup scan to re-offer. The watcher also matches rename-into events, so the atomic write-then-rename spool idiom triggers dispatch.
- A rename the debouncer pairs up within the outbox — the write-then-rename idiom when the temporary file outlived the debounce window — arrives as a single both-ends event and is now offered (destination path only); previously it matched nothing and the file sat unoffered until the next startup scan.

## [0.2.0]

### Changed
- **BREAKING:** raised the `hardy-bpa`/`hardy-bpv7`/`hardy-async` requirements to their incompatible releases. `Cla` implements `hardy_bpa::cla::Cla`, so consumers must move to `hardy-bpa` 0.2 in lockstep.
- Raised the minimum supported Rust version (MSRV) to 1.95.

### Fixed
- Map invalid-bundle ingress failures to `cla::Error::Internal` explicitly instead of relying on a blanket conversion.

Releases before this version predate this changelog; see the git history for details.
