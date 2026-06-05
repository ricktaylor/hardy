/*!
The streaming wire parser for BPv7 bundles ([RFC 9171]). [`BundleParser`]
drives the structural decode incrementally; [`parse`] is the one-shot
convenience over it. Both yield a [`Parsed`] — the authoritative byte
buffer, the structural [`Bundle`](crate::bundle::Bundle), and the decoded
BPSec OperationSets. Keyed BPSec validation is layered on top by composing
[`crate::checks`] and [`crate::rewrite`].

[RFC 9171]: https://www.rfc-editor.org/rfc/rfc9171.html
*/

use super::*;
use bytes::{Bytes, BytesMut};
use error::CaptureFieldErr;
use hardy_cbor::decode::{Error as CborError, Head, Marker};
use primary_block::PrimaryBlock;
use smallvec::SmallVec;

struct BlockHeader {
    /// `true` if the block array uses indefinite-length encoding (a trailing
    /// `0xFF` break byte must be consumed after the CRC). `false` for the
    /// definite-length forms (`0x85` = 5 items / no CRC, `0x86` = 6 items /
    /// with CRC), whose item count is validated against `crc_type` by
    /// `from_cbor`.
    is_indefinite: bool,
    number: u64,
    block_type: block::Type,
    flags: block::Flags,
    crc_type: crc::CrcType,
    // Block payload relative to the start of the block
    data_start: u64,
    data_end: u64,
}

impl hardy_cbor::decode::FromCbor for BlockHeader {
    type Error = Error;

    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        // Block array head — RFC 9171 §4.3.2: SHALL be a CBOR array with
        // 5 items (no CRC) or 6 items (with CRC); §4.1 carve-out permits
        // indefinite-length. The three legal head bytes are 0x85, 0x86, 0x9F.
        // Anything else is either a non-shortest definite-length form
        // (NotCanonical) or not an array at all.
        let (is_indefinite, mut offset) = match data.first() {
            Some(&0x85) => (false, 1),
            Some(&0x86) => (false, 1),
            Some(&0x9F) => (true, 1),
            Some(_) => return Err(slow_block_array_error(data)),
            None => return Err(Error::InvalidCBOR(CborError::NeedMoreData(1))),
        };
        let expected_count = data[0];

        let block_type: block::Type = parse_canonical(data, &mut offset, "block type")?;

        let block_number: u64 = parse_canonical(data, &mut offset, "block number")?;
        match (block_number, block_type) {
            (1, block::Type::Payload) => {}
            (0 | 1, _) | (_, block::Type::Primary | block::Type::Payload) => {
                return Err(Error::InvalidBlockNumber(block_number, block_type));
            }
            _ => {}
        }

        let flags: block::Flags = parse_canonical(data, &mut offset, "block flags")?;

        let crc_type: crc::CrcType = parse_canonical(data, &mut offset, "block crc type")?;

        // Definite-length array item count must agree with crc_type.
        // (Indefinite arrays carry no count; the trailing-break consumer
        // enforces termination separately.)
        match (expected_count, crc_type) {
            (0x85, crc::CrcType::None) => {}
            (0x86, t) if !matches!(t, crc::CrcType::None) => {}
            (0x9F, _) => {}
            _ => return Err(Error::InvalidCBOR(CborError::AdditionalItems)),
        }

        // Block-type-specific data byte string head. RFC 9171 §4.3.2:
        // MUST be a single definite-length CBOR byte string. Appendix B
        // permits an optional `#6.24` tag (CBOR-embedded content); no
        // other tags are allowed.
        let marker: Head = parse_canonical(data, &mut offset, "block data")?;
        if !matches!(marker.tags.as_slice(), [] | [24]) {
            return Err(Error::NotCanonical);
        }
        let data_end = match marker.marker {
            Marker::Bytes(Some(len)) => len.checked_add(offset as u64).ok_or(CborError::TooBig)?,
            Marker::Bytes(None) => return Err(Error::NotCanonical),
            _ => {
                return Err(Error::InvalidCBOR(CborError::IncorrectType(
                    "Definite-length Byte String".to_string(),
                    marker.to_string(),
                )))
                .map_field_err::<Error>("block data");
            }
        };

