# bpv7 TODO

> Status (2026-09-11): The push parser has landed. `parser::parse` / `parser::BundleParser` is the only entry point, `checks` carries the keyed §A–§E pass, and the legacy whole-bundle `bundle/parse.rs` and `bundle/raw_parse.rs` are both gone. See [parser_design.md](parser_design.md) for the pipeline as it now stands. Streaming AES-GCM is still unstarted.

## Streaming AES-GCM for BPSec BCB

### Background

The current `bcb_aes_gcm.rs` uses `aes-gcm` v0.11 which — even with
its new `AeadInOut` in-place API — requires the entire
plaintext/ciphertext as a contiguous buffer. This blocks
streaming payload encryption/decryption in the Transformer pipeline
(see `bpa/docs/streaming_pipeline_design.md` §6.1.5, §7.6).

AES-GCM is internally AES-CTR + GHASH — both inherently streamable.
The low-level crates are already in Hardy's dependency tree as
transitive dependencies of `aes-gcm`:

- `ctr` v0.10.1 — `StreamCipher::apply_keystream(&mut chunk)`
- `ghash` v0.6.0 — `UniversalHash::update_padded(&data)` + `finalize()`
- `aes` v0.9.3 — `BlockEncrypt` for computing H and encrypting J0

### Design

Build a `StreamingAesGcm` wrapper that exposes push-based
encryption and decryption using the `ctr` and `ghash` crates
directly, replacing the `aes-gcm` crate's all-at-once API.

```rust
pub struct StreamingAesGcm<C: BlockEncrypt + BlockSizeUser> {
    ctr: Ctr32BE<C>,           // inc32 — GCM increments lower 32 bits only
    ghash: GHash,
    j0_encrypted: Block<C>,    // AES_K(J0) for final tag XOR
    aad_len: u64,
    data_len: u64,
}
```

#### Encryption API

```rust
impl<C> StreamingAesGcm<C> {
    /// Initialise from key and 12-byte IV.
    ///
    /// Computes:
    ///   H = AES_K(0^128)            — GHASH subkey
    ///   J0 = IV || 0x00000001       — pre-counter block (96-bit IV case)
    ///   AES_K(J0)                   — saved for final tag XOR
    ///   CTR starts at inc32(J0)     — i.e., IV || 0x00000002
    pub fn new(key: &[u8], iv: &[u8; 12]) -> Result<Self, Error>;

    /// Feed AAD incrementally. Must be called before any
    /// encrypt_update(). Can be called multiple times.
    pub fn aad_update(&mut self, aad: &[u8]);

    /// Encrypt a chunk of plaintext in place. Feeds the
    /// resulting ciphertext to GHASH. Can be called multiple
    /// times with arbitrary chunk sizes.
    pub fn encrypt_update(&mut self, data: &mut [u8]);

    /// Finalize encryption. Feeds the length block to GHASH,
    /// computes tag = GHASH_result XOR AES_K(J0).
    /// Returns the 16-byte authentication tag.
    pub fn encrypt_finalize(self) -> Tag;
}
```

#### Decryption API

```rust
impl<C> StreamingAesGcm<C> {
    /// Feed AAD incrementally (same as encryption).
    pub fn aad_update(&mut self, aad: &[u8]);

    /// Decrypt a chunk of ciphertext in place. Feeds the
    /// ciphertext to GHASH BEFORE decrypting (GCM authenticates
    /// ciphertext, not plaintext).
    pub fn decrypt_update(&mut self, data: &mut [u8]);

    /// Finalize decryption. Computes and verifies the tag.
    /// Returns Ok(()) if tag matches, Err if authentication
    /// fails. On failure, all decrypted data should be
    /// discarded (spool aborted).
    pub fn decrypt_finalize(self, expected_tag: &Tag) -> Result<(), Error>;
}
```

#### GCM Construction Detail

Per NIST SP 800-38D, for a 12-byte (96-bit) IV:

1. `H = AES_K(0^128)` — the GHASH subkey
2. `J0 = IV || 0x00000001` — the pre-counter block (96-bit IV
   concatenated with 32-bit counter initialised to 1)
