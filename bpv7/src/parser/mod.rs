/*!
The streaming wire parser for BPv7 bundles ([RFC 9171]). [`BundleParser`]
drives the structural decode incrementally; [`parse()`] is the one-shot
convenience over it. Both yield a [`Parsed`] — the authoritative byte
buffer, the structural [`Bundle`], and the decoded
BPSec OperationSets. Keyed BPSec validation is layered on top by composing
the primitives in [`crate::checks`].

[RFC 9171]: https://www.rfc-editor.org/rfc/rfc9171.html
*/

use alloc::boxed::Box;

use bytes::{Bytes, BytesMut};
use hardy_cbor::decode::{Error as CborError, Untagged};
use smallvec::SmallVec;

use crate::{
    Error, HashMap, HashSet, Result, bpsec,
    bundle::{BibCoverage, Block, BlockFlags, BlockType, Bundle, PrimaryBlock},
    canonical::CaptureFieldErr,
    crc, eid,
};

use self::block_header::BlockHeader;
use self::payload_tail::{trailer_byte_len, try_consume_block_after_body};

mod block_header;
mod payload_tail;

pub use self::payload_tail::PayloadTail;

/// A successfully-parsed [`Bundle`] paired with the authoritative
/// `Bytes` buffer the parser owns (the source of truth that the
/// returned `Bundle::blocks` extent/data offsets index into) and its
/// decoded BPSec OperationSet maps (BCBs and BIBs, keyed by block
/// number). Returned by [`BundleParser::finish`] and [`parse()`].
///
/// Slice with `&parsed.data[block.payload_range()]` (or `block.extent`)
/// using [`data`](Self::data) rather than a separate copy of the input —
/// for the single-`push()` / one-shot [`parse()`] path this is the input
/// verbatim, but the multi-`push()` streaming path freezes the
/// concatenated staging buffer, so the offsets are only meaningful
/// against *that* buffer.
pub struct Parsed {
    /// The authoritative byte buffer the returned block offsets index into.
    pub data: Bytes,
    /// The structural bundle (primary block + blocks map).
    pub bundle: Bundle,
    /// Decoded BCB OperationSets, keyed by BCB block number.
    pub bcbs: HashMap<u64, bpsec::bcb::OperationSet>,
    /// Decoded BIB OperationSets, keyed by BIB block number.
    pub bibs: HashMap<u64, bpsec::bib::OperationSet>,
}

enum State {
    Start,
    PrimaryBlock(usize),
    Blocks(usize),
    Done,
    /// Headers + all BPSec blocks parsed, but the payload body is larger than
    /// the buffer (the streaming-fallback in `parse_blocks` fired). Terminal,
    /// like `Done`, but `push` reports it as [`ParserProgress::Partial`].
    Partial,
}

/// The outcome of a [`BundleParser::push`] call: either more input is needed,
/// or parsing reached one of its terminal states.
pub enum ParserProgress {
    /// More input is required before parsing can continue; carries a
    /// lower-bound hint for the number of additional bytes to feed next.
    NeedMore(usize),
    /// Parsing is complete. Carries the concatenation of all bytes received
    /// via `push()` as a single contiguous `Bytes`. Yielded exactly once.
    Ready(Bytes),
    /// Headers and all BPSec blocks are parsed, but the payload body is larger
    /// than the buffer. `consumed` is everything received so far (headers plus
    /// any payload-body prefix); pass it to [`BundleParser::finish`] to obtain
    /// the (header-only) [`Parsed`] index — in that `Parsed` the payload
    /// block's `extent` over-claims and `data` holds only `consumed`.
    ///
    /// The caller owns the rest of the stream from here: it drains the
    /// remaining bytes (e.g. from the CLA segment stream) and persists them.
    /// `tail` is a synchronous continuation, already fed the header and the
    /// body prefix in `consumed`; feed it each subsequent run of bytes via
    /// [`PayloadTail::push`] to carry the payload CRC and the block/outer-break
    /// checks to completion. Yielded at most once; do not `push` the parser
    /// after it.
    Partial { consumed: Bytes, tail: PayloadTail },
}

/// Incremental, push-based parser for a BPv7 bundle arriving in chunks.
pub struct BundleParser {
    chunk_size: usize,
    data: Option<BytesMut>,
    state: State,
    bundle: Option<Bundle>,

    /// RFC 9171 §4.4: PreviousNode, BundleAge, and HopCount blocks
    /// MUST appear at most once per bundle. Tracked as a small set
    /// during the streaming walk; `insert` returning `false` is the
    /// duplicate detection.
    unique_blocks: HashSet<BlockType>,