        Ok((
            Self {
                is_indefinite,
                number: block_number,
                block_type,
                flags,
                crc_type,
                data_start: offset as u64,
                data_end,
            },
            true,
            offset,
        ))
    }
}

/// A successfully-parsed [`Bundle`] paired with the authoritative
/// `Bytes` buffer the parser owns (the source of truth that the
/// returned `Bundle::blocks` extent/data offsets index into) and its
/// decoded BPSec OperationSet maps (BCBs and BIBs, keyed by block
/// number). Returned by [`BundleParser::finish`] and [`parse`].
///
/// Slice with `&parsed.data[block.payload_range()]` (or `block.extent`)
/// using [`data`](Self::data) rather than a separate copy of the input —
/// for the single-`push()` / one-shot [`parse`] path this is the input
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

pub enum ParserProgress {
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
    Partial {
        consumed: Bytes,
        tail: PayloadTail,
    },
}

/// Synchronous continuation that carries an oversized payload block's CRC and
/// termination checks across the streamed tail. Handed back in
/// [`ParserProgress::Partial`], already fed the block header and the body
/// prefix that were in `consumed`. The caller pushes each subsequent run of
/// bytes through [`push`](Self::push); the continuation feeds the running CRC
/// (entering the CRC-value field as zeros per RFC 9171 §4.2.2), validates the
/// block-level and outer `0xFF` breaks, verifies the CRC, and reports when the
/// bundle is complete. It performs no I/O and owns no storage — persisting the
/// drained bytes is the caller's job.
pub struct PayloadTail {
    /// `None` when the payload block declared no CRC; otherwise pre-fed the
    /// block header + body prefix, and consumed by `verify_crc`.
    digest: Option<crc::Digest>,
    crc_type: crc::CrcType,
    is_indefinite: bool,
    phase: TailPhase,
    /// Body bytes not yet seen (decrements through the `Body` phase).
    body_remaining: u64,
    /// Total bytes still expected, through the outer `0xFF` break.
    remaining: u64,
    /// Captured wire CRC value bytes (`crc_value[..crc_value_len]`).
    crc_value: [u8; 4],
    crc_filled: usize,
}

/// Where in the post-`consumed` byte stream a [`PayloadTail`] currently is.
enum TailPhase {
    /// Consuming the rest of the payload body (fed to the digest).
    Body,
    /// Expecting the 1-byte CRC byte-string head (`0x42`/`0x44`).
    CrcHead,
    /// Capturing the CRC value bytes (not fed to the digest — zeros were).
    CrcValue,
    /// Expecting the block array's `0xFF` break (indefinite-length blocks only).
    BlockBreak,
    /// Expecting the bundle's outer `0xFF` break.
    OuterBreak,
    /// Bundle complete; any further bytes are trailing data.
    Done,
}

/// Wire width of the CRC value for `crc_type` (0 if none).
fn crc_value_len(crc_type: crc::CrcType) -> usize {
    match crc_type {
        crc::CrcType::CRC16_X25 => 2,
        crc::CrcType::CRC32_CASTAGNOLI => 4,
        _ => 0,
    }
}

/// CBOR byte-string head for the CRC value (`0x42` for CRC-16, `0x44` for
/// CRC-32). Only meaningful — and only consulted — when a CRC is present.
fn crc_head_byte(crc_type: crc::CrcType) -> u8 {
    match crc_type {
        crc::CrcType::CRC16_X25 => 0x42,
        crc::CrcType::CRC32_CASTAGNOLI => 0x44,
        _ => 0,
    }
}

/// The phase that follows the payload body, given the trailer shape.
fn after_body(crc_type: crc::CrcType, is_indefinite: bool) -> TailPhase {
    if !matches!(crc_type, crc::CrcType::None) {
        TailPhase::CrcHead
    } else if is_indefinite {
        TailPhase::BlockBreak
    } else {
        TailPhase::OuterBreak
    }
}

