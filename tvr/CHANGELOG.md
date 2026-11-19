# Changelog

All notable changes to `hardy-tvr` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed
- Contact-plan `duration` and `delay` values are parsed with `jiff`, replacing the unmaintained `humantime`. Days and weeks count as 24-hour multiples; months and years, which have no fixed length, are refused.

## [0.2.0]

### Changed
- Use the shared `hardy-async` file watcher and decouple the watch config from the runtime watch mode.
- Track the `hardy-bpa` routing module restructure (dedicated routing table + fine-grained route actions).
- Raised all internal `hardy-*` dependency requirements to the v0.2.0 release line.
- Raised the minimum supported Rust version (MSRV) to 1.95.

Releases before this version predate this changelog; see the git history for details.