3. CTR keystream starts at `inc32(J0)` = `IV || 0x00000002`.
   GCM's `inc32` increments only the rightmost 32 bits — use
   `Ctr32BE`, not `Ctr128BE`. (The `aes-gcm` crate uses
   `Ctr32BE` internally for the same reason.)
4. GHASH input:
   `AAD || pad_128(AAD) || ciphertext || pad_128(C) || len_bits(AAD) || len_bits(C)`
   where `len_bits` are 64-bit big-endian **bit** lengths and
   `pad_128` pads to a 128-bit block boundary with zeros.
5. `Tag = GHASH_result XOR AES_K(J0)`

The critical ordering for streaming decryption: feed ciphertext
to GHASH *before* decrypting with CTR. GCM authenticates
ciphertext, not plaintext.

### Implementation Plan

1. Add `ctr` and `ghash` as direct dependencies (currently
   transitive only). Pin compatible versions with existing
   `aes-gcm` to avoid duplication.

2. Implement `StreamingAesGcm` in a new module
   `bpv7/src/bpsec/context/streaming_aes_gcm.rs`, alongside the
   existing `bcb_aes_gcm` and `bib_hmac_sha2` context modules.

3. Add tests using the RFC 9173 Appendix A vectors (already driven
   from `bpv7/tests/rfc9173.rs`). Verify that streaming encryption
   produces identical output to the existing `aes-gcm` crate for the
   same inputs.

4. Wire into the confidentiality filter's Transformer:
   - `aad_update()` with scope-flag-constructed AAD (same as
     current `build_data()`)
   - `encrypt_update()` / `decrypt_update()` called per chunk
     as bytes flow through the Transformer
   - `encrypt_finalize()` / `decrypt_finalize()` on `None`

5. Retain the existing `aes-gcm` dependency behind a feature
   flag for header-block BCB (small blocks, all-at-once is
   fine). The streaming wrapper is for payload-block BCB.

6. Eventually remove `aes-gcm` dependency entirely once all
   paths use the streaming wrapper.

### Zeroization

The streaming wrapper must zeroize sensitive state on drop:
- CTR key material (via `ctr`'s internal zeroization if available,
  or manual `Zeroize` impl)
- GHASH key (H)
- AES_K(J0) block

Use `zeroize::Zeroize` derive or manual impl on `StreamingAesGcm`.
Decrypted output is the caller's responsibility (the Transformer
manages `Zeroizing<>` for decrypted payload buffers).

### Dependencies

Current (transitive via `aes-gcm` 0.11.1): `aes` 0.9.3, `ctr`
0.10.1, `ghash` 0.6.0, `cipher` 0.5.2.

To add as direct:
- `ctr = "0.10"` with features `["zeroize"]`
- `ghash = "0.6"` with features `["zeroize"]`

No new crate downloads: both are already resolved in the lock file.

### Phase

This is **Phase 3** work in the streaming pipeline design
(security gateway). Header-block BCB targets are small and
continue to use the existing all-at-once API until Phase 3.

## Bundle-offset `Range<u64>` consolidation (push-parser Milestone 2 remainder)

Milestone 1 of the push-parser migration is complete: `parser::parse` /
`parser::BundleParser` is the only entry point, the keyed pass lives in
`checks`, and the legacy `bundle/parse.rs` and `bundle/raw_parse.rs` are
both deleted. Milestone 2's decision (all bundle-byte offsets are
`Range<u64>`, for 32-bit target compatibility and to keep offsets in the
wire-stream domain rather than the address-space domain) is applied to
`bundle::Block::extent` and `Block::data` but not yet everywhere.

Remaining `Range<usize>` on bundle-byte offsets:

- `src/editor.rs:59` (`Chunk::Unchanged`) and `src/editor.rs:236`
  (the `unchanged` run accumulator).