    /// Block numbers of every BIB encountered, recorded in walk
    /// order. BIBs are NOT parsed inline because a BIB may itself be
    /// the target of a BCB — in which case its body is ciphertext,
    /// not a valid OperationSet, and we can't decrypt without keys.
    /// `finish()` resolves this after BCBs are processed: BIBs that
    /// aren't BCB-protected get parsed and validated (body range
    /// recovered from `bundle.blocks[n].extent + .data`); BCB-protected
    /// BIBs are skipped and trigger a `BibCoverage::Maybe` sweep on
    /// the remaining blocks. Empty (no allocation) for bundles with
    /// no BIBs.
    pending_bibs: SmallVec<[u64; 4]>,

    /// Parsed BCB OperationSets for every BCB encountered, keyed by
    /// BCB block number. BCB bodies are always plaintext (the ASB
    /// describes what the BCB encrypts; the ASB itself isn't encrypted),
    /// so we can parse them inline during `parse_blocks`. Consumed by
    /// `finish()` for BCB cross-block validation and to mark BCB
    /// coverage on target blocks before the BIB pass runs.
    bcbs: HashMap<u64, bpsec::bcb::OperationSet>,

    /// Set when the streaming-fallback fires on an oversized payload: the CRC
    /// continuation, pre-fed the header + body prefix, that `push` hands out in
    /// [`ParserProgress::Partial`]. `None` on every other path.
    deferred: Option<PayloadTail>,
}

impl Default for BundleParser {
    fn default() -> Self {
        Self::new(4096)
    }
}

impl BundleParser {
    /// Creates a parser that grows its internal buffer in `chunk_size`-byte
    /// increments. [`Default`] uses 4096.
    pub fn new(chunk_size: usize) -> Self {
        Self {
            chunk_size,
            data: None,
            state: State::Start,
            bundle: None,
            unique_blocks: HashSet::with_capacity(3),
            pending_bibs: SmallVec::new(),
            bcbs: HashMap::new(),
            deferred: None,
        }
    }

    /// Feeds the next run of bytes and advances parsing, returning a
    /// [`ParserProgress`]: `NeedMore` if more input is required, or a terminal
    /// `Ready` / `Partial` result.
    ///
    /// # Panics
    ///
    /// Panics if called again after a terminal `Ready`/`Partial` result.
    pub fn push(&mut self, data_in: Bytes) -> Result<ParserProgress> {
        // If we have a cached buffer, extend with data_in and take it out for parsing.
        // Otherwise leave cached = None and parse against data_in directly.
        let cached = self.data.take().map(|mut buf| {
            buf.extend_from_slice(&data_in);
            buf
        });

        // `data` borrows from cached (a local) or data_in (a local) — never from self.
        let data: &[u8] = cached.as_deref().unwrap_or(&data_in);

        let r = match self.state {
            State::Start => self.parse_start(data),
            State::PrimaryBlock(offset) => self.parse_primary(data, offset),
            State::Blocks(offset) => self.parse_blocks(data, offset),
            State::Done | State::Partial => {
                panic!("push called after parser already reached a terminal state");
            }
        };

        match r {
            Ok(_) => {
                // Terminal state reached (Ok only ever returned at Done or
                // Partial). Hand back the consumed bytes as a single contiguous
                // Bytes:
                //   - multi-chunk: freeze the cached BytesMut (zero-copy)
                //   - single-chunk: the original data_in
                let bytes = match cached {
                    Some(buf) => buf.freeze(),
                    None => data_in,
                };
                match self.state {
                    // Oversized payload: the body didn't fit. Hand the caller
                    // the CRC continuation `parse_blocks` stashed (pre-fed the
                    // header + body prefix) so it can drain the tail.
                    State::Partial => {
                        let tail = self
                            .deferred
                            .take()
                            .expect("Partial state guarantees a stashed PayloadTail");
                        Ok(ParserProgress::Partial {
                            consumed: bytes,
                            tail,
                        })
                    }
                    _ => Ok(ParserProgress::Ready(bytes)),
                }
            }
            // `NeedMoreData` may surface directly (from `parse_start` /
            // `parse_blocks`) or wrapped in `InvalidField` field labels (when a
            // primary/extension field straddles a chunk boundary and bubbles up
            // through `parse_canonical`). Either is "feed me more", not a hard
            // error — `need_more` unwraps the field-label chain to find it.
            Err(e) => {
                if let Some(more) = need_more(&e) {
                    // First-time materialisation if we don't have a cache yet.
                    // try_into_mut is zero-copy when refcount=1.
                    let mut buf = cached.unwrap_or_else(|| match data_in.try_into_mut() {
                        Ok(b) => b,
                        Err(orig) => BytesMut::from(orig.as_ref()),
                    });
                    // `more` is a wire-derived shortfall (a block can claim a
                    // body far larger than any bundle we would accept), so it is
                    // only a growth hint — never an allocation size. Reserving it
                    // verbatim lets a hostile length abort the process before the
                    // caller's own size cap is consulted. Grow by at most one
                    // `chunk_size`; the buffer still fills from the bytes that
                    // actually arrive on later pushes.
                    buf.reserve(more.min(self.chunk_size));
                    self.data = Some(buf);
                    Ok(ParserProgress::NeedMore(more))
                } else {
                    Err(e)
                }
            }
        }
    }

