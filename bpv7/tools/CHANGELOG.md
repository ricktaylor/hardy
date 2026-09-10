# Changelog

All notable changes to `hardy-bpv7-tools` (the `bundle` CLI) are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Removed
- `add-block` no longer accepts `block-integrity` (`bib`) or `block-security` (`bcb`) as `--type` values, and its help no longer advertises them: with every editor door refusing the reserved wire codes, no `add-block` invocation can craft a BIB/BCB — a numeric `--type 11`/`12` now surfaces the editor's typed `SecurityBlock` refusal. Properly-formed security blocks come from the signing and encryption commands.

## [0.2.0]

### Added
- `compare` subcommand: check two bundles for semantic equivalence, backed by the new `hardy_bpv7::cmp` module.

### Changed
- Raised all internal `hardy-*` dependency requirements to the v0.2.0 release line.
- Raised the minimum supported Rust version (MSRV) to 1.95.

Releases before this version predate this changelog; see the git history for details.