impl PayloadTail {
    fn new(
        digest: Option<crc::Digest>,
        crc_type: crc::CrcType,
        is_indefinite: bool,
        body_remaining: u64,
        remaining: u64,
    ) -> Self {
        let phase = if body_remaining > 0 {
            TailPhase::Body
        } else {
            after_body(crc_type, is_indefinite)
        };
        Self {
            digest,
            crc_type,
            is_indefinite,
            phase,
            body_remaining,
            remaining,
            crc_value: [0; 4],
            crc_filled: 0,
        }
    }

    /// Bytes still expected before the bundle is complete (through the outer
    /// `0xFF` break).
    pub fn remaining(&self) -> u64 {
        self.remaining
    }

    /// Feed the next run of streamed bytes. Returns `true` once the bundle is
    /// complete (body drained, CRC verified, breaks consumed). Errors on a CRC
    /// mismatch ([`crc::Error::IncorrectCrc`]), a malformed trailer
    /// ([`Error::NotCanonical`]), or bytes after the outer break
    /// ([`Error::AdditionalData`]). On `Ok`, the whole run belonged to the
    /// bundle and should be persisted by the caller.
    pub fn push(&mut self, mut bytes: &[u8]) -> Result<bool, Error> {
        let start = bytes.len();
        while let Some(&b) = bytes.first() {
            match self.phase {
                TailPhase::Body => {
                    // `Body` is only entered with `body_remaining > 0`, so this
                    // consumes at least one byte and makes progress.
                    let take = self.body_remaining.min(bytes.len() as u64) as usize;
                    if let Some(d) = self.digest.as_mut() {
                        d.push(&bytes[..take]);
                    }
                    self.body_remaining -= take as u64;
                    bytes = &bytes[take..];
                    if self.body_remaining == 0 {
                        self.enter_after_body()?;
                    }
                }
                TailPhase::CrcHead => {
                    if b != crc_head_byte(self.crc_type) {
                        return Err(Error::NotCanonical);
                    }
                    if let Some(d) = self.digest.as_mut() {
                        // The head byte is CRC input; the value field that
                        // follows is hashed as zeros (RFC 9171 §4.2.2).
                        d.push(&[b]);
                        d.push_zeros();
                    }
                    bytes = &bytes[1..];
                    self.phase = TailPhase::CrcValue;
                }
                TailPhase::CrcValue => {
                    let want = crc_value_len(self.crc_type) - self.crc_filled;
                    let take = want.min(bytes.len());
                    self.crc_value[self.crc_filled..self.crc_filled + take]
                        .copy_from_slice(&bytes[..take]);
                    self.crc_filled += take;
                    bytes = &bytes[take..];
                    if self.crc_filled == crc_value_len(self.crc_type) {
                        if self.is_indefinite {
                            self.phase = TailPhase::BlockBreak;
                        } else {
                            self.verify_crc()?;
                            self.phase = TailPhase::OuterBreak;
                        }
                    }
                }
                TailPhase::BlockBreak => {
                    if b != 0xFF {
                        return Err(Error::NotCanonical);
                    }
                    if let Some(d) = self.digest.as_mut() {
                        d.push(&[0xFF]);
                    }
                    bytes = &bytes[1..];
                    self.verify_crc()?;
                    self.phase = TailPhase::OuterBreak;
                }
                TailPhase::OuterBreak => {
                    if b != 0xFF {
                        return Err(Error::NotCanonical);
                    }
                    bytes = &bytes[1..];
                    self.phase = TailPhase::Done;
                }
                TailPhase::Done => return Err(Error::AdditionalData),
            }
        }
        self.remaining = self.remaining.saturating_sub((start - bytes.len()) as u64);
        Ok(matches!(self.phase, TailPhase::Done))
    }