- `src/bundle/primary_block.rs:232` (`as_block`'s `extent` argument).

Not in scope: the `Range<usize>` maps in `bpsec::asb` and the context
modules. Those index within an in-memory ASB body slice, not the wire
stream, so `usize` is the correct domain there.

## Bundle reshape (push-parser Milestone 3, deferred)

End state: `bundle::Bundle` is a pure wire-format representation
(`{ primary: PrimaryBlock, blocks: HashMap<u64, Block> }`). Per-hop
processing state (bundle age, ingress timestamps, dispatch attempts,
BIB/BCB coverage) moves to a BPA-side `BundleMetadata` keyed by bundle
id. Massive blast radius across BPA (dispatcher, filters, status
reports, storage, proto). Capture as its own design doc when the time
comes rather than folding it into a refactor train.

## DtnNodeId: validating constructor + private `node_name`

`eid::DtnNodeId { pub node_name: Box<str> }` exposes its inner field publicly with no validating constructor, so it can hold a syntactically-invalid `dtn` authority. The parser is the only path that validates the name (`eid/parse.rs` — regname grammar + percent-decode), but external code builds it straight from the field: `bpa/src/node_ids.rs` (`NodeId::Dtn(DtnNodeId { node_name })`) and `bpa/fuzz/src/eid.rs`. `Display` (`eid/mod.rs`) re-emits `dtn://{node_name}/` verbatim with no percent-encoding, so an invalid or non-canonical `node_name` round-trips to an invalid EID.

This is the "inappropriate `pub` inner" smell, but unlike `BundleAge`/`Lifetime` (privatised during the 2026-06-05 newtype review, since `From<u64>` already gave a construction path) `DtnNodeId` has no safe constructor to fall back on, so this is a cross-crate change plus a design decision — do we want `DtnNodeId` to *guarantee* a valid name?

- Add a validating `TryFrom<&str>` / `new` that runs the regname grammar (share the `parse_regname` logic in `eid/parse.rs`).
- Make `node_name` private with a read accessor.
- Route the `bpa::node_ids` and fuzz-harness construction sites through the new constructor.
- Decide whether `node_name` stores the percent-decoded or the wire form, and make `Display` re-encode to match — fixes the current asymmetric round-trip.

Spotted 2026-06-05 during the bpv7 newtype `pub`-field review. The other single-value wrappers were checked and are fine: `IpnNodeId` (plain coordinate pair), `StatusAssertion`, and the rfc9173 `Results` wrappers (transparent value holders, no `From` redundancy, no construction invariant).

## Multi-target BIB shares one key (whole-codebase review 2026-07-08, #2)

`bpsec::signer` groups all `sign_block()` requests with the same `(security source, context)` into one multi-target BIB, but in key-wrap mode each target's operation mints its own random CEK; only the first operation's parameters (the wrapped CEK) are emitted, so every non-first target's HMAC was computed with a key absent from the wire and can never verify (at any RFC 9173 implementation, including Hardy). Direct mode similarly mis-merges per-target keys/variants.

RFC 9173 §3.8.2 is explicit: the HMAC key is derived from the single wrapped-key parameter *of the BIB* (one per block) and compared against the per-target results — i.e. one shared key, N per-target results. So the fix is **not** a per-target split (that is the encryptor's BCB-AES-GCM rule, driven by unique IVs). BIB-HMAC-SHA2 legitimately shares: generate one CEK per `(source, context)` group, wrap it once into the single emitted parameter set, and (direct mode) enforce one key per group. `can_share()` for BIB-HMAC-SHA2 should be `true`.

Latent, not a runtime bug: the BPA never signs, and the `bundle sign` CLI signs a single block per invocation, so no multi-target BIB is produced by shipped paths. Fix on the bpsec-editor work rather than as a standalone patch.

## bpv7-parse review triage (2026-08-19)

Open items from the `refactor/bpv7-parse` deep review (`references/reviews/bpv7-parse-review.md`, per-finding dispositions inline there) that are fixed nowhere in the refactor train. The behavioural items (E3 coverage-clearing, the `remove_blocks` screen and Maybe pull-back, E4's override-clearing, E10's checked flatten, the E11e removed-set, the dead error variants, and the semantic_eq / checks contract docs) landed on this branch on 2026-08-19; what follows is the remainder. The E3 CRC-restoration sliver was ruled intentional-as-is on 2026-08-25 (an unprocessable BIB never made a checkable integrity statement, so wholesale removal restores no CRC — rationale documented on `BPSecEditor::remove_integrity` and at the `remove_block_inner` coverage-clearing site), and the E8 release note landed in the crate changelog; both items are closed.

**Idiomatics (E11 a/d).** `BibCoverage` derives `Clone` but not `Copy` (forcing `.clone()` noise); staged BIB plaintext is copied out of `Zeroizing` into a plain `Vec` in `bpsec::edit` step 3 (contrast `remove_encryption`'s `mem::take`).

**`insert_block` Keep-reuse bypasses the cascade (E12, pre-existing).** Reusing an existing same-type block builds a fresh template with neither the security cascade nor the `bib`/`bcb` metadata `update_block_inner` preserves — replacing a signed PreviousNode via `insert_block` leaves a stale BIB signature over the new body. Pre-dates the parse refactor; fold into the next editor-cascade pass.

## Decision: `__Reserved` stays an inhabited placeholder

Recorded because it looks like an oversight and is not.
`bpsec::signer::Context` and `bpsec::encryptor::Context` each carry a
`#[doc(hidden)] __Reserved` unit variant. The obvious tidy-up is to make
it uninhabited (`__Reserved(core::convert::Infallible)`) so the compiler
knows it can never be constructed and the match arms collapse.

Do not. The `bpsec` feature can be enabled without any security context
feature, and in that configuration `__Reserved` is the enum's only
variant. An uninhabited placeholder would make `Context` itself
uninhabited, which in turn makes `sign_block` / `encrypt_block`
uncallable rather than merely unsuccessful. The current design keeps
them callable and has them return `Error::UnsupportedOperation`, which
is the behaviour a caller compiled without contexts wants: a typed
failure, not a build error. See the comments at `bpsec/mod.rs:45` and
the fallthrough arms in `signer.rs` / `encryptor.rs`.

Revisit only if the `bpsec` feature stops being independently
enablable.

## `bpsec::key_wrap::Error` is not nameable from outside the crate

`bpsec::Error::KeyWrap` carries a `key_wrap::Error`, but `key_wrap` is a
private module, so an external caller can match the variant and never
name or inspect its payload. This is the same defect the security
context error split fixed by making `bpsec::context` public.

It is currently latent rather than harmful: the one path a caller cares
about, a failed CEK unwrap during BIB verification, is deliberately
collapsed to `Error::IntegrityCheckFailed` in
`bpsec::context::bib_hmac_sha2` so the verifier gives no oracle
distinguishing a bad KEK from a bad MAC. So no shipped path surfaces
`KeyWrap` to a caller today. Fix by making the module public (and its
error type documented) the next time the BPSec error surface is opened.

## Completed

- **BPSec error split.** The 39-variant `bpsec::Error` was split into
  focused leaves that live with their owners: `bpsec::asb::Error` (ASB
  grammar: target/result mismatches, duplicate targets, canonical-form
  violations), `bpsec::context::Error` (context parameter and result
  decoding, including the AES-GCM IV length rule), and
  `bpsec::key_wrap::Error`. All three compose back onto the umbrella
  `bpsec::Error` with `#[from]`. Policy and verification variants the BPA
  matches on (`NoKey`, `DecryptionFailed`, `IntegrityCheckFailed`,
  `MaybeHasBib`, `UnsupportedOperation`, …) stayed directly matchable on
  the umbrella. RNG failure is now an opaque `Error::Rng` unit variant
  rather than an `Algorithm(String)` carrying the reason.
- **RFC 9172 §3.8 BCB-shares-target-with-BIB** is enforced in the keyed
  pass, in `checks::decrypt_and_validate_covered_bibs`, immediately after
  a BCB-encrypted BIB's OperationSet is decrypted and structurally
  checked. It fires only when `bcb_op_set.can_share()`, which
  BCB-AES-GCM answers `false` to because its IV lives in the context
  parameters.
- **Editor unit tests** moved from `src/editor.rs` to `tests/editor.rs`,
  and the builder's file-scope tests to `tests/builder.rs`.