    /// Drains the parser into the final bundle index plus the BIB and
    /// BCB `OperationSet`s decoded along the way. Returning the
    /// OperationSets lets the keyed BPSec filter skip a full CBOR
    /// re-decode of every BIB/BCB body (see `bpv7/docs/TODO.md` M1).
    /// Bundles with no BPSec return empty maps.
    ///
    /// `data` should be the buffer this parser handed back from
    /// [`push`](Self::push) — the [`ParserProgress::Ready`] buffer, or the
    /// [`ParserProgress::Partial`] `consumed` buffer. It is moved through
    /// `finish` and returned in the [`Parsed`] result so callers have a single
    /// authoritative byte source for the returned offsets — slicing their own
    /// copy of the input risks aliasing against a different buffer in the
    /// streaming case.
    ///
    /// After a [`ParserProgress::Partial`] the returned [`Parsed`] is
    /// header-only: `data` holds just `consumed` and the payload block's
    /// `extent` over-claims (its `end` lies beyond `data`). The keyless BPSec
    /// structural checks `finish` runs are header-only, so this is sound; the
    /// payload body — and its CRC — are the streaming caller's to validate.
    ///
    /// # Panics
    ///
    /// Panics if called before [`push`](Self::push) has returned a terminal
    /// [`ParserProgress::Ready`] or [`ParserProgress::Partial`] result.
    pub fn finish(mut self, data: Bytes) -> Result<Parsed> {
        assert!(
            matches!(self.state, State::Done | State::Partial),
            "finish called before parser reached a terminal state"
        );
        let bibs = if !self.bcbs.is_empty() || !self.pending_bibs.is_empty() {
            self.validate_bpsec_structure(&data)?
        } else {
            HashMap::new()
        };
        let bundle = self
            .bundle
            .expect("terminal state guarantees self.bundle is populated");
        Ok(Parsed {
            data,
            bundle,
            bcbs: self.bcbs,
            bibs,
        })
    }

    /// Cross-block structural validation of every BIB / BCB against
    /// the keyless rules from RFC 9172 §3.7 and §3.9. BCBs are processed
    /// first so each block's BCB-coverage is known, then BIBs (skipping any
    /// that are BCB-encrypted — their bodies are ciphertext and we can't
    /// decode the OperationSet without keys). BCB-protected BIBs trigger a
    /// `BibCoverage::Maybe` sweep on the remaining blocks.
    ///
    /// All errors map to existing `bpsec::Error` variants — no new
    /// error surface. As a side effect, populates `Block::bib` and
    /// `Block::bcb` on every targeted block so downstream filters
    /// can query coverage directly off the block index. Drains
    /// `self.pending_bibs` and returns the freshly-decoded BIB
    /// `OperationSet`s so the keyed BPSec filter can reuse them
    /// without re-decoding; `self.bcbs` is borrowed and left intact
    /// for `finish()` to hand back to the caller.
    fn validate_bpsec_structure(
        &mut self,
        data: &[u8],
    ) -> Result<HashMap<u64, bpsec::bib::OperationSet>> {
        let pending_bibs = core::mem::take(&mut self.pending_bibs);
        let bundle = self
            .bundle
            .as_mut()
            .expect("Done state guarantees self.bundle is populated");

        // BCB pass — tracks targets cross-BCB so the "block at most
        // one BCB target" rule (§3.9) is checked while we walk.
        // Borrow the bcbs map so we can hand it back to the caller
        // intact at the end.
        for (bcb_block_number, ops) in &self.bcbs {
            // Per-OperationSet rules (including §3.9 duplicate-target
            // check via the already-stamped .bcb fields). Single source
            // of truth shared with the post-decrypt keyed filter.
            ops.check(
                *bcb_block_number,
                &bpsec::PlainBlockSet {
                    blocks: &bundle.blocks,
                    source_data: data,
                },
            )?;

            // Stamp each target. OperationSet::check has already verified
            // every target exists and is not claimed by a different BCB.
            for &target_number in ops.operations.keys() {
                bundle
                    .blocks
                    .get_mut(&target_number)
                    .expect("OperationSet::check verified every target exists")
                    .bcb = Some(*bcb_block_number);
            }
        }

        // BIB pass — BCB coverage on every block is now settled, so
        // we can tell which BIBs we can decode and which are
        // ciphertext. Decode the plaintext ones, validate their
        // targets, stash them for the caller, and remember whether any
        // were skipped.
        let mut bibs = HashMap::with_capacity(pending_bibs.len());
        let mut has_undecryptable_bibs = false;
        for bib_block_number in pending_bibs {
            let bib_block = bundle
                .blocks
                .get(&bib_block_number)
                .expect("BIB memoised but missing from bundle.blocks");
            if bib_block.bcb.is_some() {
                // BCB-protected BIB — body is ciphertext, can't decode
                // without keys. Defer to the Maybe-sweep below.
                has_undecryptable_bibs = true;
                continue;
            }

            // Body range recovered from the block index — no need for
            // the parser to have stashed it separately.
            let body = bib_block.payload_range();
            // Non-payload blocks are wholly inside the staged buffer, so
            // their offsets fit the address space by construction.
            let body_end = usize::try_from(body.end)
                .expect("non-payload block offsets are bounded by the in-memory buffer");
            let mut o = usize::try_from(body.start)
                .expect("non-payload block offsets are bounded by the in-memory buffer");
            // Bound the slice to the body: `OperationSet::from_cbor` uses
            // `parse_sequence`, which requires `offset == data.len()` at
            // completion (cbor series.rs:79). Handing it the whole bundle
            // trips `AdditionalItems` once the OperationSet ends.
            let ops: bpsec::bib::OperationSet =
                parse_canonical(&data[..body_end], &mut o, "BIB operation set")?;
            debug_assert_eq!(o, body_end);

            // Per-OperationSet rules (including §2.6 duplicate-target
            // check via the already-stamped .bib fields). Single source
            // of truth shared with the post-decrypt keyed filter.
            ops.check(
                bib_block_number,
                &bpsec::PlainBlockSet {
                    blocks: &bundle.blocks,
                    source_data: data,
                },
            )?;

            // Stamp each target. OperationSet::check has already verified
            // every target exists and is not claimed by a different BIB.
            for &target_number in ops.operations.keys() {
                bundle
                    .blocks
                    .get_mut(&target_number)
                    .expect("OperationSet::check verified every target exists")
                    .bib = BibCoverage::Some(bib_block_number);
            }

            bibs.insert(bib_block_number, ops);
        }

        // Encrypted BIBs whose targets we couldn't read: every non-
        // security block whose BIB coverage is still `None` becomes
        // `Maybe`.
        if has_undecryptable_bibs {
            for block in bundle.blocks.values_mut() {
                if !matches!(
                    block.block_type,
                    BlockType::BlockIntegrity | BlockType::BlockSecurity
                ) && matches!(block.bib, BibCoverage::None)
                {
                    block.bib = BibCoverage::Maybe;
                }
            }
        }

        Ok(bibs)
    }

