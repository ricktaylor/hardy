# bpv7 TODO

## Streaming AES-GCM for BPSec BCB

### Background

The current `bcb_aes_gcm.rs` uses `aes-gcm` v0.11 which — even with
its new `AeadInOut` in-place API — requires the entire
plaintext/ciphertext as a contiguous buffer. This blocks
streaming payload encryption/decryption in the cryptographic stages
(see `bpa/docs/streaming_pipeline_design.md` §6.1.2, §7.3).

AES-GCM is internally AES-CTR + GHASH — both inherently streamable.
The low-level crates are already in Hardy's dependency tree as
transitive dependencies of `aes-gcm`:

- `ctr` v0.10.1 — `StreamCipher::apply_keystream(&mut chunk)`
- `ghash` v0.6.0 — `UniversalHash::update_padded(&data)` + `finalize()`
- `aes` v0.9.3 — `BlockCipherEncrypt` for computing H and encrypting J0

### Design

Build a `StreamingAesGcm` wrapper that exposes push-based
encryption and decryption using the `ctr` and `ghash` crates
directly, replacing the `aes-gcm` crate's all-at-once API.

```rust
pub struct StreamingAesGcm<C: BlockCipherEncrypt + BlockSizeUser> {
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

GHASH pads only at the end of the AAD and at the end of the ciphertext, but `update_padded` pads on every call, so feeding it arbitrary chunk sizes produces a wrong tag. The wrapper buffers a partial 16-byte block across `aad_update` / `*_update` calls and flushes it, padded, at the switch from AAD to ciphertext and at finalize.

### Implementation Plan

1. Add `ctr` and `ghash` as direct dependencies (currently
   transitive only). Pin compatible versions with existing
   `aes-gcm` to avoid duplication.

2. Implement `StreamingAesGcm` in a new module
   `bpv7/src/bpsec/rfc9173/streaming_aes_gcm.rs`.

3. Add tests using the RFC 9173 Appendix A.2 and A.4 BCB vectors (pinned in `bpv7/tests/rfc9173.rs`, `rfc9173_appendix_a_2` and `rfc9173_appendix_a_4`). Verify that streaming encryption produces identical output to the existing `aes-gcm` crate for the same inputs.

4. Wire into the confidentiality stage (streaming pipeline design §6.1.1; not yet built):
   - `aad_update()` with scope-flag-constructed AAD (same as
     current `build_data()`)
   - `encrypt_update()` / `decrypt_update()` called per chunk
     as the payload streams through the stage
   - `encrypt_finalize()` / `decrypt_finalize()` at the end of the payload

5. Retain the existing `aes-gcm` dependency behind a feature
   flag for header-block BCB (small blocks, all-at-once is
   fine). The streaming wrapper is for payload-block BCB.

6. Eventually remove `aes-gcm` dependency entirely once all
   paths use the streaming wrapper.

### Zeroization

The streaming wrapper must zeroize sensitive state on drop:
- CTR key material (`ctr` 0.10's `zeroize` feature makes `CtrCore` `ZeroizeOnDrop` when the block cipher is)
- GHASH key (H)
- AES_K(J0) block

Use `zeroize::Zeroize` derive or manual impl on `StreamingAesGcm`.
Decrypted output is the caller's responsibility (the confidentiality
stage manages `Zeroizing<>` for decrypted payload buffers).

### Dependencies

Current (transitive via `aes-gcm` 0.11.1):
- `aes` 0.9.3
- `ctr` 0.10.1
- `ghash` 0.6.0
- `cipher` 0.5.2

To add as direct:
- `ctr = "0.10"` with features `["zeroize"]`
- `ghash = "0.6"` with features `["zeroize"]`

No new crate downloads — these are already resolved in the
lock file.

### Phase

This is **Phase D** work in the streaming pipeline design (§10, security gateway), after Phase B's streamed egress. Header-block BCB targets are small and continue to use the existing all-at-once API until then.

## RFC 9172 §3.8: the `can_share()` exemption is an interpretation

The keyed pass enforces RFC 9172 §3.8 ("A BCB MUST NOT target a BIB unless it shares a security target with that BIB") in `checks::decrypt_and_validate_covered_bibs`, after decrypting a BCB-encrypted BIB, and only when that BIB's BCB `can_share()`. BCB-AES-GCM cannot share, because the IV and any wrapped key are block-level security context parameters, so a multi-target BCB would reuse a key and IV, which RFC 9173 §4.3.1 forbids; the Encryptor therefore gives every target, a BIB included, its own BCB. Every encrypted BIB Hardy builds therefore sits under a BCB that shares no target with it, and the exemption is what stops the check rejecting Hardy's own bundles. RFC 9172 states no such exemption: read literally, §3.8 forbids §3.9's own "new BCB that targets the existing BIB" option, which is the only RFC 9173 layout without key and IV reuse, so an implementation that enforces §3.8 literally rejects every bundle in which Hardy has encrypted a signed block. A stricter per-BCB reading, under which a BCB that encrypts a BIB encrypts every target of that BIB, forbids the option too; only a whole-bundle reading permits it. The conflict is being raised with the IETF DTN WG. The CCSDS BPSec draft profile restates §3.8 as a mandatory conformance item, so the exemption is also a deviation from that profile; the PICS records it as item 16, note 1.

- Keep the exemption until the WG rules, then align the check with the ruling. If §3.8 is restated in terms of the whole bundle ("a BCB MUST NOT target a BIB unless every target of the BIB is the target of some BCB"), replace the `can_share()` gate with that check. It is the invariant the keyless parse's `BibCoverage::Maybe` sweep already assumes, but it makes the partial-acceptance state that `remove_encryption` produces today non-conformant (re-review R-3 in the bpv7-reader-editor review ledger below), so the R-3 fix must land first.

## DtnNodeId: validating constructor + private `node_name`

`eid::DtnNodeId { pub node_name: Box<str> }` exposes its inner field publicly with no validating constructor, so it can hold a syntactically-invalid `dtn` authority. The parser is the only path that validates the name (`eid/parse.rs` — regname grammar + percent-decode), but external code builds it straight from the field: `eid-patterns/src/dtn_pattern.rs` (`DtnPatternItem::try_to_eid`), `bpa/fuzz/src/eid.rs`, and test fixtures in `bpa/src/node_ids.rs` and `bpv7/tests/eid.rs`. The parser stores the percent-decoded name, and `Eid`'s `Display` and CBOR encoder re-encode it (`URI_ENCODE_SET`), but `DtnNodeId`'s own `Display` (`eid/mod.rs`) re-emits `dtn://{node_name}/` verbatim, so a name needing percent-encoding, or an invalid one, round-trips to an invalid EID.

