# Changelog

All notable changes to `hardy-tvr` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed
- Contact-plan `duration` and `delay` values are parsed with `jiff`, replacing the unmaintained `humantime`. Days and weeks count as 24-hour multiples; months and years, which have no fixed length, are refused.
- The `otel` feature is on by default: the agent exports traces, metrics, and logs over OTLP when a collector is configured. Build with `--no-default-features` for console logging only.
- Logging is set up by `hardy-otel` in every build: `log-level` is parsed by its `LogLevel` (names in any case, or `1`-`5`), and the console-only subscriber used without the `otel` feature is its `init`. Configuration files are unchanged.

## [0.2.0]

### Changed
- Use the shared `hardy-async` file watcher and decouple the watch config from the runtime watch mode.
- Track the `hardy-bpa` routing module restructure (dedicated routing table + fine-grained route actions).
- Raised all internal `hardy-*` dependency requirements to the v0.2.0 release line.
- Raised the minimum supported Rust version (MSRV) to 1.95.

Releases before this version predate this changelog; see the git history for details.