    fn parse_start(&mut self, data: &[u8]) -> Result<usize> {
        // Bundle outer array head. RFC 9171 §4.1: a bundle SHALL be
        // represented as a CBOR *indefinite-length* array, so the only
        // conformant first byte is 0x9F. A definite-length outer array is
        // therefore non-conformant — and §4.1 explicitly lets an
        // implementation "MAY discard any sequence of bytes that does not
        // conform", which is what we do (the match below maps a
        // definite-length head to NotCanonical).
        //
        // §4.1 also grants a MAY-*accept* carve-out (definite-length arrays
        // are its worked example): an implementation may accept the
        // non-conformant bytes and "transform [them] into conformant BP
        // structure before processing", the transform itself being out of
        // scope. We deliberately decline it. It is optional; every real
        // BPv7 encoder emits the indefinite form; and the RFC's model is a
        // transform pre-pass, not a second framing mode — whereas the outer
        // 0xFF break is load-bearing both here (loop termination) and in
        // parse() (the completeness check). If a definite-length sender ever
        // turns up, add a normalisation shim in front of BundleParser
        // rather than making this parser bimodal.
        //
        // Note the asymmetry with BlockHeader, which accepts 0x85/0x86/0x9F:
        // individual block arrays MAY be definite OR indefinite (§4.1's
        // deterministic-encoding rule, "indefinite-length items are not
        // prohibited"). Only the *outer* array is pinned to indefinite.
        //
        // Appendix B's CDDL `bpv7_start = bundle / #6.55799(bundle)` is
        // informational and "the textual representation rules" on conflict,
        // so the self-describing CBOR tag (0xD9D9F7) is rejected here too.
        //
        // This is the first gate every datagram crosses, so under
        // adversarial traffic each malformed packet is rejected here: the
        // classification works from the single byte and the error variants
        // carry no owned data, keeping the reject path free of heap
        // allocation.
        let offset = match data.first() {
            None => return Err(Error::InvalidCBOR(CborError::NeedMoreData(1))),
            Some(0x9F) => 1,
            // Major type 4, definite length: a canonical-encoding violation
            // of §4.1's indefinite-length requirement, not a non-bundle.
            Some(0x80..=0x9B) => return Err(Error::NotCanonical),
            // CBOR unsigned integer 6 == the version byte opening an
            // RFC 5050 primary block.
            Some(0x06) => return Err(Error::PossibleBpv6),
            // Anything else — including a tag wrapper — cannot start a
            // bundle at all.
            Some(first) => return Err(Error::NotABundle(*first)),
        };
        self.state = State::PrimaryBlock(offset);
        self.parse_primary(data, offset)
    }

