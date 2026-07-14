# bpv7 TODO

> Status (2026-07-08): The keyed §3.8 deferral and the push-parser migration below describe the push parser (`BundleParser` / `bundle/raw_parse.rs`), which is in progress on the `refactor/parse` branch and not yet merged. On main, `bundle/parse.rs` is still the whole-bundle parser and `raw_parse.rs` does not exist, so these sections are planned/in-flight work, not the state of main. Streaming AES-GCM is unstarted on every branch.

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
- `aes` v0.9.1 — `BlockEncrypt` for computing H and encrypting J0

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
   `bpv7/src/bpsec/rfc9173/streaming_aes_gcm.rs`.

3. Add unit tests using the RFC 9173 test vectors (already in
   `bpv7/src/bpsec/rfc9173/test.rs`). Verify that streaming
   encryption produces identical output to the existing
   `aes-gcm` crate for the same inputs.

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

Current (transitive via `aes-gcm` 0.10.3):
- `aes` 0.8.4
- `ctr` 0.9.2
- `ghash` 0.5.1
- `cipher` 0.4.4

To add as direct:
- `ctr = "0.9"` with features `["zeroize"]`
- `ghash = "0.5"` with features `["zeroize"]`

No new crate downloads — these are already resolved in the
lock file.

### Phase

This is **Phase 3** work in the streaming pipeline design
(security gateway). Header-block BCB targets are small and
continue to use the existing all-at-once API until Phase 3.

## Keyed BPSec filter: RFC 9172 §3.8 BCB-shares-target-with-BIB

### Background

RFC 9172 §3.8 requires that "a BCB MUST NOT target a BIB unless it
shares a security target with that BIB" — except when the BCB's
security context does not support sharing (e.g., BCB-AES-GCM, where
IV uniqueness rules out sharing). The legacy whole-bundle parser
enforces this at `bpv7/src/bundle/parse.rs:552-568` after decoding
the BIB OperationSet, gated on `bcb.can_share()`.

### Why the keyless push-parser does not enforce this

The check needs to compare the BIB's target list against the BCB's
target list. The BCB target list is always available (BCB
OperationSets are plaintext). The BIB target list is only available
if the BIB itself is decryptable. The §3.8 check fires *only* when
the BIB is BCB-encrypted — which is precisely the case where the
keyless parser cannot decode the BIB OperationSet. Catch-22.

