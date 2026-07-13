# Changelog

All notable changes to `hardy-bpa-server` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0]

### Added
- BPSec key configuration: EID-pattern key bindings with role-gated key release.
- `service-priority` is now configurable through the config file.

### Changed
- Default to persistent storage (SQLite metadata + localdisk bundles) instead of in-memory.
- Routing: dedicated routing table with fine-grained route actions, and a restructured routing module layout.
- Explicit gRPC server lifecycle (build then serve) via `hardy-proto`'s `GrpcServer`.
- Use the shared `hardy-async` file watcher; reorganised static-routes handling; flattened the module structure (build moved to `main`, config split out).
- Track the `hardy-bpa` `filters` → `filter` module rename.
- Raised all internal `hardy-*` dependency requirements to the v0.2.0 release line.
- Raised the minimum supported Rust version (MSRV) to 1.95.

### Fixed
- Surface route-validation errors through `Result` from the RIB.
- Refuse to start when the configured default storage backend is compiled out.

Releases before this version predate this changelog; see the git history for details.