    fn parse_primary(&mut self, data: &[u8], mut offset: usize) -> Result<usize> {
        let block_start = offset;
        let primary: PrimaryBlock = parse_canonical(data, &mut offset, "primary block")?;

        // RFC 9171 §4.2.3-4 / §4.2.3-5: invalid bundle-flag combinations.
        // Null source ⇒ must not be a fragment, must set do_not_fragment,
        // and must not request any status reports. Admin record ⇒ must
        // not request any status reports.
        {
            let f = &primary.flags;
            let any_report = f.receipt_report_requested
                || f.forward_report_requested
                || f.delivery_report_requested
                || f.delete_report_requested;
            let null_source_bad =
                primary.id.source.is_null() && (f.is_fragment || !f.do_not_fragment || any_report);
            let admin_record_bad = f.is_admin_record && any_report;
            if null_source_bad || admin_record_bad {
                return Err(Error::InvalidFlags);
            }
        }

        // Primary blocks have no inner byte-string wrapper — the CBOR
        // array IS the block — and must be definite-length. §4.1's
        // indefinite-length carveout applies only to the outer bundle
        // array, not to individual blocks. `parse_canonical` above
        // enforces this via its `!s` check: an indefinite-length primary
        // array returns `s = false` from `from_cbor`, which `parse_canonical`
        // turns into `NotCanonical`. So `data` always spans the full
        // canonical extent — what BPSec hashes for primary AAD
        // (RFC 9173 §3.7 / §4.5) and what Builder/Editor emit via `as_block`.
        self.bundle = Some(Bundle {
            blocks: [(
                0,
                Block {
                    block_type: BlockType::Primary,
                    flags: BlockFlags::primary(),
                    crc_type: primary.crc_type,
                    bib: BibCoverage::None,
                    bcb: None,
                    extent: block_start as u64..offset as u64,
                    data: 0..(offset - block_start) as u64,
                },
            )]
            .into(),
            primary,
        });
        self.state = State::Blocks(offset);
        self.parse_blocks(data, offset)
    }

