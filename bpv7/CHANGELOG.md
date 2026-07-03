# Changelog

All notable changes to `hardy-bpv7` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this crate adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.6.0]

### Added
- `bundle_age` and `lifetime` modules with `BundleAge`/`Lifetime` newtypes that enforce canonical CBOR on the wire.
- `cmp` module with `compare_bundles()` for structural bundle diffing.
- `crc::Digest` push-mode digest (`new`/`cbor_head`/`push`/`push_zeros`/`verify`/`finalize`) avoiding heap allocation on the CRC path.
- `editor::Chunk` zero-copy output type and `Editor::rebuild_bundle()` returning the parsed `Bundle` alongside its chunks.
- `builder::BlockTemplate::build_to_vec()`, `eid::Eid::to_node_id()`, `block::Payload::{len, is_empty}`.
- `IpnNodeId: Copy`; `block::Type: PartialOrd + Ord`.

### Changed
- **BREAKING:** `FromCbor::Error` for `block::Flags`/`Type`, `bundle::Flags`, `crc::CrcType`, `bpsec::Context`, `dtn_time::DtnTime`, `status_report::ReasonCode` changed from `hardy_cbor::decode::Error` to the crate/`bpsec` error type (carrying `NotCanonical`).
- **BREAKING:** `Editor::rebuild()` now returns `Vec<editor::Chunk>` instead of `Box<[u8]>`; `RewrittenBundle::Rewritten.new_data` and `CheckedBundle.new_data` now hold `Vec<editor::Chunk>` instead of `Box<[u8]>`.
- **BREAKING:** new variants on public (non-`#[non_exhaustive]`) error enums — `Error::{InvalidHopLimit, NotCanonical}`, `editor::Error::SecurityBlock`, `eid::Error::NotCanonical`, `status_report::Error::NotCanonical` — can break exhaustive `match` arms.
- **BREAKING (behaviour):** scalar decoders now strictly enforce RFC 9171 canonical form, rejecting non-shortest encodings and hop limits outside `1..=255`; some bundles that previously parsed are now rejected.
- Bumped `aes-gcm` 0.10 → 0.11 (internal; BPSec AES-GCM adapted to the `AeadInOut`/`decrypt_inout_detached` API; behaviour unchanged).

### Fixed
- Accept fragment bundles with `offset == 0` and `total == 0`.
- Accept multi-target BCBs from other implementations; handle non-payload BCB decrypt failures per RFC 9172; narrow handling to the `DecryptionFailed` case.
- Use `core::cmp::Reverse` so `no_std` builds compile.

Releases before this version predate this changelog; see the git history for details.
