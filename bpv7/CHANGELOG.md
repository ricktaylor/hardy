# Changelog

All notable changes to `hardy-bpv7` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this crate adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- `Result<T>` aliases over the owning `Error` at the crate root and in `eid`, `bpsec`, `crc`, `builder`, `editor`, and `status_report`, per the house error convention. Signatures across the crate now spell `Result<T>`; the editor/signer/encryptor recovery-tuple returns (`Result<T, (Self, Error)>`) deliberately stay explicit, because the `(Self, Error)` payload is the point of those signatures.

### Removed
- **BREAKING:** the nested `bundle::id` module, and `bundle::Id` is `bundle::BundleId` (the key-parse error follows as `bundle::BundleIdError`, was `bundle::id::Error`). The bare name is importable everywhere, ending the workspace's most-qualified path; the precedent is `std::thread::ThreadId`. The base64url key encoding (`from_key`/`to_key`) is unchanged, so persisted and wire-carried keys are unaffected.
- **BREAKING:** the top-level `primary_block` module. `PrimaryBlock` lives with the bundle data model as `bundle::PrimaryBlock`, beside `bundle::{Id, Flags, FragmentInfo}`.
- **BREAKING:** the top-level `block` module. The wire-block types live with the bundle data model: `bundle::{Block, BlockType, BlockFlags, Payload, BibCoverage}`. `block::Type` is `BlockType`, `block::Flags` is `BlockFlags`, and the bundle-level `bundle::Flags` is `BundleFlags`: the two flag types no longer share a name that only module qualification could tell apart.
- **BREAKING:** the `lifetime` module and its `Lifetime` newtype. It had no consumers: `PrimaryBlock::lifetime` is a `core::time::Duration` and no other code referenced the type.
- **BREAKING:** the `eid::IpnServiceNumber` and `eid::DtnServiceName` type aliases. They were transparent spellings of `u32` and `Box<str>`; the underlying types now appear directly in the `Service` and `Eid` signatures (same types, so only code naming the aliases breaks).
- **BREAKING:** `PrimaryBlock::as_block` is crate-internal. It fabricates the block-0 index entry for the parser and editor, which are its only callers.

### Fixed
- The parser's block-offset conversions no longer use bare `as usize` casts on wire-derived `u64` lengths: on a 32-bit target an offset beyond the address space (excluded today by the staged-buffer invariant) now fails loudly via `try_from` + a stated invariant instead of silently truncating. No behavior change on any supported configuration.
- A CBOR tag on the status flag of a status-report assertion was silently accepted — the bare `bool` decode folds tag presence into a canonical flag the caller discarded. It is now rejected (`InvalidField("status")` wrapping `NotCanonical`).

### Changed
- **BREAKING:** the BPSec security contexts group by context name: `bpsec::rfc9173` is `bpsec::context`, with `bpsec::context::{bib_hmac_sha2, bcb_aes_gcm}` now public modules, so the operation types carried inside `bib::Operation::HMAC_SHA2` and `bcb::Operation::AES_GCM` are nameable by consumers. The shared crypto primitives (IV, AES key wrap, MAC tag) move up to private `bpsec` modules where a future context can share them. The `rfc9173` cargo feature keeps its name: it accurately toggles the RFC 9173 default contexts.
- **BREAKING:** `bpsec::Context` is `bpsec::ContextId`: it is the RFC 9172 §3.4 wire context identifier, and the rename ends the collision with the signer/encryptor `Context` enums, which keep their names. `bpsec::parse` (private) is `bpsec::asb`, and `UnknownOperation` is re-exported as `bpsec::UnknownOperation`, so `Operation::Unrecognised` payloads are nameable. `Signer` and `Encryptor` are re-exported at `bpsec::{Signer, Encryptor}`.
- The AES key-wrap helpers return a typed error (invalid KEK length vs failed operation) instead of `Result<_, String>`, and RNG failure is the new deliberately detail-free `bpsec::Error::Rng` (was a stringified `Algorithm`); wrap failures still surface as `Error::Algorithm` until the planned bpsec error split.
- **BREAKING:** the `parse` module is `parser`, and the entry points move to the crate root: `hardy_bpv7::parse(bytes)` and `hardy_bpv7::Parsed` (were `parse::parse` / `parse::Parsed`). The streaming machinery stays module-qualified as `parser::{BundleParser, ParserProgress, PayloadTail}`. Internally the 1400-line module is split into `parser/{mod, block_header, payload_tail, bpsec_structure}.rs` with no behavior change.
- `creation_timestamp::CreationTimestamp::now` issues strictly monotonic `(time, sequence)` pairs per process, from one atomic: with a well-behaved clock the time component is truly the current UTC time, bundles created in the same millisecond take ascending sequence numbers, and a wall clock that steps backwards never re-issues an earlier pair — so ids built from `now()` are unique by construction within a process and callers need no collision checks. Previously the sequence number was derived from the clock's nanoseconds alone, so two reads landing on the same instant (coarse clocks, concurrent issuers) could collide. Uniqueness across process restarts rests on the wall clock moving forward (RFC 9171 §4.2.7); each fresh millisecond's sequence numbers are seeded from the sub-millisecond nanoseconds, so even a restart across a backward clock step is overwhelmingly unlikely to re-issue a pre-restart pair. Every production minting site benefits without per-site collision handling.
- **BREAKING (behaviour):** non-canonical bundle framing is now a hard parse error (`Error::NotCanonical`). The parser no longer records non-shortest framing for later repair: `bundle validate` loses its "non-canonical but semantically valid" diagnostic, and `bundle rewrite` no longer repairs framing (its repair now covers PreviousNode/HopCount bodies only).
- **BREAKING:** new variants `Error::PossibleBpv6` and `Error::NotABundle(u8)` on the non-`#[non_exhaustive]` `Error` enum can break exhaustive `match` arms. The classification of data that cannot start a BPv7 bundle also changed: the first-byte gate now returns `PossibleBpv6` (CBOR unsigned integer 6, the opening byte of an RFC 5050 primary block), `NotCanonical` (definite-length outer array), or `NotABundle` carrying the offending first byte, where previous releases returned `InvalidCBOR(IncorrectType(..))`.
- CBOR tags at grammar positions that permit none — every scalar and structured field, and the block-array head — are now rejected from the first byte of the tag run, without reading it (decoded via the new `hardy-cbor` `Untagged` wrapper; its cbor-level `UnexpectedTag` is translated to each error domain's `NotCanonical`, so the observable classification is unchanged from previous releases, which read the entire run before rejecting). The one position permitting a tag (`#6.24` on block data) enforces the same fixed-byte bound by hand. This keeps ingress rejection of adversarial tag runs free of per-tag work and per-tag allocation (a scalar-field reject still boxes its one constant field-label error).

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