    fn parse_blocks(&mut self, data: &[u8], mut offset: usize) -> Result<usize> {
        let bundle = self
            .bundle
            .as_mut()
            .expect("parse_blocks called without a bundle");

        loop {
            // RFC 9171 §4.1: the outer indefinite array terminates with
            // `0xFF`. Encountering it here means the bundle had no payload
            // block — surface a clean `MissingPayload` rather than letting
            // `parse_canonical` fail trying to parse `0xFF` as a block
            // array head.
            match data.get(offset) {
                Some(&0xFF) => return Err(Error::MissingPayload),
                None => return Err(Error::InvalidCBOR(CborError::NeedMoreData(1))),
                Some(_) => {}
            }

            let block_start = offset;
            let header: BlockHeader = parse_canonical(data, &mut offset, "block")?;

            // RFC 9171 §4.4.1-3: PreviousNode, BundleAge, HopCount MUST
            // each appear at most once per bundle. (Payload uniqueness
            // is enforced indirectly: it must be block number 1, so a
            // second payload would trip `DuplicateBlockNumber` below.)
            // Checked here but recorded only after the block's last
            // fallible step: a `NeedMore` below re-parses this block from
            // `block_start` on the next push, so bookkeeping written before
            // that point would read as a duplicate on the retry.
            match header.block_type {
                BlockType::PreviousNode | BlockType::BundleAge | BlockType::HopCount
                    if self.unique_blocks.contains(&header.block_type) =>
                {
                    return Err(Error::DuplicateBlocks(header.block_type));
                }
                _ => {}
            }
            if bundle.blocks.contains_key(&header.number) {
                return Err(Error::DuplicateBlockNumber(header.number));
            }

            // RFC 9171 §4.2.3-4 / §4.2.3-5: an admin-record or null-source
            // bundle MUST NOT have the `report_on_failure` flag set on any
            // extension block.
            if (bundle.primary.flags.is_admin_record || bundle.primary.id.source.is_null())
                && header.flags.report_on_failure
            {
                return Err(Error::InvalidFlags);
            }

            // offset now sits at the start of the byte-string body. Per
            // §4.3.2 the byte string is definite-length, so the body end
            // is known from the header — and so is the post-trailer end
            // (CRC head byte + value bytes per §4.2.2, plus a possible
            // 0xFF break for indefinite block arrays).
            let block_start_u64 = block_start as u64;
            let body_end = block_start_u64
                .checked_add(header.data_end)
                .ok_or(Error::InvalidCBOR(CborError::TooBig))?;
            let trailer_len = trailer_byte_len(header.crc_type, header.is_indefinite);
            let extent_end = body_end
                .checked_add(trailer_len as u64)
                .ok_or(Error::InvalidCBOR(CborError::TooBig))?;

            let is_payload = matches!(header.block_type, BlockType::Payload);
            if (data.len() as u64) < body_end {
                // Body doesn't fit in the buffer yet. Keep the shortfall in
                // u64: the streaming-fallback path below must stay reachable on
                // 32-bit for a payload whose missing byte count exceeds usize
                // (`Block::extent` is u64 precisely so streamed bundles need
                // not fit in usize), so the `usize` conversion is deferred into
                // the two `NeedMoreData` returns that actually need it.
                let shortfall = body_end - data.len() as u64;

                if is_payload {
                    // For payloads, "small wait" vs "streaming fallback":
                    // the trailer is tiny, so if the remaining chunk
                    // capacity covers body + trailer, we prefer to wait
                    // one more chunk over falling back to streaming.
                    // Compare in u64: on 32-bit a wire-derived shortfall of
                    // usize::MAX would overflow the usize add and wrap the
                    // threshold test onto the small-wait path.
                    let needed = shortfall + trailer_len as u64;
                    if needed <= self.chunk_size.saturating_sub(offset) as u64 {
                        // The threshold bounds `shortfall` below `chunk_size`
                        // (a usize), so this conversion is infallible; the
                        // hint is only a lower bound `push` re-clamps anyway.
                        let hint = usize::try_from(shortfall).unwrap_or(usize::MAX);
                        return Err(Error::InvalidCBOR(CborError::NeedMoreData(hint)));
                    }
                    // Body too big to inline — streaming fallback for the
                    // payload. `offset` stays at the post-header position so
                    // the BPA's spool picks up from there. `extent.end` is
                    // still known (computed above) — only the CRC over the
                    // body is deferred.
                } else {
                    // Extension blocks must fit fully in the buffer; a shortfall
                    // beyond usize is unrepresentable (and unaddressable) here.
                    let shortfall_usize =
                        usize::try_from(shortfall).map_err(|_| CborError::TooBig)?;
                    return Err(Error::InvalidCBOR(CborError::NeedMoreData(shortfall_usize)));
                }
            } else {
                // Body is in the buffer. Consume CRC + trailing break and
                // verify the CRC. Any NeedMoreData from here propagates
                // normally — we want the small wait.
                offset = usize::try_from(body_end)
                    .expect("in-buffer block bodies are bounded by the in-memory buffer");
                let (new_offset, crc_value_start) = try_consume_block_after_body(
                    data,
                    offset,
                    header.crc_type,
                    header.is_indefinite,
                )?;
                offset = new_offset;
                debug_assert_eq!(offset as u64, extent_end);
                if let Some(crc_value_start) = crc_value_start {
                    let mut digest = crc::Digest::new(header.crc_type)?;
                    digest.push(&data[block_start..crc_value_start]);
                    let crc_value_end = digest.push_zeros() + crc_value_start;
                    digest.push(&data[crc_value_end..offset]);
                    // consume_crc above already enforced the exact
                    // value length, so no length pre-check is needed.
                    if digest.finalize() != data[crc_value_start..crc_value_end] {
                        return Err(crc::Error::IncorrectCrc.into());
                    }
                }
            }

            // The payload block's terminal steps are fallible, so run them
            // before the bookkeeping below (retry safety, as above).
            let mut payload_tail = None;
            if is_payload {
                if offset as u64 == extent_end {
                    // Inline payload (body fit in the buffer): consume the
                    // outer indefinite-array `0xFF` break and reject any
                    // trailing data after it. Bundle is complete.
                    match data.get(offset) {
                        Some(&0xFF) => offset += 1,
                        Some(_) => return Err(Error::NotCanonical),
                        None => return Err(Error::InvalidCBOR(CborError::NeedMoreData(1))),
                    }
                    if offset != data.len() {
                        return Err(Error::AdditionalData);
                    }
                } else {
                    // Streaming-fallback fired above: the payload body exceeds
                    // the buffer, so `offset` still sits at the post-header
                    // position and the body, trailer, and outer break have not
                    // arrived. The payload block's `extent` over-claims (its
                    // `end` lies beyond the buffer). Build the CRC continuation
                    // pre-fed with the header + body prefix already in `data`
                    // (`Digest::new` also rejects an unrecognised CRC type here,
                    // matching the body-fits path); `push` hands it to the
                    // caller as `ParserProgress::Partial` to drain the tail.
                    let digest = match header.crc_type {
                        crc::CrcType::None => None,
                        _ => {
                            let mut digest = crc::Digest::new(header.crc_type)?;
                            digest.push(&data[block_start..data.len()]);
                            Some(digest)
                        }
                    };
                    let body_remaining = body_end - data.len() as u64;
                    let remaining = extent_end
                        .saturating_add(1)
                        .saturating_sub(data.len() as u64);
                    payload_tail = Some(PayloadTail::new(
                        digest,
                        header.crc_type,
                        header.is_indefinite,
                        body_remaining,
                        remaining,
                    ));
                }
            }

            // BPSec block handling. Non-payload blocks always reach
            // the body-fits branch above, so `data` covers the block-
            // type-specific data byte string body in full.
            //
            // BCBs are decoded inline: BCB bodies are always plaintext
            // (the OperationSet describes what the BCB encrypts; the
            // OperationSet itself is never encrypted).
            //
            // BIBs are deferred: a BIB may itself be the target of a
            // BCB, in which case its body is ciphertext and decoding
            // it as an OperationSet here would fail (or produce garbage
            // on a chance CBOR shape match). We stash the body range
            // and let `finish()` decide after BCBs have been processed
            // and BCB-coverage on each block is known.
            let bcb_ops = match header.block_type {
                BlockType::BlockSecurity => {
                    // A BCB is a non-payload block: its body is wholly
                    // inside the staged buffer, so the offsets fit the
                    // address space by construction.
                    let mut o = block_start
                        + usize::try_from(header.data_start).expect(
                            "non-payload block offsets are bounded by the in-memory buffer",
                        );
                    let body_end = block_start
                        + usize::try_from(header.data_end).expect(
                            "non-payload block offsets are bounded by the in-memory buffer",
                        );
                    // See the BIB call in `finish()` for the slice-bound
                    // rationale — `parse_sequence` requires consuming the
                    // whole input.
                    Some(parse_canonical::<bpsec::bcb::OperationSet>(
                        &data[..body_end],
                        &mut o,
                        "BCB operation set",
                    )?)
                }
                _ => None,
            };

            // Past every fallible step for this block — record it.
            match header.block_type {
                BlockType::PreviousNode | BlockType::BundleAge | BlockType::HopCount => {
                    self.unique_blocks.insert(header.block_type);
                }
                BlockType::BlockIntegrity => {
                    // Body range is recoverable from bundle.blocks[n]
                    // (extent + data) at finalize time — no need to
                    // duplicate it here.
                    self.pending_bibs.push(header.number);
                }
                BlockType::BlockSecurity => {
                    self.bcbs.insert(
                        header.number,
                        bcb_ops.expect("BlockSecurity always decodes ops above"),
                    );
                }
                _ => {}
            }
            bundle.blocks.insert(
                header.number,
                Block {
                    block_type: header.block_type,
                    flags: header.flags,
                    crc_type: header.crc_type,
                    bib: BibCoverage::None,
                    bcb: None,
                    extent: block_start_u64..extent_end,
                    data: header.data_start..header.data_end,
                },
            );

            if is_payload {
                match payload_tail {
                    None => self.state = State::Done,
                    Some(tail) => {
                        self.deferred = Some(tail);
                        self.state = State::Partial;
                    }
                }
                return Ok(offset);
            }
            self.state = State::Blocks(offset);
        }
    }
}