The keyless parser therefore marks every block as
`BibCoverage::Maybe` when any BIB is BCB-encrypted (see
`raw_parse::validate_bpsec_structure`'s Maybe-sweep) and defers the
§3.8 check to the post-decrypt keyed filter.

### What the keyed filter needs to do

After decrypting a BCB-encrypted BIB OperationSet, the filter
should call `check_bib(&ops, bib_block_number, bundle)` (the same
free function the parser uses for plaintext BIBs — single source of
truth for the per-OperationSet rules) and additionally:

```rust
// RFC 9172 §3.8
if let Some(bcb_block_number) = bundle.blocks[&bib_block_number].bcb
    && let Some(bcb_ops) = decrypted_bcbs.get(&bcb_block_number)
    && bcb_ops.can_share()
    && !ops.operations.keys().any(|t| bcb_ops.operations.contains_key(t))
{
    return Err(bpsec::Error::InvalidBCBTarget.into());
}
```

The filter holds the BCB OperationSets in scope (it parsed them to
do the decryption) so the comparison is local.

Consider promoting this into `check_bib` itself by adding a
`bcbs: &HashMap<u64, bpsec::bcb::OperationSet>` parameter once the
keyed filter's BCB store stabilises — would keep the free function
as the single source of truth for *all* per-OperationSet BIB rules,
keyless and keyed.

## Push parser migration plan

### Where we are (on `refactor/parse`, 2026-05-19)

`bundle/raw_parse.rs` (`BundleParser`) performs all keyless
structural validation that the legacy `bundle/parse.rs` does:
canonical CBOR enforcement, primary/extension block flag combos,
duplicate-block rules, outer-array termination, and BIB/BCB
structural rules (target existence/type, must-replicate, delete
flag, intra-bundle uniqueness, §9172 §3.9 BIB-must-be-encrypted).
It produces a `raw_parse::Bundle` carrying primary fields plus a
block index with `extent`, `data`, `bib` (None/Some/Maybe), `bcb`.

The legacy whole-bundle parser is still wired into every consumer
(BPA filters, dispatcher, BPSec, fuzz harnesses, tests). Nothing
uses the push parser in production yet.

### Milestone 1 — Wrap legacy parser around push parser

**Goal:** `bundle/parse.rs` collapses to a thin adapter that
(a) drives `BundleParser::push` (or accepts a whole-bundle blob),
(b) runs the *key-dependent* checks against the resulting
`raw_parse::Bundle`, and (c) assembles today's public `bundle::Bundle`
shape so consumers see no API change.

**Parser interface change:** `BundleParser::finish()` returns a
`ParseResult` rather than a bare `Bundle`:

```rust
pub struct ParseResult {
    pub bundle: Bundle,                            // persistent BPA-pipeline index
    pub bibs: HashMap<u64, bib::OperationSet>,    // parser byproduct
    pub bcbs: HashMap<u64, bcb::OperationSet>,    // parser byproduct
}
```

`Bundle` is the persistent representation passed through the BPA
pipeline (it's an index over the wire bytes plus stamped `.bib`/`.bcb`
coverage on each block). OperationSets are not part of that — they
are parser byproducts that the wrapper consumes for the keyed pass
and then drops. The persistent record of BPSec coverage lives on
`Block::bib` / `Block::bcb`.

**Efficiency win for security ingress filters.** Today the security
filter re-parses every BIB and BCB OperationSet from raw bytes,
duplicating CBOR-decode work the parser already did for keyless
validation. With `ParseResult` flowing through, the filter receives
the OperationSets pre-decoded:

- plaintext BIBs → jump straight to `op.verify(…)` per target
- BCBs → jump straight to decrypting each target
- BCB-encrypted BIBs (`Maybe`) → filter still decodes one
  OperationSet per BIB after decryption, but only those

At gateways with heavy BPSec, this drops per-bundle CBOR work
roughly in half.

**Parser additions that don't need keys (do inline at decode time):**

- **`is_unsupported() && delete_bundle_on_failure` → hard reject.**
  When a BIB or BCB OperationSet decodes successfully but its
  security context is one we don't implement, and the block flags
  the bundle for deletion on failure, return `Error::Unsupported(n)`
  from the parser. No reason to ship that bundle into the keyed
  pass.

**With-keys work that lives in the wrapper:**

1. **Decode BCB-encrypted BIBs.** For each block left with
   `BibCoverage::Maybe`, the wrapper decrypts the BIB body via the
   key provider, then runs `check_bib(&ops, n, &bundle)` on the
   plaintext OperationSet.

2. **RFC 9172 §3.8 BCB-shares-target-with-BIB.** Already TODO'd
   above — fires on decrypted BIBs whose BCB context supports
   sharing.

3. **BIB cryptographic verification.** `op.verify(key_source, …)`
   per BIB target. `NoKey` is skip-not-fail.

4. **BCB decryption.** Per filter policy: decrypt the BCB's targets
   and re-canonicalise the resulting plaintext blocks.

5. **Remaining `is_unsupported()` consequences.** For
   `report_on_failure` → emit status report; for
   `delete_block_on_failure` (BIB only — BCB already errors on this
   flag in `check_bcb`) → queue block removal + bundle rewrite. The
   wrapper iterates `result.bibs.values()` / `result.bcbs.values()`
   and inspects the corresponding `result.bundle.blocks[n].flags`.

6. **Canonical-rewrite tracking.** Blocks whose OperationSets
   weren't shortest-form get re-emitted; the wrapper records this
   so it can rebuild the bundle bytes when something downstream
   needs the canonical form.

7. **Public `bundle::Bundle` assembly.** Flatten the nested
   `raw_parse::Bundle` into today's shape (scattered primary
   fields, blocks HashMap). Per the Milestone 2 decision, ranges
   are `Range<u64>` — either flip `bundle::Block` first (Milestone 2
   before Milestone 1) or do the `u64`/`usize` conversion at the
   boundary as a temporary measure. Deletes `bundle/primary_block.rs`'s
   Result-wrapped intermediate either way.

### Milestone 2 — Block struct consolidation

`raw_parse::Block` and `bundle::Block` are duplicates apart from
`Range<u64>` vs `Range<usize>`. **Decision: commit to `Range<u64>`
workspace-wide for all bundle-byte offsets.** Rationale: 32-bit
target compat (Cortex-M, RISC-V32, etc.), uniform offset arithmetic
between the push parser and downstream consumers, no boundary
conversions.

**Audit scope:** ~70 sites across editor.rs, parse.rs,
`payload_range()` / `payload()` helpers, builder.rs, and test
fixtures. Mostly mechanical (`as u64` / `as usize` at slice
boundaries) — tests catch arithmetic regressions.

After the migration: one `block::Block`, no shim, no boundary
conversions. `raw_parse::Block` deleted.

### Milestone 3 (deferred) — Bundle reshape (Option 2)

End state: `bundle::Bundle` is a pure wire-format representation
(`{ primary: PrimaryBlock, blocks: HashMap<u64, Block> }`).
Per-hop processing state — bundle age, ingress timestamps, dispatch
attempts, BIB/BCB coverage — moves to a BPA-side `BundleMetadata`
keyed by bundle id. Massive blast radius across BPA (dispatcher,
filters, status reports, storage, proto). Out of scope for the
push-parser migration; capture as its own design doc when the time
comes. `raw_parse::Bundle` is already the right shape, so the
eventual migration is mechanical at the boundary.

