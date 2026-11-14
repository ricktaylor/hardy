# Changelog

All notable changes to `hardy-otel` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this crate adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- `LogLevel`, the `log-level` setting every Hardy binary reads: wraps `tracing::Level`, defaults to `info`, parses `error`/`warn`/`info`/`debug`/`trace` in any case or `1`-`5` (`ParseLogLevelError` otherwise), and displays as the lowercase name. The new `serde` feature adds `Serialize`/`Deserialize`, so a config struct needs only `#[serde(default)]`.
- `init` and `OtelGuard` exist without the OpenTelemetry stack: built without `otlp`, `init` installs a console `tracing` subscriber at the given level (`RUST_LOG` overriding it), and the guard holds nothing. A binary calls `init` the same way in both builds.

### Changed
- **BREAKING:** the OpenTelemetry exporters and the `metrics` recorder are behind the new `otlp` feature, off by default; enable it for the behaviour of 0.2's `init`.
- **BREAKING:** `init` takes its level as a `LogLevel` instead of a `tracing::Level` (`Level::INFO.into()` converts).

## [0.2.0]

### Changed
- Raised the minimum supported Rust version (MSRV) from 1.87 to 1.95 — the reason this release is a minor rather than a patch bump.

### Fixed
- Suppress OTEL flush/shutdown warning logs when no OTLP exporter endpoint is configured, eliminating spurious warnings in deployments that run without OpenTelemetry.

Releases before this version predate this changelog; see the git history for details.