/// Extract a `NeedMoreData` shortfall from an error, seeing through the
/// `InvalidField` field-label chain that wrap-time conversion builds around
/// errors from nested field parses. `NeedMoreData` always means "the input
/// is truncated here", never "this is malformed", so any occurrence in the
/// chain is a genuine need-more signal for the streaming `push` loop.
/// Returns the innermost shortfall (a lower-bound hint for buffer
/// reservation).
///
/// Each domain gets its own recursive helper, and every arm that is not a
/// wrapper or a shortfall falls through to `None`. A newly wrapped domain
/// therefore needs an arm added here by hand: without one it reads as "not
/// a shortfall", and the streaming `push` loop reports the truncation as
/// malformed input instead of asking for more.
fn need_more(e: &Error) -> Option<usize> {
    match e {
        Error::InvalidCBOR(e) => cbor_need_more(e),
        Error::InvalidEid(e) => eid_need_more(e),
        Error::InvalidBPSec(e) => bpsec_need_more(e),
        Error::InvalidField { source, .. } => need_more(source),
        _ => None,
    }
}

fn cbor_need_more(e: &CborError) -> Option<usize> {
    match e {
        CborError::NeedMoreData(more) => Some(*more),
        _ => None,
    }
}

fn eid_need_more(e: &eid::Error) -> Option<usize> {
    match e {
        eid::Error::InvalidCBOR(e) => cbor_need_more(e),
        eid::Error::InvalidField { source, .. } => eid_need_more(source),
        _ => None,
    }
}

fn bpsec_need_more(e: &bpsec::Error) -> Option<usize> {
    match e {
        bpsec::Error::InvalidCBOR(e) => cbor_need_more(e),
        bpsec::Error::InvalidEid(e) => eid_need_more(e),
        bpsec::Error::InvalidField { source, .. } => bpsec_need_more(source),
        _ => None,
    }
}