### Currently deferred items

- §9172 §3.8 BCB-shares-target-with-BIB → Milestone 1.

## DtnNodeId: validating constructor + private `node_name`

`eid::DtnNodeId { pub node_name: Box<str> }` exposes its inner field publicly with no validating constructor, so it can hold a syntactically-invalid `dtn` authority. The parser is the only path that validates the name (`eid/parse.rs` — regname grammar + percent-decode), but external code builds it straight from the field: `bpa/src/node_ids.rs` (`NodeId::Dtn(DtnNodeId { node_name })`) and `bpa/fuzz/src/eid.rs`. `Display` (`eid/mod.rs`) re-emits `dtn://{node_name}/` verbatim with no percent-encoding, so an invalid or non-canonical `node_name` round-trips to an invalid EID.

This is the "inappropriate `pub` inner" smell, but unlike `BundleAge`/`Lifetime` (privatised during the 2026-06-05 newtype review, since `From<u64>` already gave a construction path) `DtnNodeId` has no safe constructor to fall back on, so this is a cross-crate change plus a design decision — do we want `DtnNodeId` to *guarantee* a valid name?

- Add a validating `TryFrom<&str>` / `new` that runs the regname grammar (share the `parse_regname` logic in `eid/parse.rs`).
- Make `node_name` private with a read accessor.
- Route the `bpa::node_ids` and fuzz-harness construction sites through the new constructor.
- Decide whether `node_name` stores the percent-decoded or the wire form, and make `Display` re-encode to match — fixes the current asymmetric round-trip.

Spotted 2026-06-05 during the bpv7 newtype `pub`-field review. The other single-value wrappers were checked and are fine: `IpnNodeId` (plain coordinate pair), `StatusAssertion`, and the rfc9173 `Results` wrappers (transparent value holders, no `From` redundancy, no construction invariant).

## Move editor unit tests to the integration-test crate

The 23 tests in `bpv7/src/editor.rs`'s inline `#[cfg(test)] mod tests` exercise only the public API, so they belong in `bpv7/tests/editor.rs` alongside `remove_block_rejects_security_block` (moved there 2026-07-09 with the R-3 `remove_block` security-block guard).

Verified all items they touch are `pub`: every `Editor` method used (`new`, `with_source`/`with_destination`/`with_report_to`/`with_lifetime`/`with_bundle_crc_type`, `push_block`, `insert_block`, `with_data`, `remove_block`, `rebuild`, `rebuild_bundle`), `Chunk::flatten` / `Chunk::flatten_inplace`, `bundle::RewrittenBundle::parse_with_keys`, and every `block::Block` field the assertions read (`bib`, `bcb`, `extent`, `data`, plus `block_type`/`flags`/`crc_type`). Nothing needs a visibility widening.

The move is mechanical: relocate the five private helpers too (`make_bundle`, `make_bundle_with_hop_count`, `ok`, `reparse`, `assert_rebuild_matches_parse`), convert `super::*` / `crate::` paths to `hardy_bpv7::`, and drop the `#[cfg(test)]` gate (integration tests get `serde`/`std` from the `[dev-dependencies]` self-dep).

Do this on `refactor/parse` rather than as a standalone cleanup off main. Milestone 2's `Range<u64>` audit already rewrites ~70 sites across `editor.rs` and its test fixtures — the assertions in `assert_rebuild_matches_parse` read `block.extent` / `block.data`, which flip to `Range<u64>` there — so folding the relocation into that pass keeps `editor.rs` churn in one place and avoids conflicts between a main-side move and the refactor-side edits.

## Multi-target BIB shares one key (whole-codebase review 2026-07-08, #2)

`bpsec::signer` groups all `sign_block()` requests with the same `(security source, context)` into one multi-target BIB, but in key-wrap mode each target's operation mints its own random CEK; only the first operation's parameters (the wrapped CEK) are emitted, so every non-first target's HMAC was computed with a key absent from the wire and can never verify (at any RFC 9173 implementation, including Hardy). Direct mode similarly mis-merges per-target keys/variants.

RFC 9173 §3.8.2 is explicit: the HMAC key is derived from the single wrapped-key parameter *of the BIB* (one per block) and compared against the per-target results — i.e. one shared key, N per-target results. So the fix is **not** a per-target split (that is the encryptor's BCB-AES-GCM rule, driven by unique IVs). BIB-HMAC-SHA2 legitimately shares: generate one CEK per `(source, context)` group, wrap it once into the single emitted parameter set, and (direct mode) enforce one key per group. `can_share()` for BIB-HMAC-SHA2 should be `true`.

Latent, not a runtime bug: the BPA never signs, and the `bundle sign` CLI signs a single block per invocation, so no multi-target BIB is produced by shipped paths. Fix on the bpsec-editor work rather than as a standalone patch.
