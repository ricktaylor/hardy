# Changelog

All notable changes to `hardy-cbor` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this crate adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- `decode::ItemType` and `decode::ItemKind` (both `Copy`): the wire-level classification of an item — tag status plus payload-free marker shape — with `From<&Head>`/`From<&Marker>` conversions. `Display` output matches the strings `Head`'s `Display` (unchanged) has always produced.
- `FromCbor` implementations for `String` and `Box<[u8]>` — the two owned-container decodes. Each copies by construction (the requested type announces the allocation), gathers indefinite-length chunks, and folds tags and indefinite encodings into the canonical flag; decoding otherwise remains zero-copy through the borrowed `Value` forms, whose docs now state that contract.

### Changed
- **BREAKING:** `decode::Error::IncorrectType` carries `(&'static str, ItemType)` instead of `(String, String)`, so constructing a type-mismatch error never heap-allocates; message formatting is deferred to `Display` and the rendered text is unchanged.
- **BREAKING:** `decode::Value::type_name(tagged) -> String` is replaced by `decode::Value::item_type(tagged) -> ItemType`.

## [2.0.0]

### Added
- `decode::skip_value(data, max_recursion)`: walk past a CBOR item directly on the byte slice with no allocation.
- Public `decode::Head` and `decode::Marker` (`Tags = SmallVec<[u64; 1]>`): a lightweight type-marker decode path for dispatching on CBOR type without a full parse, plus an optimized fast path.

### Changed
- **BREAKING:** `encode::BytesHeader` is now `BytesHeader(pub u64)` instead of `BytesHeader<'a, V>(pub &'a V) where V: AsRef<[u8]>` — pass the payload length directly instead of borrowing the bytes.
- `decode::Marker::Bytes`/`Marker::Text` carry the payload length as `Option<u64>` rather than a byte `Range`.
- Internal `decode` reorganised into `head`/`impls`/`series` submodules; tests moved out of `src/`.
- Raised the minimum supported Rust version (MSRV) to 1.95.

### Fixed
- `Option<T>` decode handling and assorted clippy lints; `Value::type_name` tidy-up (output unchanged).

Releases before this version predate this changelog; see the git history for details.