/// Parses a `T` from `data` starting at `*offset`, advancing `*offset`
/// by the bytes consumed (TooBig on overflow). Fails with `NotCanonical`
/// if the encoding isn't shortest-form; labels both that failure and any
/// underlying parse error with `field` (so the diagnostic carries the
/// field name in either case). A non-shortest encoding is rejected
/// outright as `NotCanonical`.
/// Decodes through [`Untagged`], so a tag run in front of the item is
/// rejected from its first byte without being read, surfacing as a
/// field-labelled `NotCanonical` (the `Error::from` conversion below is
/// what translates the cbor-level rejection into the domain error). The
/// one grammar position where a tag is legal (`#6.24` on block data) has
/// its own hand-rolled guard in `BlockHeader::from_cbor` instead.
fn parse_canonical<T>(data: &[u8], offset: &mut usize, field: &'static str) -> Result<T>
where
    T: hardy_cbor::decode::FromCbor,
    <T as hardy_cbor::decode::FromCbor>::Error: From<CborError>,
    Error: From<<T as hardy_cbor::decode::FromCbor>::Error>,
{
    let (Untagged(v), s, l): (Untagged<T>, bool, usize) =
        hardy_cbor::decode::parse(&data[*offset..])
            .map_err(Error::from)
            .map_field_err::<Error>(field)?;
    if !s {
        return Err(Error::InvalidField {
            field,
            source: Box::new(Error::NotCanonical),
        });
    }
    *offset = offset
        .checked_add(l)
        .ok_or(Error::InvalidCBOR(CborError::TooBig))?;
    Ok(v)
}

/// One-shot bundle parse: feeds `data` to a fresh [`BundleParser`],
/// asserts it parsed to completion (otherwise the input was truncated
/// — returned as `InvalidCBOR(NeedMoreData)`), then returns the
/// finalised [`Parsed`].
///
/// For streamed input arriving in pieces, drive a [`BundleParser`]
/// directly via [`BundleParser::push`] until it yields
/// [`ParserProgress::Ready`] (complete) or [`ParserProgress::Partial`]
/// (oversized payload — the caller drains the body tail).
pub fn parse(data: Bytes) -> Result<Parsed> {
    let mut parser = BundleParser::default();
    let data = match parser.push(data)? {
        ParserProgress::NeedMore(more) => {
            return Err(Error::InvalidCBOR(CborError::NeedMoreData(more)));
        }
        // A one-shot buffer that triggers the streaming fallback is, by
        // definition, a truncated oversized payload (a complete one fits and
        // takes the body-fits path). One-shot `parse` deals only in complete
        // buffers, so surface it as truncation.
        ParserProgress::Partial { tail, .. } => {
            return Err(Error::InvalidCBOR(CborError::NeedMoreData(
                usize::try_from(tail.remaining()).unwrap_or(usize::MAX),
            )));
        }
        ParserProgress::Ready(data) => data,
    };

    let parsed = parser.finish(data)?;

    // For one-shot parse(), enforce that the bundle is complete. The inline
    // payload path (small payload) already consumed the outer 0xFF and checked
    // for trailing data before returning Ok. Only the streaming-fallback path
    // (large payload body that didn't fit in the buffer) can reach here without
    // having done so — that path is designed for the multi-push BPA use case,
    // not one-shot use.
    //
    // `extent.end` is a u64 derived from an attacker-controlled byte-string
    // length: it can exceed both the buffer and `usize` (on 32-bit targets).
    // Do the buffer-bounds test in u64 space *before* any cast — casting first
    // would truncate the high bits on 32-bit and could let a huge declared
    // payload alias onto an in-bounds index, falsely accepting a truncated
    // bundle. If the payload ends at or past the buffer end, the outer break
    // can't be present, so the bundle was truncated.
    let payload_end = parsed
        .bundle
        .blocks
        .get(&1)
        .ok_or(Error::MissingPayload)?
        .extent
        .end;
    let data_len = parsed.data.len() as u64;
    if payload_end >= data_len {
        let needed = payload_end.saturating_add(1).saturating_sub(data_len);
        return Err(Error::InvalidCBOR(CborError::NeedMoreData(
            usize::try_from(needed).unwrap_or(usize::MAX),
        )));
    }

    // payload_end < data_len <= usize::MAX, so the cast is lossless and the
    // index is in bounds. The outer bundle array is always indefinite-length
    // (parse_start enforces 0x9F as the only legal first byte), so in a
    // complete, valid bundle the outer 0xFF break sits at payload_end with
    // nothing after it. Checking the actual byte there is more robust than the
    // old `data.len() - 1` arithmetic: if the §4.1 MAY-accept carve-out for
    // definite-length outer arrays is ever implemented, this must be revisited.
    let payload_end = payload_end as usize;
    if parsed.data[payload_end] != 0xFF || payload_end + 1 != parsed.data.len() {
        return Err(Error::AdditionalData);
    }

    Ok(parsed)
}