This is the "inappropriate `pub` inner" smell, but unlike `BundleAge`/`Lifetime` (privatised during the 2026-06-05 newtype review, since `From<u64>` already gave a construction path) `DtnNodeId` has no safe constructor to fall back on, so this is a cross-crate change plus a design decision — do we want `DtnNodeId` to *guarantee* a valid name?

- Add a validating `TryFrom<&str>` / `new` that runs the regname grammar (share the `parse_regname` logic in `eid/parse.rs`).
- Make `node_name` private with a read accessor.
- Route the `eid-patterns` and fuzz-harness construction sites (and the test fixtures) through the new constructor.
- Keep `node_name` in the percent-decoded form the parser and `Eid` encoders already assume, and make `DtnNodeId`'s `Display` re-encode with `URI_ENCODE_SET` to match — fixes the asymmetric round-trip.

Spotted 2026-06-05 during the bpv7 newtype `pub`-field review. The other single-value wrappers were checked and are fine: `IpnNodeId` (plain coordinate pair), `StatusAssertion`, and the rfc9173 `Results` wrappers (transparent value holders, no `From` redundancy, no construction invariant).

## Multi-target BIB shares one key (whole-codebase review 2026-07-08, #2)

`bpsec::signer` groups the `sign_block()` requests of one `Signer` pass that share a `(security source, context)` into one multi-target BIB (the context carries the scope flags; the key is not part of the grouping), but in key-wrap mode each target's operation mints its own random CEK; only the first operation's parameters (the wrapped CEK) are emitted, so every non-first target's HMAC was computed with a key absent from the wire and can never verify (at any RFC 9173 implementation, including Hardy). Direct mode similarly mis-merges per-target keys/variants.

