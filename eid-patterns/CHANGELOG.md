# Changelog

All notable changes to `hardy-eid-patterns` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this crate adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- `hardy_eid_patterns::Result<T>`, the crate's `Result` alias over its `Error` enum.
- `EidPattern::into_atoms()`: decomposes a multi-item union into one single-item pattern per member (any other pattern yields itself), so a route table can key each member by its own specificity.

### Changed
- **BREAKING:** `EidPattern` is opaque. The `Any`/`Set` variants and the `EidPatternItem` type are no longer public: patterns are built by parsing or by the `From` conversions (both produce canonical values by construction) and consumed through the behavior API (`matches`, `is_subset`, `specificity_score`, `expand_local_node`, `into_atoms`, `Display`, the `Eid` conversions). Code that matched on `EidPattern::Any`/`Set` or named `EidPatternItem` must move to those methods; `Debug` output is now the pattern's text form.
- **BREAKING:** pattern-to-EID conversion is canonical. `Eid::try_from(EidPattern)` and the conversions built on it now yield the same EID values `hardy-bpv7`'s text parser produces: `ipn:0.0.0` converts to `Eid::Null` (was an `Eid::Ipn` with all-zero components), `ipn:0.4294967295.s` / `ipn:!.s` converts to `Eid::LocalNode(s)`, and `ipn:0.0.s` with `s != 0` denotes no valid EID and fails with `Error::NotExact` (was an invalid `Eid::Ipn`). A multi-item pattern set whose items all denote the same single EID now converts successfully instead of failing with `NotExact`, so `Eid::try_from(EidPattern::from(Eid::Null))` round-trips for the first time.
- A single-value ipn range is the same pattern as the bare number: `ipn:0.3.[5]` and `ipn:0.3.[5,5]` now parse to the value `ipn:0.3.5` parses to, so `Eq`, `Hash`, and `is_subset` agree across the spellings (previously the displayed text of a `[5]` pattern reparsed to a non-equal value).

## [0.4.0]

### Added
- `EidPattern::expand_local_node(&self, &IpnNodeId) -> Option<EidPattern>`: replaces the `ipn:!.*` LocalNode sentinel with a concrete `IpnNodeId`, returning `None` when no LocalNode item is present.

### Changed
- **BREAKING:** raised the `hardy-bpv7` requirement to the incompatible 0.6 release. `EidPattern::matches` takes `&hardy_bpv7::Eid`, so consumers must move to `hardy-bpv7` 0.6 in lockstep.
- Raised the minimum supported Rust version (MSRV) to 1.95.

Releases before this version predate this changelog; see the git history for details.
