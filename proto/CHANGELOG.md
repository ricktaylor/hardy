# Changelog

All notable changes to `hardy-proto` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this crate adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0]

### Added
- Public `MAX_MESSAGE_SIZE` (16 MiB) and `MAX_PAYLOAD_SIZE` constants bounding gRPC message and payload sizes; sinks pre-check payload size against `MAX_PAYLOAD_SIZE` before sending.

### Changed
- **BREAKING:** replaced the `server::init()` free function with a `GrpcServer` struct — `GrpcServer::new()` builds it, `GrpcServer::serve(cancel)` returns a future the caller spawns/awaits — giving callers explicit control of the serve lifecycle.
- **BREAKING:** tracked the upstream `hardy_bpa::routes` → `hardy_bpa::routing` rename (`RemoteBpa`'s `BpaRegistration` impl, route action/error/sink types).

### Fixed
- Map routing validation errors to appropriate gRPC status codes (`invalid_argument` for null/own-node next hops, `unavailable` for disconnects, `internal` otherwise) instead of always surfacing as internal errors.
- Pre-check payload size before sending so an over-sized bundle returns a typed error instead of breaking the underlying gRPC stream.

### Removed
- `server::init()` (superseded by `GrpcServer`).

Releases before this version predate this changelog; see the git history for details.
