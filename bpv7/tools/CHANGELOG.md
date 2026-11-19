# Changelog

All notable changes to `hardy-bpv7-tools` (the `bundle` CLI) are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed
- `create --lifetime` and `update-primary --lifetime` are parsed with `jiff`, replacing the unmaintained `humantime`. Days and weeks count as 24-hour multiples; months and years, which have no fixed length, are refused (`--lifetime 1year` is now an error; write `365d`). `inspect` prints the lifetime and bundle age as whole days plus the rest balanced up to hours (`1d`, `30d`, `2d 16m 40s`), not humantime's `1day`, `30days` or `1month`.

### Fixed
- `inspect` lists a non-fragment bundle's primary flags: every flag after "Is a fragment", and the report-to note, sat inside the fragment check, so only a fragment's flags were printed. The note that status time is requested without reports now appears only when the bundle requests no status report at all, not whenever one of the four is unrequested.

### Removed
- `add-block` no longer accepts `block-integrity` (`bib`) or `block-security` (`bcb`) as `--type` values, and its help no longer advertises them: with every editor door refusing the reserved wire codes, no `add-block` invocation can craft a BIB/BCB — a numeric `--type 11`/`12` now surfaces the editor's typed `SecurityBlock` refusal. Properly-formed security blocks come from the signing and encryption commands.

## [0.2.0]

### Added
- `compare` subcommand: check two bundles for semantic equivalence, backed by the new `hardy_bpv7::cmp` module.

### Changed
- Raised all internal `hardy-*` dependency requirements to the v0.2.0 release line.
- Raised the minimum supported Rust version (MSRV) to 1.95.

Releases before this version predate this changelog; see the git history for details.