    /// Assert the bundle completed. Errors with `NeedMoreData` (the still-
    /// outstanding count) if the stream ended before the outer break — i.e. the
    /// bundle was truncated.
    pub fn finish(self) -> Result<(), Error> {
        if matches!(self.phase, TailPhase::Done) {
            Ok(())
        } else {
            Err(Error::InvalidCBOR(CborError::NeedMoreData(
                usize::try_from(self.remaining).unwrap_or(usize::MAX),
            )))
        }
    }

    /// Transition out of the `Body` phase, running the CRC verification eagerly
    /// when the next thing expected is the outer break (no CRC / no block break
    /// between here and it).
    fn enter_after_body(&mut self) -> Result<(), Error> {
        self.phase = after_body(self.crc_type, self.is_indefinite);
        if matches!(self.phase, TailPhase::OuterBreak) {
            self.verify_crc()?;
        }
        Ok(())
    }

    /// Compare the accumulated digest against the captured wire value. A no-op
    /// when the block declared no CRC. Consumes the digest so it runs once.
    fn verify_crc(&mut self) -> Result<(), Error> {
        if let Some(digest) = self.digest.take()
            && !digest.verify(&self.crc_value[..crc_value_len(self.crc_type)])
        {
            return Err(crc::Error::IncorrectCrc.into());
        }
        Ok(())
    }
}

pub struct BundleParser {
    chunk_size: usize,
    data: Option<BytesMut>,
    state: State,
    bundle: Option<Bundle>,

    /// RFC 9171 §4.4: PreviousNode, BundleAge, and HopCount blocks
    /// MUST appear at most once per bundle. Tracked as a small set
    /// during the streaming walk; `insert` returning `false` is the
    /// duplicate detection.
    unique_blocks: HashSet<block::Type>,

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

    pub fn push(&mut self, data_in: Bytes) -> Result<ParserProgress, Error> {
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
                    buf.reserve(more);
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
    pub fn finish(mut self, data: Bytes) -> Result<Parsed, Error> {
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
    /// the keyless rules from RFC 9172 §3.7 and §3.9. Mirrors the
    /// older parser's BCB→BIB ordering: BCBs first so each block's
    /// BCB-coverage is known, then BIBs (skipping any that are
    /// BCB-encrypted — their bodies are ciphertext and we can't
    /// decode the OperationSet without keys). BCB-protected BIBs
    /// trigger a `BibCoverage::Maybe` sweep on the remaining blocks,
    /// matching the older parser's `mark_bib_coverage_unknown`.
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
    ) -> Result<HashMap<u64, bpsec::bib::OperationSet>, Error> {
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
            let body_end = body.end as usize;
            let mut o = body.start as usize;
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
                    .bib = block::BibCoverage::Some(bib_block_number);
            }

            bibs.insert(bib_block_number, ops);
        }

        // Encrypted BIBs whose targets we couldn't read: every non-
        // security block whose BIB coverage is still `None` becomes
        // `Maybe`. Mirrors the older parser's `mark_bib_coverage_unknown`.
        if has_undecryptable_bibs {
            for block in bundle.blocks.values_mut() {
                if !matches!(
                    block.block_type,
                    block::Type::BlockIntegrity | block::Type::BlockSecurity
                ) && matches!(block.bib, block::BibCoverage::None)
                {
                    block.bib = block::BibCoverage::Maybe;
                }
            }
        }

