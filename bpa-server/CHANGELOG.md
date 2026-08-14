# Changelog

All notable changes to `hardy-bpa-server` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed
- Command-line parsing uses clap: the version flag is `-V`/`--version`, and lowercase `-v` no longer prints the version.
- `status-reports`, `poll-channel-depth`, `processing-pool-size`, `storage.lru-capacity`, `storage.max-cached-bundle-size`, the `type: "memory"` backend knobs (`max-bundles`, `capacity`, `min-bundles`), the `rfc9171-validity` checks, and the `static-routes` keys (`routes-file`, `priority`, `protocol-id`) are optional: absent keys defer to the library's own defaults (previously restated by this crate) instead of being written into the loaded configuration. The storage backend schemas (`memory`, `sqlite`, `postgres`, `localdisk`, `s3`) and the `rfc9171-validity` schema are owned by this crate (previously deserialized through the storage crates' and `hardy-bpa`'s types); the keys are unchanged (except the `postgres` pool timings, next bullet), and an `s3` entry without a `bucket` is now a parse error instead of a startup error, a `postgres` entry with `max-connections: 0` and a memory bundle entry with `min-bundles: 0` are parse errors, and an `s3` `multipart-threshold` below the part size is a startup error (previously an unenforced doc claim), while one above the S3 5 GiB `PutObject` limit, or a `multipart-part-size` outside the S3 part bounds, is a parse error.
- **BREAKING**: the `postgres` pool timings are the duration keys `connect-timeout`, `idle-timeout`, and `max-lifetime`, written as humantime strings (e.g. `30s`, `10m`, `1h 30m`) and required to be greater than zero, replacing the unit-suffixed integer keys `connect-timeout-secs`, `idle-timeout-mins`, and `max-lifetime-mins`.
- **BREAKING**: the schema of `type: "tcpclv4"` entries in the `clas` list is owned by this crate (previously deserialized through `hardy-tcpclv4`'s serde types) and mapped onto the library's `Tcpclv4::builder()`. Absent keys defer to the library's defaults instead of restating them. The `address` key is replaced by `listeners`, a list with one entry per listening element: absent listens on the IANA-registered `[::]:4556`, and an empty list is the dial-only spelling that `address: null` used to be. An explicit `keepalive-interval: null` is refused at parse with the replacement named (`0` disables keepalives). Out-of-range values are startup errors instead of warnings: `contact-timeout` must be 1 to 60 seconds (RFC 9174 Section 4.2), and `segment-mru`, `transfer-mru`, and `connection-rate-limit` must be greater than zero.
- **BREAKING**: unknown keys in a `type: "tcpclv4"` CLA entry and its `tls` block are refused at parse with the known keys listed (previously they were silently ignored), so the removed `address` key, or a typo, cannot quietly leave the default wildcard listener in force. A malformed entry of a known CLA type is now a startup error: previously its parse error was swallowed by the unknown-CLA-type fallback and the entry was ignored with a warning, which also silently defeated the `keepalive-interval: null` refusal. Entries of genuinely unknown CLA types are still tolerated and ignored with a warning.
- **BREAKING**: the whole config-file schema now refuses unknown keys with the known keys listed, extending the tcpclv4-entry strictness to every section: the top level, the storage backends and cache knobs, the `grpc` block, `bpsec` and its bindings, `rfc9171-validity`, `static-routes`, and `built-in-services`. A section this build was compiled without (e.g. `grpc` in a build without the feature) is now a parse error instead of being silently ignored. Entries of unknown CLA types and unknown `policies` types remain the deliberate extension points and are still tolerated. The same strictness applies to the environment: a stray `HARDY_BPA_SERVER_*` variable is a startup error, while `HARDY_BPA_SERVER_CONFIG_FILE` remains the loader's own interface and is never treated as a schema key.
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
