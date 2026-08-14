# Changelog

All notable changes to `hardy-tcpclv4-server` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed
- **BREAKING**: the TCPCLv4 config-file schema is owned by this crate (previously deserialized through `hardy-tcpclv4`'s serde types) and mapped onto the library's `Tcpclv4::builder()`; the library's example configuration files (`example_config.toml`/`.yaml`) now live here, and the `HARDY_TCPCLV4_*` environment overrides continue to apply per key (a `listeners` override is not expressible through the environment, as the removed `address` override was). Absent keys defer to the library's defaults instead of restating them. The `address` key is replaced by `listeners`, a list with one entry per listening element: absent listens on the IANA-registered `[::]:4556`, and an empty list is the dial-only spelling that `address: null` used to be. An explicit `keepalive-interval: null` is refused at parse with the replacement named (`0` disables keepalives). Out-of-range values are startup errors instead of warnings: `contact-timeout` must be 1 to 60 seconds (RFC 9174 Section 4.2), and `segment-mru`, `transfer-mru`, and `connection-rate-limit` must be greater than zero.
- **BREAKING**: unknown config keys are refused at parse with the known keys listed (previously they were silently ignored), so the removed `address` key, or a typo, cannot quietly leave a default in force: an old `address: "127.0.0.1:4556"` config fails loudly instead of binding the default wildcard listener. The same strictness applies to the environment: a stray `HARDY_TCPCLV4_*` variable is a startup error, while `HARDY_TCPCLV4_CONFIG_FILE` remains the loader's own interface and is never treated as a schema key.
- **BREAKING**: the `tls` section is restructured. `required` replaces the top-level `require-tls`, so requiring TLS without configuring it cannot be written. The certificate and key move into an `identity` object (`identity.cert-file`/`identity.key-file`; a lone half is a parse error, and `private-key-file` remains an alias for `key-file`). The new `client-auth` key (`off` | `optional` | `required`) enables mutual TLS for inbound connections. The trust anchor is mandatory and spelled directly in the section: `ca-certs`, or the deliberately loud `insecure-skip-verify` (replacing the `debug.accept-self-signed` flag), which overrides `ca-certs` with a startup warning naming both keys rather than silently ignoring it, so a debug session is one line to flip. The TLS rules are judged by the library, with errors in the config's own vocabulary.

## [0.2.0]

### Added
- Peer configuration support — statically configured TCPCLv4 peers.

### Changed
- Raised the `hardy-tcpclv4`/`hardy-bpa`/`hardy-proto`/`hardy-async` dependency requirements to the v0.2.0 release line.
- Raised the minimum supported Rust version (MSRV) to 1.95.

Releases before this version predate this changelog; see the git history for details.
