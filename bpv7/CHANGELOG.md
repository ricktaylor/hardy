# Changelog

All notable changes to `hardy-bpv7` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this crate adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- `bpsec::DecryptingReader`: a `reader::Reader` that decrypts BCB-covered blocks on demand and memoises each block's outcome — plaintext, no-key, or decrypt-failure — for the reader's lifetime, so a chain of consumers sharing one reader costs one decrypt attempt per covered block. The trait impl lends (cache borrows, zeroized when the reader drops); the inherent `block_data()` gives (owned plaintext, typed errors carrying the diagnostic cause, and `Ok(None)` for non-resident extents), preserving the free `bpsec::block_data` contract.

- `reader::ReaderExt`, blanket-implemented for every `Reader` (trait objects included): `extract<T>()` CBOR-decodes a block's payload, with `Ok(None)` for absent-or-unavailable and `Err` only for decode failures.

### Removed
- **BREAKING:** the free `bpsec::block_data()` function — `bpsec::DecryptingReader::block_data` is the same contract (typed errors carrying the cause; owned decrypted plaintext) plus outcome memoisation and an explicit `Ok(None)` for non-resident extents.

### Changed
- **BREAKING:** the block read abstraction is renamed and lifted to the crate root, joining the `Builder`/`Editor`/`Signer`/`Encryptor` role-noun family: trait `bpsec::BlockSet` is now `reader::Reader`, and `bpsec::PlainBlockSet` is now `reader::PlainReader`. No behavioural change — signatures and semantics are otherwise identical, and plain block reading no longer requires the `bpsec` module path.
- **BREAKING:** `reader::Reader::block` reports the payload slot as a four-state `reader::Availability` instead of `Option<Payload>`: `Available(Payload)`, `NotResident` (extents beyond the resident bytes), `NoKey` (BCB-covered, no usable key), and `NotDecryptable` (BCB-covered, decryption attempted and failed). A present block's unavailable payload no longer conflates "not held in memory" with "not decryptable by this node", so a caller can respond to each state differently; callers to whom every unavailable state is equivalent use `Availability::available()`.

- `creation_timestamp::CreationTimestamp::now` issues strictly monotonic `(time, sequence)` pairs per process, from one atomic: with a well-behaved clock the time component is truly the current UTC time, bundles created in the same millisecond take ascending sequence numbers, and a wall clock that steps backwards never re-issues an earlier pair — so ids built from `now()` are unique by construction within a process and callers need no collision checks. Previously the sequence number was derived from the clock's nanoseconds alone, so two reads landing on the same instant (coarse clocks, concurrent issuers) could collide. Uniqueness across process restarts rests on the wall clock moving forward (RFC 9171 §4.2.7); each fresh millisecond's sequence numbers are seeded from the sub-millisecond nanoseconds, so even a restart across a backward clock step is overwhelmingly unlikely to re-issue a pre-restart pair. Every production minting site benefits without per-site collision handling.
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
