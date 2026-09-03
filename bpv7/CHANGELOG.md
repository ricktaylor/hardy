# Changelog

All notable changes to `hardy-bpv7` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this crate adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Incremental payload-BIB verification for streaming ingress: `bpsec::bib::Operation::begin_verify` returns a `bpsec::bib::Verifier` pre-fed with every header-resident IPPT part; the caller feeds the payload's block-type-specific data as it streams (`update`) and settles with `finish` (constant-time tag compare). The verifier owns everything it needs — including key material *copied* into the MAC state, the recorded exception to keeping raw keys out of async scopes — so it is `Send` and may cross `await` points and task boundaries. `checks::begin_payload_verification` begins one verifier per deferred payload BIB (`VerifyFacts::deferred_bibs`) from header material alone, mirroring the resident path's skip rules (BCB-covered payload, `NoKey`).

### Changed
- **BREAKING:** `checks::verify_payload` is removed. Callers holding deferred payload-BIB op-sets settle them incrementally via `checks::begin_payload_verification` + `bib::Verifier` — no resident payload buffer is required, which is the point.
- **BREAKING (behaviour):** non-canonical bundle framing is now a hard parse error (`Error::NotCanonical`). The parser no longer records non-shortest framing for later repair: `bundle validate` loses its "non-canonical but semantically valid" diagnostic, and `bundle rewrite` no longer repairs framing (its repair now covers PreviousNode/HopCount bodies only).
- **BREAKING:** new variants `Error::PossibleBpv6` and `Error::NotABundle(u8)` on the non-`#[non_exhaustive]` `Error` enum can break exhaustive `match` arms. The classification of data that cannot start a BPv7 bundle also changed: the first-byte gate now returns `PossibleBpv6` (CBOR unsigned integer 6, the opening byte of an RFC 5050 primary block), `NotCanonical` (definite-length outer array), or `NotABundle` carrying the offending first byte, where previous releases returned `InvalidCBOR(IncorrectType(..))`.
- CBOR tags at grammar positions that permit none — every scalar and structured field, and the block-array head — are now rejected from the first byte of the tag run, without reading it (decoded via the new `hardy-cbor` `Untagged` wrapper; its cbor-level `UnexpectedTag` is translated to each error domain's `NotCanonical`, so the observable classification is unchanged from previous releases, which read the entire run before rejecting). The one position permitting a tag (`#6.24` on block data) enforces the same fixed-byte bound by hand. This keeps ingress rejection of adversarial tag runs free of per-tag work and per-tag allocation (a scalar-field reject still boxes its one constant field-label error).

### Fixed
- A CBOR tag on the status flag of a status-report assertion was silently accepted — the bare `bool` decode folds tag presence into a canonical flag the caller discarded. It is now rejected (`InvalidField("status")` wrapping `NotCanonical`).

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
- Raised the minimum supported Rust version (MSRV) to 1.95.

### Fixed
- Accept fragment bundles with `offset == 0` and `total == 0`.
- Accept multi-target BCBs from other implementations; handle non-payload BCB decrypt failures per RFC 9172; narrow handling to the `DecryptionFailed` case.
- Use `core::cmp::Reverse` so `no_std` builds compile.
- `Builder::build` keys the returned `Bundle.blocks` map by wire block number (primary 0, payload 1, extensions 2+) instead of the extension enumeration index, which previously collided with the primary and payload entries.
- `Editor::flatten_inplace` handles mixed-direction edits (a block shrinking before an unchanged block while another grows after it) by assembling into a fresh buffer instead of an unsound single-direction in-place copy.
- **BREAKING (behaviour):** signing the primary block with a BIB now removes the primary's CRC before generating the IPPT (RFC 9173 §3.8.1), matching what conformant verifiers compute; a prior release signed the primary with its CRC still present, producing a non-interoperable signature. The CRC is retained when the primary is only IPPT scope context, not the BIB target.
- `Editor::remove_integrity` clears the target block's BIB coverage when its covering BIB is removed, so `rebuild_bundle()` no longer reports a dangling reference to a BIB that no longer exists.

Releases before this version predate this changelog; see the git history for details.