        Ok(bibs)
    }

    fn parse_start(&mut self, data: &[u8]) -> Result<usize, Error> {
        // Bundle outer array head. RFC 9171 §4.1: a bundle SHALL be
        // represented as a CBOR *indefinite-length* array, so the only
        // conformant first byte is 0x9F. A definite-length outer array is
        // therefore non-conformant — and §4.1 explicitly lets an
        // implementation "MAY discard any sequence of bytes that does not
        // conform", which is what we do (slow_bundle_array_error maps the
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
        let offset = match data.first() {
            Some(&0x9F) => 1,
            None => return Err(Error::InvalidCBOR(CborError::NeedMoreData(1))),
            Some(_) => return Err(slow_bundle_array_error(data)),
        };
        self.state = State::PrimaryBlock(offset);
        self.parse_primary(data, offset)
    }

    fn parse_primary(&mut self, data: &[u8], mut offset: usize) -> Result<usize, Error> {
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
        // array IS the block — and must be definite-length
        // (`PrimaryBlock::from_cbor` rejects indefinite via
        // `block.is_definite()`, which `parse_canonical` above turns
        // into `NotCanonical`). §4.1's indefinite-length carveout
        // applies only to the outer bundle array, not to individual
        // blocks. So `data` always spans the full canonical extent —
        // what BPSec hashes for primary AAD (RFC 9173 §3.7 / §4.5) and
        // what Builder/Editor emit via `as_block`.
        self.bundle = Some(Bundle {
            blocks: [(
                0,
                block::Block {
                    block_type: block::Type::Primary,
                    flags: block::Flags::primary(),
                    crc_type: primary.crc_type,
                    bib: block::BibCoverage::None,
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

    fn parse_blocks(&mut self, data: &[u8], mut offset: usize) -> Result<usize, Error> {
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
            match header.block_type {
                block::Type::PreviousNode | block::Type::BundleAge | block::Type::HopCount
                    if !self.unique_blocks.insert(header.block_type) =>
                {
                    return Err(Error::DuplicateBlocks(header.block_type));
                }
                _ => {}
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

            let is_payload = matches!(header.block_type, block::Type::Payload);
            if (data.len() as u64) < body_end {
                // Body doesn't fit in the buffer yet.
                let shortfall = body_end - data.len() as u64;
                let shortfall_usize = usize::try_from(shortfall).map_err(|_| CborError::TooBig)?;

                if is_payload {
                    // For payloads, "small wait" vs "streaming fallback":
                    // the trailer is tiny, so if the remaining chunk
                    // capacity covers body + trailer, we prefer to wait
                    // one more chunk over falling back to streaming.
                    let needed = shortfall_usize + trailer_len;
                    if needed <= self.chunk_size.saturating_sub(offset) {
                        return Err(Error::InvalidCBOR(CborError::NeedMoreData(shortfall_usize)));
                    }
                    // Body too big to inline — streaming fallback for the
                    // payload. `offset` stays at the post-header position so
                    // the BPA's spool picks up from there. `extent.end` is
                    // still known (computed above) — only the CRC over the
                    // body is deferred.
                } else {
                    // Extension blocks must fit fully in the buffer; bubble
                    // up the exact shortfall and wait for more bytes.
                    return Err(Error::InvalidCBOR(CborError::NeedMoreData(shortfall_usize)));
                }
            } else {
                // Body is in the buffer. Consume CRC + trailing break and
                // verify the CRC. Any NeedMoreData from here propagates
                // normally — we want the small wait.
                offset = body_end as usize;
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

            if bundle
                .blocks
                .insert(
                    header.number,
                    block::Block {
                        block_type: header.block_type,
                        flags: header.flags,
                        crc_type: header.crc_type,
                        bib: block::BibCoverage::None,
                        bcb: None,
                        extent: block_start_u64..extent_end,
                        data: header.data_start..header.data_end,
                    },
                )
                .is_some()
            {
                return Err(Error::DuplicateBlockNumber(header.number));
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
            match header.block_type {
                block::Type::BlockIntegrity => {
                    // Body range is recoverable from bundle.blocks[n]
                    // (extent + data) at finalize time — no need to
                    // duplicate it here.
                    self.pending_bibs.push(header.number);
                }
                block::Type::BlockSecurity => {
                    let mut o = block_start + header.data_start as usize;
                    let body_end = block_start + header.data_end as usize;
                    // See the BIB call in `finish()` for the slice-bound
                    // rationale — `parse_sequence` requires consuming the
                    // whole input.
                    let ops: bpsec::bcb::OperationSet =
                        parse_canonical(&data[..body_end], &mut o, "BCB operation set")?;
                    self.bcbs.insert(header.number, ops);
                }
                _ => {}
            }

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
                    self.state = State::Done;
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
                    self.deferred = Some(PayloadTail::new(
                        digest,
                        header.crc_type,
                        header.is_indefinite,
                        body_remaining,
                        remaining,
                    ));
                    self.state = State::Partial;
                }
                return Ok(offset);
            }
            self.state = State::Blocks(offset);
        }
    }
}

/// Extract a `NeedMoreData` shortfall from an error, seeing through the
/// `InvalidField` field-label chain that `parse_canonical` wraps around errors
/// from nested field parses. `NeedMoreData` always means "the input is
/// truncated here", never "this is malformed", so any occurrence in the chain
/// is a genuine need-more signal for the streaming `push` loop. Returns the
/// innermost shortfall (a lower-bound hint for buffer reservation).
fn need_more(e: &Error) -> Option<usize> {
    match e {
        Error::InvalidCBOR(CborError::NeedMoreData(more)) => Some(*more),
        Error::InvalidField { source, .. } => source.downcast_ref::<Error>().and_then(need_more),
        _ => None,
    }
}

/// Wire-form length of a block's post-body trailer: optional CRC byte
/// string (head byte 0x42 / 0x44 + 2 / 4 value bytes, §4.2.2) and an
/// optional `0xFF` break if the block array used indefinite-length
/// encoding.
fn trailer_byte_len(crc_type: crc::CrcType, is_indefinite_array: bool) -> usize {
    let crc_len = match crc_type {
        crc::CrcType::None => 0,
        crc::CrcType::CRC16_X25 => 3,
        crc::CrcType::CRC32_CASTAGNOLI => 5,
        crc::CrcType::Unrecognised(_) => 0,
    };
    crc_len + if is_indefinite_array { 1 } else { 0 }
}

/// Consume the bytes after a block's body: the CRC byte string (if
/// declared) and any trailing `0xFF` break for indefinite-length block
/// arrays. Strict canonical layout per §4.2.2 / §4.3.2 makes the CRC
/// head byte exact (`0x42` for CRC-16, `0x44` for CRC-32) and the
/// value length fixed (2 or 4), so this reduces to byte matching plus
/// arithmetic. Returns the new cursor position plus the absolute start
/// offset of the CRC value bytes (if any), for the caller to run
/// `Digest::finalize` over the now-known full block extent.
fn try_consume_block_after_body(
    data: &[u8],
    mut offset: usize,
    crc_type: crc::CrcType,
    is_indefinite_array: bool,
) -> Result<(usize, Option<usize>), Error> {
    let crc_value_start = match crc_type {
        crc::CrcType::None => None,
        crc::CrcType::CRC16_X25 => Some(consume_crc(data, &mut offset, 0x42, 2)?),
        crc::CrcType::CRC32_CASTAGNOLI => Some(consume_crc(data, &mut offset, 0x44, 4)?),
        crc::CrcType::Unrecognised(t) => return Err(crc::Error::InvalidType(t).into()),
    };

    if is_indefinite_array {
        match data.get(offset) {
            Some(&0xFF) => offset += 1,
            Some(_) => return Err(Error::NotCanonical),
            None => return Err(Error::InvalidCBOR(CborError::NeedMoreData(1))),
        }
    }

    Ok((offset, crc_value_start))
}

/// Strict-shape CRC byte-string consumer: expects `head` (0x42 or 0x44)
/// at `data[*offset]`, followed by exactly `value_len` value bytes.
/// Returns the absolute offset of the first CRC value byte and advances
/// `*offset` past the CRC. `NeedMoreData` is reported for the exact
/// shortfall so the caller can wait one chunk; any other shape is a
/// canonical-encoding violation.
fn consume_crc(
    data: &[u8],
    offset: &mut usize,
    head: u8,
    value_len: usize,
) -> Result<usize, Error> {
    let needed = 1 + value_len;
    if data.len() < *offset + needed {
        return Err(Error::InvalidCBOR(CborError::NeedMoreData(
            *offset + needed - data.len(),
        )));
    }
    if data[*offset] != head {
        return Err(Error::NotCanonical);
    }
    let value_start = *offset + 1;
    *offset += needed;
    Ok(value_start)
}

/// Parses a `T` from `data` starting at `*offset`, advancing `*offset`
/// by the bytes consumed (TooBig on overflow). Fails with `NotCanonical`
/// if the encoding isn't shortest-form; labels both that failure and any
/// underlying parse error with `field` (so the diagnostic carries the
/// field name in either case). Strict-canonical counterpart of the old
/// `parse_checked` — the shortest indicator from FromCbor becomes a
/// yes/no gate rather than something to AND into a running flag.
fn parse_canonical<T>(data: &[u8], offset: &mut usize, field: &'static str) -> Result<T, Error>
where
    T: hardy_cbor::decode::FromCbor,
    <T as hardy_cbor::decode::FromCbor>::Error:
        From<CborError> + Into<Box<dyn core::error::Error + Send + Sync>>,
{
    let (v, s, l): (T, bool, usize) =
        hardy_cbor::decode::parse(&data[*offset..]).map_field_err::<Error>(field)?;
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

/// Sequence-aware counterpart of [`parse_canonical`] for parsing the
/// next item inside a nested CBOR array — the array tracks its own
/// cursor and adds array-boundary handling on top of the same parse +
/// shortest-form check + field-label-on-error machinery (including
/// labelling `NotCanonical` with the field name).
pub(super) fn parse_canonical_item<T>(
    block: &mut hardy_cbor::decode::Array<'_>,
    field: &'static str,
) -> Result<T, Error>
where
    T: hardy_cbor::decode::FromCbor,
    <T as hardy_cbor::decode::FromCbor>::Error:
        From<CborError> + Into<Box<dyn core::error::Error + Send + Sync>>,
{
    let (v, s): (T, bool) = block.parse().map_field_err::<Error>(field)?;
    if !s {
        return Err(Error::InvalidField {
            field,
            source: Box::new(Error::NotCanonical),
        });
    }
    Ok(v)
}

/// Cold path: the outer-array first byte wasn't `0x9F`. Re-parse via
/// `Head` to produce a high-quality diagnostic (definite-length
/// arrays = `NotCanonical`; an unsigned integer 6 = "looks like BPv6";
/// anything else = "this isn't a CBOR array head at all").
#[cold]
fn slow_bundle_array_error(data: &[u8]) -> Error {
    let parsed = hardy_cbor::decode::parse::<(Head, bool, usize)>(data);
    match parsed {
        Ok((marker, _, _)) => match marker.marker {
            Marker::Array(Some(_)) => Error::NotCanonical,
            Marker::UnsignedInteger(6) => Error::InvalidCBOR(CborError::IncorrectType(
                "BPv7 bundle".to_string(),
                "Possible BPv6 bundle".to_string(),
            )),
            _ => Error::InvalidCBOR(CborError::IncorrectType(
                "BPv7 bundle".to_string(),
                marker.to_string(),
            )),
        },
        Err(e) => Error::InvalidCBOR(e),
    }
}

/// Cold path: the block-array first byte wasn't `0x85`, `0x86`, or
/// `0x9F`. Re-parse via `Head` to distinguish "not an array"
/// from "definite-length array with the wrong item count" (the latter
/// is still a canonical violation, but we map it to `InvalidCBOR` for
/// diagnostic continuity with the underlying CBOR machinery).
#[cold]
fn slow_block_array_error(data: &[u8]) -> Error {
    let parsed = hardy_cbor::decode::parse::<(Head, bool, usize)>(data);
    match parsed {
        Ok((marker, _, _)) => match marker.marker {
            Marker::Array(Some(7..)) => Error::InvalidCBOR(CborError::AdditionalItems),
            Marker::Array(Some(_)) => Error::InvalidCBOR(CborError::NoMoreItems),
            _ => Error::InvalidCBOR(CborError::IncorrectType(
                "Definite length array".to_string(),
                marker.to_string(),
            )),
        },
        Err(e) => Error::InvalidCBOR(e),
    }
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
pub fn parse(data: Bytes) -> Result<Parsed, Error> {
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
