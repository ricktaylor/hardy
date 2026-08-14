# Changelog

All notable changes to `hardy-bpa-server` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed
- **BREAKING**: the schema of `type: "tcpclv4"` entries in the `clas` list is owned by this crate (previously deserialized through `hardy-tcpclv4`'s serde types) and mapped onto the library's `Tcpclv4::builder()`. Absent keys defer to the library's defaults instead of restating them. The `address` key is replaced by `listeners`, a list with one entry per listening element: absent listens on the IANA-registered `[::]:4556`, and an empty list is the dial-only spelling that `address: null` used to be. An explicit `keepalive-interval: null` is refused at parse with the replacement named (`0` disables keepalives). Out-of-range values are startup errors instead of warnings: `contact-timeout` must be 1 to 60 seconds (RFC 9174 Section 4.2), and `segment-mru`, `transfer-mru`, and `connection-rate-limit` must be greater than zero.
- **BREAKING**: unknown keys in a `type: "tcpclv4"` CLA entry and its `tls` block are refused at parse with the known keys listed (previously they were silently ignored), so the removed `address` key, or a typo, cannot quietly leave the default wildcard listener in force. A malformed entry of a known CLA type is now a startup error: previously its parse error was swallowed by the unknown-CLA-type fallback and the entry was ignored with a warning, which also silently defeated the `keepalive-interval: null` refusal. Entries of genuinely unknown CLA types are still tolerated and ignored with a warning.
- **BREAKING**: the `tls` block of a tcpclv4 CLA entry is restructured, in lockstep with `hardy-tcpclv4-server`. `required` replaces the entry-level `require-tls`, so requiring TLS without configuring it cannot be written. The certificate and key move into an `identity` object (`identity.cert-file`/`identity.key-file`; a lone half is a parse error, and `private-key-file` remains an alias for `key-file`). The new `client-auth` key (`off` | `optional` | `required`) enables mutual TLS for inbound connections. The trust anchor is mandatory and spelled directly in the block: `ca-certs`, or the deliberately loud `insecure-skip-verify` (replacing the `debug.accept-self-signed` flag), which overrides `ca-certs` with a startup warning naming both keys rather than silently ignoring it. The TLS rules are judged by the library, with errors in the config's own vocabulary.

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
