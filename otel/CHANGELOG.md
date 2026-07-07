# Changelog

All notable changes to `hardy-otel` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this crate adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0]

### Changed
- Raised the minimum supported Rust version (MSRV) from 1.87 to 1.95 — the reason this release is a minor rather than a patch bump.

### Fixed
- Suppress OTEL flush/shutdown warning logs when no OTLP exporter endpoint is configured, eliminating spurious warnings in deployments that run without OpenTelemetry.

Releases before this version predate this changelog; see the git history for details.