RFC 9173 §3.8.2 does allow a shared key: the HMAC key is derived from the single wrapped-key parameter *of the BIB* and compared against the per-target results. But a multi-target BIB under the default integrity scope of 7, which includes the security header, is exactly the construction erratum 8723 would prohibit: a downstream waypoint that splits it under RFC 9172 §3.9 moves results into a BIB with a different block number, and every moved MAC then fails, because RFC 9173 §3.7 step 4 puts the BIB's own block number in the IPPT. Ruled 2026-09-29: emit **one BIB per target**, as the encryptor emits one BCB per target. A BCB over the target then matches all of the BIB's targets, so the first rule of §3.9 applies and no split is ever needed, and each BIB carries its own wrapped CEK, which removes the key-wrap bug by construction; the cost is one ASB header and parameter set per target. Rejected: keeping multi-target BIBs with one CEK per group and the security-header flag cleared whenever a group has more than one target (erratum 8723's condition), which saves bytes but drops the binding to the BIB's own header and still forces other encryptors to widen or refuse.

Latent today: the BPA never signs, and the `bundle sign` CLI signs a single block per invocation, so no shipped path produces a multi-target BIB. That ends when the BPA starts signing at egress; fix before then.

## Key-material zeroization gaps (ingress-spool key review, 2026-09-03)

Gaps found while auditing what key material the streaming ingress carries across `await` points (answer: none — the raw CEK copy is confined to `begin_verify`'s sync scope and wiped, and only key-*derived*, fixed-size MAC state rides the drain). Both are in material that *stays behind*:

**`hmac`'s `zeroize` feature is off.** `bpv7/Cargo.toml` enables `aes-gcm`'s `zeroize` feature but not `hmac`'s (`zeroize = ["digest/zeroize"]` in hmac 0.13), so the ipad/opad key-derived digest state inside every `Hmac` — including the state a streaming `bib::Verifier` carries across the ingress drain — is not wiped on drop. One-line feature addition; verify the transitive `digest`/`block-buffer` impls actually cover `HmacCore` before claiming the property.

**The server's key-file loader leaves decode scratch unwiped.** `bpa-server/src/bpsec.rs` loads the `KeySet` with `serde_json::from_reader`, which stages string content, including each base64url key, in an internal scratch buffer that is never wiped; `bpv7`'s own key decode uses wiped temporaries (`bpsec/key.rs`). Read the file into a `Zeroizing` buffer and deserialize from the slice, so the key text lives only in memory that is wiped.

## bpv7-parse review triage (2026-08-19)

The one item left open from the `refactor/bpv7-parse` deep review:

**Idiomatics (E11 a/d).** `BibCoverage` derives `Clone` but not `Copy`, forcing `.clone()` noise in `editor.rs`; staged BIB plaintext is copied out of `Zeroizing` into a plain `Vec` (`plaintext.as_ref().to_vec()`) in `remove_blocks` step 3 and in `remove_encryption`'s covered-BIB staging (`bpsec/edit.rs`), whereas `remove_encryption` moves its target payload out with `mem::take`.

## bpv7-reader-editor review ledger (2026-09-28)

**No `cargo doc` gate (review 2.9).** No workflow runs `cargo doc` (`docs.yml` builds only the mkdocs site), so nothing catches a broken intra-doc link. Add a workspace `cargo doc --no-deps --all-features` step with `RUSTDOCFLAGS="-D warnings"` to the `checks` job in `rust.yml`.

**`remove_encryption` leaves an encrypted BIB over a decrypted target (re-review R-3).** With BCB-AES-GCM the Encryptor emits one BCB per target, so a signed-then-encrypted block X carries one BCB over X and another over the BIB that covers X. `bpsec::edit::remove_encryption(X)` decrypts under X's BCB only: the same-BCB BIB handling it documents is unreachable with a context that cannot share a BCB. The result is conformant and verifiable by a key-holder, but X is no longer BCB-covered, so a keyless parse reads its coverage as `None` rather than `Maybe`, and an editor (a relay's per-hop rewrite, or a Stage 2 Rewriter through `ExtensionEditor`) modifies it; the key-holder's check of the hidden result then fails closed. For a per-hop block this state is invalid under the block lifecycle model (the node that decrypted the block was its acceptor and should have ended the BIB's operation too), so a relay's per-hop rewrite of it is legitimate; the residual concerns other blocks. Fix direction: when the decrypted target's BIB sits under a different BCB, `remove_encryption` also decrypts and strips that BCB if it holds the key, restoring a plaintext BIB (which a relay's per-hop rewrite then strips by policy), and refuses otherwise; the `bundle remove-encryption` CLI inherits the change. That holds only while every other target of the BIB is plaintext too. Where some stay encrypted, a plaintext BIB over them would violate RFC 9172 §3.9 and expose their plaintext to a MAC oracle, so instead remove X's result from the BIB and re-encrypt the rest under a BCB this node sources (the step RFC 9172 omits; the Security Source follows the re-encryption ruling in the per-hop audit ledger below). Partial acceptors in other implementations can produce the same state, so the parser-side residual remains, as the `BibCoverage::Maybe` doc and the CHANGELOG record.

## Fix the BIB-HMAC-SHA2 IPPT for a primary-block target (RFC 9173 A.3, 2026-09-28)

`bpsec/rfc9173/bib_hmac_sha2.rs`'s `absorb_resident_target` emits a CBOR byte-string head before every target, including a primary-block target, so the IPPT for OP(bib-integrity, primary block) wraps the canonical primary block in a byte string. The normative text requires it bare: RFC 9173 §3.7 step 5 makes the target's contribution "the canonical form of the primary block", RFC 9172 §4 defines that form "as specified in [RFC9171]" and forbids "any other encapsulating CBOR encoding", and RFC 9171 §4.3.1 represents the primary block as a CBOR array. The note to §3.7 ("avoids adding the bundle's primary block twice") confirms that steps 2 and 5 contribute the same bytes, and the scope-flag path in `ippt_prefix` already appends the primary block bare. The wrapping exists only to reproduce the published RFC 9173 Appendix A.3 vector, which is itself in error, as RFC 9173 erratum 9192 records.

- In `absorb_resident_target`, emit the canonical primary block without the byte-string head when the target is the primary block. `sign` and the resident `verify` share the function, so this is the single change point for the normative form (the transition fallback below adds a wrapped retry to the resident `verify` only); `begin_verify` never sees a primary-block target (its only caller, `checks::begin_payload_verification`, verifies the payload).
- Move `rfc9173_appendix_a_3` in `bpv7/tests/rfc9173.rs` to erratum 9192's corrected vector, which keeps it a pinned external vector (cite the erratum in the test): the primary-block MAC becomes `8e059b8e71f7218264185a666bf3e453076f2b883f4dce9b3cdb6464ed0dcf0f`, and the ASB, BIB and bundle encodings change only in those 32 bytes. Keep the published A.3 bundle as a test that verifies only through the transition fallback below, and fails with the fallback disabled.
- Interop stance, ruled 2026-09-29: signing emits only the normative form; verification tries the normative form first and falls back to the A.3 form behind an option that is on by default during a transition, logs each fallback, and is removed once erratum 9192 is verified and peers have fixed. Implementations that copied A.3 would otherwise reject Hardy's primary-block BIBs and have theirs rejected, and A.3 is the only published primary-block vector, so most probably did. Both forms need the HMAC key, so the fallback adds code but no forgery path. The stance matters early for CCSDS: its BPSec draft profile (734.5-R-2, Annex B) requires the RFC 9173 contexts for interoperability testing, and its BPv7 specification (734.20-O-1, §2.5.2) intends to recommend integrity over the primary block.
- Record the change in the CHANGELOG: it is wire-visible for every BIB over the primary block.

## BPSec per-hop paper audit ledger (2026-09-29)

Items found checking Hardy's BPSec behaviour against RFC 9172 and RFC 9173 while drafting a working-group discussion paper on per-hop blocks (in draft). The paper's block lifecycle model, used below, holds that a node that must change or remove a block is the acceptor of every security operation on it, and the security source of any protection on the block that replaces it; a BIB split or a re-encryption is such a change. The signer's multi-target BIBs are under "Multi-target BIB shares one key" above.

**The encryptor refuses a primary-covering BIB only by accident.** `Encryptor::encrypt_block` does not split a BIB that covers its target: it widens the encryption to every other target of that BIB, and to the BIB, each under its own single-target BCB. When the BIB also covers the primary block, as RFC 9173 Appendix A.3's BIB over blocks 0 and 2 does, `encrypt_block` queues block 0 without complaint, and the failure surfaces only in `rebuild()` as the generic `Editor(PrimaryBlock)` ("Cannot edit the primary block"); no test covers it. Refusing is right, since a BCB must not target the primary block and a split would make this node the new BIB's security source under the lifecycle model, which needs the original source's key to accept the moved results (and under the default scope, results moved unchanged fail anyway), but make it deliberate: check in `encrypt_block` before queueing anything, return a dedicated error that names the BIB, and add a test on the A.3 bundle. The widening itself stays (ruled 2026-09-29, rather than requiring the caller to name every target of the BIB), but state it in `encrypt_block`'s rustdoc, which says nothing about encrypting blocks the caller did not name; only an inline comment and PICS note 2 describe it.

**Re-encrypting a BIB under a received multi-target BCB-AES-GCM block emits a wrong IV.** Hardy never builds a multi-target BCB-AES-GCM block, but it parses one: RFC 9173 Appendix A.4 encrypts the payload and a BIB under one BCB with one IV. When a `remove_blocks` cascade shrinks a BIB under such a BCB, `bpsec::edit::reencrypt_covered_bib` re-encrypts the BIB with a fresh `Operation` (a fresh IV, and a fresh CEK in key-wrap mode), puts it back into the same BCB's `OperationSet`, and re-emits the BCB. `bcb::OperationSet::to_cbor` writes only the first operation's context parameters, so one of the two targets is emitted with parameters that do not decrypt it. Reusing the original IV instead would be worse, since two plaintexts under one GCM nonce leak their XOR. When the covering BCB cannot share (`!can_share()`) and has other targets, move the re-encrypted BIB into a new single-target BCB and remove it from the original BCB's targets, leaving the other targets' shared parameters untouched. Ruled 2026-10-01: the new BCB names this node as its Security Source and is encrypted under a key this node sources, as RFC 9172 §3.6 defines the field (the BPA that inserted the block) and as the lifecycle model requires. Downstream acceptors then find this node's key through the field, which key distribution must provide. A BIB split under RFC 9172 §3.9 is the same case: it re-sources the moved results, which is why the encryptor widens rather than splits. Add a test built from the A.4 vector.

**PICS notes 1 and 2 misdescribe the implementation.** Note 1 says the §3.8 shared-target check is "not enforced on parse" for BCB-AES-GCM, but the keyless parse cannot run it for any context (an encrypted BIB's targets are ciphertext); the keyed pass, `checks::decrypt_and_validate_covered_bibs`, runs it only when the BCB `can_share()`. Note 2 says the implementation "widens the BCB"; it adds a single-target BCB for each other target of the BIB and one for the BIB, so the first rule of §3.9 is met only by reading those BCBs as a set, the same whole-bundle reading the §3.8 entry above depends on. Its reason for not splitting, that a split "would require the integrity keys", holds whatever the integrity scope under the lifecycle model: a split makes the splitting node the new BIB's Security Source, which must accept the moved results and sign them afresh. The security header (RFC 9173 §3.7 step 4) only adds that results moved unchanged would also fail to verify. Reword both notes, keeping the item numbering, which follows the later CCSDS draft rather than 734.5-R-2.

**`bundle sign`'s help misdescribes the signer.** The `long_about` in `bpv7/tools/src/cmd/sign.rs` says that when a BIB from the same security source already exists, "the target is added to that BIB's security target list". The `Signer` never extends an existing BIB: every group gets a new BIB, and an already-signed target is refused with `AlreadySigned`. It also says the scope flags control the "Additional Authenticated Data", which is BCB vocabulary; for BIB-HMAC-SHA2 they control the IPPT. Fix both sentences.

**The `Maybe` sweep marks per-hop blocks, and outlives the BIB that caused it.** Two behaviours keep a per-hop block from being edited where the per-hop rules allow it. First, the keyless parse (`parse.rs`) sets `BibCoverage::Maybe` on every BCB-covered non-security block whenever the bundle carries an encrypted BIB, with no regard to block type. An integrity operation on a per-hop block inside an encrypted BIB is invalid, so an editor may treat a per-hop block's coverage as `None` (bpa TODO, signed-then-encrypted park entry, fix direction (a)). Exempt the RFC 9171 per-hop types from the sweep, or let the editor disregard the mark for them, and keep the exception that entry records for the bundle in which every BCB-covered non-security block is per-hop. Second, when `Editor` removes a whole encrypted BIB (a `remove_blocks` failure-drop), `remove_block_inner` cannot read its targets and leaves their `Maybe` marks in place ("another encrypted BIB may cover them"), and `Editor::block` reads `Keep` blocks from the original parse, so a later `insert_block` in the same session still refuses, even when the removed BIB was the bundle's only encrypted BIB. Recompute the marks after such a removal: with no encrypted BIB left, `Maybe` collapses to `None`.

**A BIB the node holds no key for leaves no fact behind.** `checks::verify` skips a BIB under a BCB it cannot decrypt (`decrypt_and_validate_covered_bibs`, `NoKey`) and a BIB operation it cannot verify (`verify_all_bibs`, `NoKey`) without recording either in `VerifyFacts`, which carries `NoKey` only for the per-hop blocks it decrypts itself (`nokey_ext`). Where the node is the acceptor of such an operation, as it is of every operation on a per-hop block it must replace, that is a failed acceptance under the per-hop rules (RFC 9172 §5.1.2: the target is then processed according to security policy), and ingress policy cannot see it: today the forwarder strips a plaintext BIB operation it could not verify without knowing that it failed. Record `NoKey` outcomes per operation (target and BIB) in `VerifyFacts`, so that ingress can apply its policy to the failed acceptances and report them.

## ExtensionEditor accept-set invariant (external #712 review, 2026-09-30)

`ExtensionEditor` refuses at call time every edit its own `finish()` or a receiver would reject: the forbidden `report_on_failure` flag (`PrimaryBlock::forbids_report_on_failure`) and unrecognised CRC types, which the parser rejects, and Previous Node / Bundle Age / Hop Count data that does not decode as its type, which the parser never inspects but a receiving BPA's decode of those blocks rejects. That list is kept in step by hand, so a future parser or decode rule can widen the gap again, and a caller that treats a failed `finish()` or an unreadable result as fatal aborts on it. Pin the invariant with a fuzz target beside `random_bundles`: for every parseable bundle and every `ExtensionEditor` operation sequence, `finish()` succeeding implies the flattened result parses and its Previous Node, Bundle Age, and Hop Count blocks decode as their types (`Block::extract`). One known member the editor does not refuse: the 256 MiB pre-payload bound (`Error::ExtensionBlocksTooLarge`) — reachable only through inserts totalling that much, on a node whose `max_bundle_size` exceeds 256 MiB.

**Write doors take the parsed-only `CrcType::Unrecognised`.** `Builder::with_crc_type`, both `BlockBuilder::with_crc_type`s (builder and editor), `Editor::with_bundle_crc_type`, and `BundleTemplate.crc_type` take the wire `CrcType`, whose `Unrecognised(code)` only a parse produces meaningfully; the write fails late, at `crc::append_crc_value`, as `builder::Error::InternalError`. `ExtensionEditor::insert`'s call-time refusal is the stopgap. Fix direction: a write-side `crc::Kind` (`None`, `Crc16`, `Crc32`) taken by the write doors, with `From<Kind> for CrcType` and `TryFrom<CrcType> for Kind`, while `CrcType` stays the read type; the tools' `ArgCrcType` (`bpv7/tools/src/flags.rs`) already spells that set.

**The RFC 9173 `ScopeFlags` encoder lets `unrecognised` set named bits.** `ScopeFlags::to_cbor` seeds the wire value from `unrecognised` and ORs the three named flags on top, the defect `block::Flags` and `bundle::Flags` no longer have: a hand-built `unrecognised` carrying bit 0 puts `include_primary_block` on the wire while the field reads false, so the security source computes the IPPT/AAD without the primary block and the receiver's check fails. Mask the named bits out of `unrecognised` as those encoders do.

**`ExtensionEditor` holds its inner editor as `Option<Editor>`**, taken and put back around every operation with an `expect` on the invariant that it is present between operations. Consuming methods (`fn insert(self, …) -> (Self, Result<u64>)`) would remove those by construction, as would a `&mut self` door on the owner `Editor`.

**`ExtensionEditor` takes well-known block data as raw bytes.** It decodes Previous Node, Bundle Age, and Hop Count data only to refuse what does not decode, then discards the value. Typed data instead — an enum of `PreviousNode(Eid)`, `BundleAge(BundleAge)`, `HopCount(HopInfo)` and opaque bytes, or typed methods in the style of `Builder::with_hop_count` — would make undecodable well-known data unrepresentable, removing that refusal and its decode. It is cheapest to adopt before the BPA's Rewriter surface, which hands Rewriters this editor, ships in a release.
