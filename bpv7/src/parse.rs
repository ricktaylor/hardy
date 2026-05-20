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
}

pub enum ParserProgress {
    NeedMore(usize),
    /// Parsing is complete. Carries the concatenation of all bytes received
    /// via `push()` as a single contiguous `Bytes`. Yielded exactly once.
    Ready(Bytes),
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
    pending_bibs: Vec<u64>,

    /// Parsed BCB OperationSets for every BCB encountered, keyed by
    /// BCB block number. BCB bodies are always plaintext (the ASB
    /// describes what the BCB encrypts; the ASB itself isn't encrypted),
    /// so we can parse them inline during `parse_blocks`. Consumed by
    /// `finish()` for BCB cross-block validation and to mark BCB
    /// coverage on target blocks before the BIB pass runs.
    bcbs: HashMap<u64, bpsec::bcb::OperationSet>,
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
            pending_bibs: Vec::new(),
            bcbs: HashMap::new(),
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
            State::Done => {
                panic!("push called after parser already reached Done state");
            }
        };

        match r {
            Ok(_) => {
                // Parse complete (Ok only ever returned at Done). Hand back
                // the consumed bytes as a single contiguous Bytes:
                //   - multi-chunk: freeze the cached BytesMut (zero-copy)
                //   - single-chunk: the original data_in
                let bytes = match cached {
                    Some(buf) => buf.freeze(),
                    None => data_in,
                };
                Ok(ParserProgress::Ready(bytes))
            }
            Err(Error::InvalidCBOR(CborError::NeedMoreData(more))) => {
                // First-time materialisation if we don't have a cache yet.
                // try_into_mut is zero-copy when refcount=1.
                let mut buf = cached.unwrap_or_else(|| match data_in.try_into_mut() {
                    Ok(b) => b,
                    Err(orig) => BytesMut::from(orig.as_ref()),
                });
                buf.reserve(more);
                self.data = Some(buf);
                Ok(ParserProgress::NeedMore(more))
            }
            Err(e) => Err(e),
        }
    }

    /// Drains the parser into the final bundle index plus the BIB and
    /// BCB `OperationSet`s decoded along the way. Returning the
    /// OperationSets lets the keyed BPSec filter skip a full CBOR
    /// re-decode of every BIB/BCB body (see `bpv7/docs/TODO.md` M1).
    /// Bundles with no BPSec return empty maps.
    ///
    /// `data` should be the [`ParserProgress::Ready`] buffer this
    /// parser handed back from [`push`](Self::push). It is moved
    /// through `finish` and returned in the [`Parsed`] result so
    /// callers have a single authoritative byte source for the returned
    /// offsets — slicing their own copy of the input risks aliasing
    /// against a different buffer in the streaming case.
    pub fn finish(mut self, data: Bytes) -> Result<Parsed, Error> {
        assert!(
            matches!(self.state, State::Done),
            "finish called before parser reached Done state"
        );
        let bibs = if !self.bcbs.is_empty() || !self.pending_bibs.is_empty() {
            self.validate_bpsec_structure(&data)?
        } else {
            HashMap::new()
        };
        let bundle = self
            .bundle
            .expect("Done state guarantees self.bundle is populated");
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
            let bcb_block = bundle
                .blocks
                .get(bcb_block_number)
                .expect("BCB memoised but missing from bundle.blocks");

            // Per-OperationSet rules (including §3.9 duplicate-target
            // check via the already-stamped .bcb fields). Single source
            // of truth shared with the post-decrypt keyed filter.
            checks::check_bcb(ops, *bcb_block_number, &bcb_block.flags, &bundle.blocks)?;

            // Stamp each target. check_bcb has already verified every
            // target exists and is not claimed by a different BCB.
            for &target_number in ops.operations.keys() {
                bundle
                    .blocks
                    .get_mut(&target_number)
                    .expect("check_bcb verified every target exists")
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
            checks::check_bib(&ops, bib_block_number, &bundle.blocks)?;

            // Stamp each target. check_bib has already verified every
            // target exists and is not claimed by a different BIB.
            for &target_number in ops.operations.keys() {
                bundle
                    .blocks
                    .get_mut(&target_number)
                    .expect("check_bib verified every target exists")
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
        // Bundle outer array head — RFC 9171 §4.1 (normative): SHALL be a
        // CBOR indefinite-length array. The only legal first byte is 0x9F.
        // Appendix B's CDDL `bpv7_start = bundle / #6.55799(bundle)` is
        // informational and explicitly subordinate to the textual spec
        // (`§4.1`), so the self-describing CBOR tag is rejected here.
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
                // Inline payload: consume the outer indefinite-array
                // `0xFF` break and reject any trailing data after it.
                // For streaming-fallback payloads (body not in buffer),
                // the BPA owns those checks downstream.
                if offset as u64 == extent_end {
                    match data.get(offset) {
                        Some(&0xFF) => offset += 1,
                        Some(_) => return Err(Error::NotCanonical),
                        None => return Err(Error::InvalidCBOR(CborError::NeedMoreData(1))),
                    }
                    if offset != data.len() {
                        return Err(Error::AdditionalData);
                    }
                }
                self.state = State::Done;
                return Ok(offset);
            }
            self.state = State::Blocks(offset);
        }
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
/// [`ParserProgress::Ready`].
pub fn parse(data: Bytes) -> Result<Parsed, Error> {
    let mut parser = BundleParser::default();
    let data = match parser.push(data)? {
        ParserProgress::NeedMore(more) => {
            return Err(Error::InvalidCBOR(CborError::NeedMoreData(more)));
        }
        ParserProgress::Ready(data) => data,
    };

    let parsed = parser.finish(data)?;
    if parsed
        .bundle
        .blocks
        .get(&1)
        .ok_or(Error::MissingPayload)?
        .extent
        .end
        < (parsed.data.len() - 1) as u64
    {
        return Err(Error::AdditionalData);
    }

    Ok(parsed)
}
