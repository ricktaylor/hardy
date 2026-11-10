use alloc::borrow::Cow;
use core::ops::Range;

use bytes::Bytes;
use hardy_cbor::decode::parse_exact;
use thiserror::Error;

use super::*;
#[cfg(feature = "bpsec")]
use crate::bpsec::{Decrypted, key::KeySource};
use crate::reader::{Availability, Reader};

#[derive(Debug, Error)]
pub enum Error {
    #[error("Attempt to add duplicate block of type {0:?}")]
    IllegalDuplicate(block::Type),

    #[error("No more available block numbers")]
    OutOfBlockNumbers,

    #[error("Cannot edit the primary block")]
    PrimaryBlock,

    #[error("Cannot remove the payload block")]
    PayloadBlock,

    #[error("No such block number {0}")]
    NoSuchBlock(u64),

    #[error("Block {0} is protected by an encrypted BIB; use remove_integrity() first")]
    BibIsEncrypted(u64),

    #[error("Primary block is protected by a BIB; use remove_integrity(0) first")]
    PrimaryBlockHasBib,

    /// The block is the target of no BCB, so has no encryption to remove.
    #[error("Block is not the target of a BCB")]
    NotEncrypted,

    /// The block is the target of no BIB, so has no integrity operation to
    /// remove.
    #[error("Block is not the target of a BIB")]
    NotSigned,

    /// An encrypted BIB that could not be decrypted may cover the block, so
    /// the edit cannot tell whether it breaks an integrity operation.
    #[error("Block {0} may be protected by an encrypted BIB that couldn't be decrypted")]
    MaybeHasBib(u64),

    /// Removing the BCB without its targets would leave their ciphertext to
    /// reparse as plaintext.
    #[error("Cannot remove BCB {0} without also removing all of its targets")]
    StrandsCiphertext(u64),

    /// Removing `block` would remove its operation from the encrypted BIB
    /// `bib`, which also covers a block that stays: the BIB would have to be
    /// encrypted again, which only its BCB's security source may do.
    #[error(
        "Removing block {block} would shrink encrypted BIB {bib}, which only its BCB's security source may encrypt"
    )]
    ShrinksEncryptedBib { block: u64, bib: u64 },

    /// A change to the primary block's bytes would break an operation whose
    /// scope includes the primary block, as RFC 9173's default scope does:
    /// one held by security block `{0}`, the lowest such block, which may
    /// be an encrypted BIB whose scope cannot be read.
    #[error("Security block {0} has an operation whose scope includes the primary block")]
    PrimaryInSecurityScope(u64),

    #[error("Security blocks (BIB/BCB) must be managed via Signer/Encryptor, not the Editor")]
    SecurityBlock,

    /// The payload's data was staged, but the payload streams past the
    /// editor ([`Editor::with_streamed_payload`]): the staged data would
    /// never reach the wire.
    #[error("The payload's data is staged, but the payload streams")]
    StagedPayload,

    /// A block carries `report_on_failure` on a bundle whose final primary
    /// forbids it (RFC 9171 §4.2.3-4/-5); refused when the bundle is rebuilt,
    /// naming the lowest such block.
    #[error("Block {0} requests report_on_failure, which the bundle forbids")]
    ReportOnFailureForbidden(u64),

    /// A security operation the edit needed failed: a decryption, the
    /// checks of a decoded BIB, a key the source does not hold.
    #[error(transparent)]
    Bpsec(#[from] bpsec::Error),

    #[error(transparent)]
    Builder(#[from] builder::Error),
}

// A BPSec error reaches the editor as `Bpsec`, however it was wrapped on
// the way; any other bundle error is the builder's.
impl From<error::Error> for Error {
    fn from(e: error::Error) -> Self {
        match e {
            error::Error::InvalidBPSec(e) => Error::Bpsec(e),
            e => Error::Builder(builder::Error::InternalError(e)),
        }
    }
}

#[derive(Debug)]
pub enum Chunk {
    Unchanged(Range<usize>),
    New(Box<[u8]>),
}

impl Chunk {
    fn len(&self) -> usize {
        match self {
            Chunk::Unchanged(range) => range.len(),
            Chunk::New(data) => data.len(),
        }
    }

    /// Flatten chunks into a contiguous byte buffer, wrapping in a CBOR
    /// indefinite-length array (0x9F prefix, 0xFF suffix):
    /// [`flatten_head`](Self::flatten_head) with the break.
    ///
    /// # Panics
    ///
    /// Panics if an `Unchanged` extent lies outside `source`. Chunk plans
    /// from [`Editor::rebuild`] / [`Editor::rebuild_bundle`] over the same
    /// source buffer always satisfy this; pairing a plan with a different
    /// (or truncated) source is a caller error.
    pub fn flatten(chunks: Vec<Self>, source: &[u8]) -> Box<[u8]> {
        let mut result = Self::flatten_head(chunks, source);
        result.push(0xFF);
        result.into()
    }

    /// Flatten chunks into a contiguous byte buffer after the head of the
    /// CBOR indefinite-length array (0x9F), without its break: the bytes a
    /// caller continues itself, as the rebuilt header region of a streamed
    /// rebuild ([`Editor::rebuild_streamed`]) is continued by the payload
    /// and the trailer that ends the bundle.
    ///
    /// # Panics
    ///
    /// Panics if an `Unchanged` extent lies outside `source`, as
    /// [`flatten`](Self::flatten) does.
    pub fn flatten_head(chunks: Vec<Self>, source: &[u8]) -> Vec<u8> {
        let len = 1 + chunks.iter().map(|c| c.len()).sum::<usize>();
        // Room for the break a caller may add.
        let mut result = Vec::with_capacity(len + 1);
        result.push(0x9F);
        for c in chunks {
            match c {
                Chunk::Unchanged(extent) => {
                    result.extend_from_slice(
                        source
                            .get(extent)
                            .expect("Chunk::Unchanged extent out of bounds of the source buffer"),
                    );
                }
                Chunk::New(items) => {
                    result.extend(items);
                }
            }
        }
        debug_assert_eq!(result.len(), len);
        result
    }

    /// Bytes-flavoured rebuild. Consumes `source` and returns the new
    /// `Bytes`. Zero-copy when `source` uniquely owns its buffer
    /// (`source.try_into_mut()` succeeds — the typical case for bundles
    /// that have just come off the wire), falling back to allocating a
    /// fresh buffer when the source is shared. Saves callers the
    /// `Bytes → Vec → flatten_inplace → Bytes` dance.
    pub fn flatten_bytes(chunks: Vec<Self>, source: Bytes) -> Bytes {
        match source.try_into_mut() {
            Ok(bm) => {
                // BytesMut → Vec → flatten_inplace → Vec → Bytes:
                // both conversions are zero-copy when the BytesMut owns
                // its allocation, which `try_into_mut` guarantees.
                let mut vec: Vec<u8> = bm.into();
                Self::flatten_inplace(chunks, &mut vec);
                Bytes::from(vec)
            }
            Err(shared) => Bytes::from(Self::flatten(chunks, &shared)),
        }
    }

    /// Modify the source buffer in place to produce the rebuilt bundle.
    ///
    /// This avoids allocation when the assembled chunks fit within the
    /// original buffer. Unchanged ranges that are already at the correct
    /// position are left untouched; New chunks overwrite the gaps.
    /// The buffer is resized (truncated or extended) if the total output
    /// length differs from the source.
    ///
    /// # Panics
    ///
    /// Panics if an `Unchanged` extent lies outside `source` (same
    /// precondition as [`Chunk::flatten`]).
    pub fn flatten_inplace(chunks: Vec<Self>, source: &mut Vec<u8>) {
        // Single pass: compute total length and the required copy direction. An
        // Unchanged range shifts right if its destination is past its source
        // (needs a backward pass), left if before it (needs a forward pass).
        let mut content_len: usize = 0;
        let mut write_pos: usize = 1; // after 0x9F
        let mut shifts_right = false;
        let mut shifts_left = false;
        for chunk in &chunks {
            let len = chunk.len();
            if let Chunk::Unchanged(range) = chunk {
                debug_assert!(range.end <= source.len());
                if write_pos > range.start {
                    shifts_right = true;
                } else if write_pos < range.start {
                    shifts_left = true;
                }
            }
            content_len += len;
            write_pos += len;
        }

        // A single in-place pass is only sound when all Unchanged ranges shift
        // the same way: forward copy for all-left, backward copy for all-right.
        // If ranges shift both ways, either pass would overwrite source bytes it
        // has not yet copied, so assemble into a fresh buffer instead.
        if shifts_right && shifts_left {
            *source = Self::flatten(chunks, source).into_vec();
            return;
        }
        let needs_backward = shifts_right;

        let total_len = 1 + content_len + 1; // 0x9F prefix + content + 0xFF suffix

        // Ensure buffer is large enough
        if total_len > source.len() {
            source.resize(total_len, 0);
        }

        source[0] = 0x9F;

        if needs_backward {
            // Backward pass: process chunks from back to front to avoid
            // overwriting source data that hasn't been read yet
            let mut write_end = 1 + content_len;
            for chunk in chunks.iter().rev() {
                match chunk {
                    Chunk::Unchanged(range) => {
                        write_end -= range.len();
                        if range.start != write_end {
                            source.copy_within(range.clone(), write_end);
                        }
                    }
                    Chunk::New(data) => {
                        write_end -= data.len();
                        source[write_end..write_end + data.len()].copy_from_slice(data);
                    }
                }
            }
            debug_assert_eq!(write_end, 1);
        } else {
            // Forward pass: safe when write positions are at or before source positions
            let mut write_pos: usize = 1;
            for chunk in chunks {
                match chunk {
                    Chunk::Unchanged(range) => {
                        let len = range.len();
                        if range.start != write_pos {
                            source.copy_within(range, write_pos);
                        }
                        write_pos += len;
                    }
                    Chunk::New(data) => {
                        source[write_pos..write_pos + data.len()].copy_from_slice(&data);
                        write_pos += data.len();
                    }
                }
            }
            debug_assert_eq!(write_pos, 1 + content_len);
        }

        // Write 0xFF suffix and truncate
        source[total_len - 1] = 0xFF;
        source.truncate(total_len);
    }

    /// Assemble chunks into optimal order for output:
    /// - Primary first, payload last
    /// - Unchanged extension blocks sorted by source position
    /// - New extension blocks slotted into gaps of matching size where possible
    /// - Adjacent Unchanged ranges merged
    ///
    /// If `bundle` is provided, block extents are updated to reflect positions
    /// in the flattened output (offset 1 for the 0x9F array header).
    fn assemble(
        primary: (u64, Chunk),
        extensions: Vec<(u64, Chunk)>,
        payload: (u64, Chunk),
        bundle: Option<&mut bundle::Bundle>,
    ) -> Vec<Chunk> {
        // Separate unchanged and new extension chunks
        let mut unchanged: Vec<(u64, Range<usize>)> = Vec::with_capacity(extensions.len());
        let mut new_chunks: Vec<(u64, Box<[u8]>)> = Vec::with_capacity(extensions.len());

        for (block_number, chunk) in extensions {
            match chunk {
                Chunk::Unchanged(range) => unchanged.push((block_number, range)),
                Chunk::New(data) => new_chunks.push((block_number, data)),
            }
        }

        // Sort unchanged by source position
        unchanged.sort_by_key(|(_, range)| range.start);

        // Sort new chunks by size descending for best-fit gap filling, then
        // block number ascending as a tiebreak for deterministic output.
        new_chunks.sort_by_key(|(bn, b)| (core::cmp::Reverse(b.len()), *bn));

        // Build ordered extension list: try to fill gaps with matching-size New chunks
        let mut ordered: Vec<(u64, Chunk)> = Vec::with_capacity(unchanged.len() + new_chunks.len());
        let mut prev_end: Option<usize> = None;

        for (block_number, range) in unchanged {
            // Check for gap before this unchanged block
            if let Some(end) = prev_end {
                let gap_size = range.start.saturating_sub(end);
                if gap_size > 0 {
                    // Try to find a New chunk that fits this gap exactly
                    if let Some(idx) = new_chunks
                        .iter()
                        .position(|(_, data)| data.len() == gap_size)
                    {
                        let (new_bn, new_data) = new_chunks.swap_remove(idx);
                        ordered.push((new_bn, Chunk::New(new_data)));
                    }
                }
            }
            prev_end = Some(range.end);
            ordered.push((block_number, Chunk::Unchanged(range)));
        }

        // Append remaining New chunks that didn't fit in gaps
        for (block_number, data) in new_chunks {
            ordered.push((block_number, Chunk::New(data)));
        }

        // Assemble final list: primary + extensions + payload
        let mut assembled: Vec<(u64, Chunk)> = Vec::with_capacity(1 + ordered.len() + 1);
        assembled.push(primary);
        assembled.extend(ordered);
        assembled.push(payload);

        // Update block extents if bundle provided
        if let Some(bundle) = bundle {
            let mut offset: usize = 1; // 0x9F prefix
            for (block_number, chunk) in &assembled {
                let len = chunk.len();
                if let Some(block) = bundle.blocks.get_mut(block_number) {
                    block.extent = offset as u64..(offset + len) as u64;
                }
                offset += len;
            }
        }

        // Merge adjacent Unchanged ranges
        let mut result: Vec<Chunk> = Vec::with_capacity(assembled.len());
        for (_, chunk) in assembled {
            match (&mut result.last_mut(), &chunk) {
                (Some(Chunk::Unchanged(prev)), Chunk::Unchanged(next))
                    if prev.end == next.start =>
                {
                    prev.end = next.end;
                }
                _ => result.push(chunk),
            }
        }

        result
    }
}

/// The `Editor` provides an interface for modifying a bundle.
///
/// The editor is designed to allow for efficient modification of a bundle by
/// reusing the unmodified portions of the original bundle.
///
/// The primary-block setters ([`set_source`](Self::set_source),
/// [`set_destination`](Self::set_destination) and the rest) refuse while a
/// BIB targets the primary block, and while any remaining BIB or BCB
/// operation's scope includes it, as RFC 9173's default scope does: a
/// change to the primary's bytes would break that operation. An encrypted
/// BIB, whose scope cannot be read, counts as including it.
/// [`remove_integrity(0)`](crate::bpsec::edit::BPSecEditor::remove_integrity),
/// or removing that BIB with
/// [`remove_blocks`](crate::bpsec::edit::BPSecEditor::remove_blocks),
/// releases a primary no other operation scopes. Fragment info is exempt
/// (see [`set_fragment_info`](Self::set_fragment_info)).
pub struct Editor<'a> {
    original: &'a bundle::Bundle,
    source_data: &'a [u8],
    /// Primary-block edits staged via `with_*` methods. `Some` =
    /// caller has modified at least one primary field; `None` = use
    /// `original.primary` verbatim. Isomorphic to a `PrimaryBlock`
    /// because that's exactly what the edits collectively form.
    primary: Option<primary_block::PrimaryBlock>,
    blocks: HashMap<u64, BlockTemplate<'a>>,
    bib_overrides: HashMap<u64, block::BibCoverage>,
    bcb_overrides: HashMap<u64, Option<u64>>,
    #[cfg(feature = "bpsec")]
    pub(crate) key_source: Option<&'a dyn KeySource>,
    /// The plaintext an earlier pass over the bundle decrypted
    /// ([`with_decrypted`](Self::with_decrypted)), read before any key.
    #[cfg(feature = "bpsec")]
    pub(crate) decrypted: Option<&'a Decrypted>,
    /// The length of the payload's plaintext, where the payload's data
    /// streams past the editor rather than being resident
    /// ([`with_streamed_payload`](Self::with_streamed_payload)).
    streamed_payload: Option<u64>,
    /// The edits staged so far, counted: one per block staged, removed or
    /// primary field set.
    generation: u64,
}

enum BlockTemplate<'a> {
    Keep(block::Type),
    Update(builder::BlockTemplate<'a>),
    Insert(builder::BlockTemplate<'a>),
}

/// The `BlockBuilder` is used to construct a new or replacement block for a
/// bundle. It borrows the editor it was taken from and stages the block
/// into it on [`rebuild`](Self::rebuild); dropped unrebuilt, it stages
/// nothing.
#[must_use = "a block builder stages nothing until it is rebuilt"]
pub struct BlockBuilder<'e, 'a> {
    editor: &'e mut Editor<'a>,
    block_number: u64,
    is_new: bool,
    template: builder::BlockTemplate<'a>,
}

impl<'a> Editor<'a> {
    /// Create a new `Editor` for the given `bundle`.
    ///
    /// The `source_data` is the serialized form of the `bundle`.
    pub fn new(original: &'a bundle::Bundle, source_data: &'a [u8]) -> Self {
        Self {
            blocks: original
                .blocks
                .iter()
                .map(|(block_number, block)| (*block_number, BlockTemplate::Keep(block.block_type)))
                .collect(),
            source_data,
            original,
            primary: None,
            bib_overrides: HashMap::new(),
            bcb_overrides: HashMap::new(),
            #[cfg(feature = "bpsec")]
            key_source: None,
            #[cfg(feature = "bpsec")]
            decrypted: None,
            streamed_payload: None,
            generation: 0,
        }
    }

    /// The number of edits staged so far: a block inserted, updated,
    /// restored or removed, or a primary field set. Zero until the first
    /// edit, so a caller can ask the editor whether anything changed rather
    /// than keep a flag beside it.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Declare that the payload's data streams past the editor, as the
    /// source bytes hold only the headers, and that its plaintext is
    /// `plaintext_len` bytes long: the payload's security acceptor decrypts
    /// it as it streams ([`bcb::Operation::begin_decrypt`](crate::bpsec::bcb::Operation::begin_decrypt)),
    /// and knows that length before the first byte. Removing the payload's
    /// encryption ([`remove_encryption`](crate::bpsec::edit::BPSecEditor::remove_encryption))
    /// then writes the payload block for a streamed plaintext of that
    /// length, rather than decrypting resident data, and
    /// [`rebuild_streamed`](Self::rebuild_streamed) frames it.
    pub fn with_streamed_payload(mut self, plaintext_len: u64) -> Self {
        self.streamed_payload = Some(plaintext_len);
        self
    }

    /// The primary block as the staged edits leave it.
    pub(crate) fn current_primary(&self) -> &primary_block::PrimaryBlock {
        self.primary.as_ref().unwrap_or(&self.original.primary)
    }

    /// Whether the primary block, as the staged edits leave it, has
    /// integrity protection, judged as
    /// [`Bundle::primary_has_integrity`](bundle::Bundle::primary_has_integrity)
    /// judges a parsed one, with the block's current coverage.
    #[must_use]
    pub fn primary_has_integrity(&self) -> bool {
        !matches!(self.current_primary().crc_type, crc::CrcType::None)
            || self
                .current_coverage(0)
                .is_some_and(|(bib, _)| !matches!(bib, block::BibCoverage::None))
    }

    /// The bundle as the staged edits leave it: the current primary block
    /// and the block headers, each with its current BIB and BCB coverage.
    /// A copy of the headers alone, for reading while the editor goes on
    /// editing: later edits do not show in it.
    ///
    /// A staged block's `extent` and `data` index no bytes: its body is
    /// staged, not on the wire, and that includes a staged primary block's.
    /// Read block bodies through the editor's
    /// [`Reader`] impl, or through a
    /// [`DecryptingReader`](crate::bpsec::DecryptingReader) over it.
    #[must_use]
    pub fn current_bundle(&self) -> bundle::Bundle {
        let blocks = self
            .blocks
            .keys()
            .map(|&block_number| {
                let (block, bib, bcb) = self
                    .current_block(block_number)
                    .expect("the editor holds every block it lists");
                let mut header = match (block_number, &self.primary) {
                    (0, Some(primary)) => {
                        primary_block::PrimaryBlock::as_block(primary.crc_type, 0..0)
                    }
                    _ => block.clone(),
                };
                header.bib = bib;
                header.bcb = bcb;
                (block_number, header)
            })
            .collect();
        bundle::Bundle {
            primary: self.current_primary().clone(),
            blocks,
        }
    }

    fn primary_block(&mut self) -> Result<&mut primary_block::PrimaryBlock, Error> {
        // Refuse while a BIB still targets the primary block, judged by the
        // primary's current coverage rather than the BIB's template: an edit
        // that strips another target from the same BIB rewrites that BIB but
        // leaves the primary under it. Then refuse while any remaining
        // operation's scope includes the primary.
        if self.primary_bib().is_some() {
            return Err(Error::PrimaryBlockHasBib);
        }
        if let Some(security_block) = self.primary_scoped_operation(&HashSet::new()) {
            return Err(Error::PrimaryInSecurityScope(security_block));
        }
        Ok(self.primary_block_unchecked())
    }

    fn primary_block_unchecked(&mut self) -> &mut primary_block::PrimaryBlock {
        self.primary
            .get_or_insert_with(|| self.original.primary.clone())
    }

    /// A block's header and its current BIB and BCB coverage, or `None` if
    /// the bundle no longer holds it. An earlier edit's strip records the new
    /// coverage in the overrides and leaves the header's stamps stale: the
    /// BIB or BCB they name may be gone, and its number reused by an
    /// unrelated block.
    pub(crate) fn current_block(
        &self,
        block_number: u64,
    ) -> Option<(&block::Block, block::BibCoverage, Option<u64>)> {
        let (block, _) = self.block(block_number)?;
        Some((
            block,
            self.bib_overrides
                .get(&block_number)
                .cloned()
                .unwrap_or(block.bib),
            self.bcb_overrides
                .get(&block_number)
                .copied()
                .unwrap_or(block.bcb),
        ))
    }

    /// A block's current BIB and BCB coverage, as
    /// [`current_block`](Self::current_block) gives it.
    pub(crate) fn current_coverage(
        &self,
        block_number: u64,
    ) -> Option<(block::BibCoverage, Option<u64>)> {
        self.current_block(block_number)
            .map(|(_, bib, bcb)| (bib, bcb))
    }

    /// The BIB that currently targets the primary block, if any.
    pub(crate) fn primary_bib(&self) -> Option<u64> {
        let coverage = match self.bib_overrides.get(&0) {
            Some(coverage) => coverage,
            None => &self.original.blocks.get(&0)?.bib,
        };
        match coverage {
            block::BibCoverage::Some(bib_num) if self.blocks.contains_key(bib_num) => {
                Some(*bib_num)
            }
            _ => None,
        }
    }

    /// The lowest security block outside `removed` holding an operation
    /// whose scope includes the primary block: a BIB or BCB operation on a
    /// target outside `removed` (other than the primary, which a BIB covers
    /// as its target), or a security block whose body does not decode, as
    /// an encrypted BIB's ciphertext does not, and whose operations may. A
    /// change to the primary block's bytes breaks every such operation.
    pub(crate) fn primary_scoped_operation(&self, removed: &HashSet<u64>) -> Option<u64> {
        let live = |target: &u64| *target != 0 && !removed.contains(target);
        self.blocks
            .keys()
            .copied()
            .filter(|n| !removed.contains(n))
            .filter(|&n| {
                let Some((block, payload)) = self.block(n) else {
                    return false;
                };
                match block.block_type {
                    block::Type::BlockIntegrity => payload
                        .and_then(|p| parse_exact::<bpsec::bib::OperationSet>(p).ok())
                        .is_none_or(|opset| {
                            opset.operations.iter().any(|(target, operation)| {
                                live(target) && operation.scope_includes_primary()
                            })
                        }),
                    block::Type::BlockSecurity => payload
                        .and_then(|p| parse_exact::<bpsec::bcb::OperationSet>(p).ok())
                        .is_none_or(|opset| {
                            opset.operations.iter().any(|(target, operation)| {
                                live(target) && operation.scope_includes_primary()
                            })
                        }),
                    _ => false,
                }
            })
            .min()
    }

    /// Sets the bundle flags for this [`Editor`], canonicalized so an alias
    /// bit in `unrecognised` counts as the flag it encodes. The fragment flag
    /// follows the fragment info, which
    /// [`set_fragment_info`](Self::set_fragment_info) sets.
    ///
    /// # Errors
    ///
    /// [`Error::PrimaryBlockHasBib`] while a BIB targets the primary block,
    /// and [`Error::PrimaryInSecurityScope`] while a remaining operation's
    /// scope includes it (see [`Editor`]).
    pub fn set_bundle_flags(&mut self, flags: bundle::Flags) -> Result<(), Error> {
        let pb = self.primary_block()?;
        pb.flags = flags.canonicalize();
        pb.flags.is_fragment = pb.id.fragment_info.is_some();
        self.generation += 1;
        Ok(())
    }

    /// Sets the [`crc::CrcType`] for this [`Editor`].
    ///
    /// # Errors
    ///
    /// [`Error::PrimaryBlockHasBib`] while a BIB targets the primary block,
    /// and [`Error::PrimaryInSecurityScope`] while a remaining operation's
    /// scope includes it (see [`Editor`]).
    pub fn set_bundle_crc_type(&mut self, crc_type: crc::CrcType) -> Result<(), Error> {
        let pb = self.primary_block()?;
        pb.crc_type = crc_type;
        self.generation += 1;
        Ok(())
    }

    /// Sets the creation timestamp for this [`Editor`].
    ///
    /// # Errors
    ///
    /// [`Error::PrimaryBlockHasBib`] while a BIB targets the primary block,
    /// and [`Error::PrimaryInSecurityScope`] while a remaining operation's
    /// scope includes it (see [`Editor`]).
    pub fn set_timestamp(
        &mut self,
        timestamp: creation_timestamp::CreationTimestamp,
    ) -> Result<(), Error> {
        self.primary_block()?.id.timestamp = timestamp;
        self.generation += 1;
        Ok(())
    }

    /// Sets the source [`eid::Eid`] for this [`Editor`].
    ///
    /// # Errors
    ///
    /// [`Error::PrimaryBlockHasBib`] while a BIB targets the primary block,
    /// and [`Error::PrimaryInSecurityScope`] while a remaining operation's
    /// scope includes it (see [`Editor`]).
    pub fn set_source(&mut self, source: eid::Eid) -> Result<(), Error> {
        let pb = self.primary_block()?;
        pb.id.source = source;
        self.generation += 1;
        Ok(())
    }

    /// Sets the destination [`eid::Eid`] for this [`Editor`].
    ///
    /// # Errors
    ///
    /// [`Error::PrimaryBlockHasBib`] while a BIB targets the primary block,
    /// and [`Error::PrimaryInSecurityScope`] while a remaining operation's
    /// scope includes it (see [`Editor`]).
    pub fn set_destination(&mut self, destination: eid::Eid) -> Result<(), Error> {
        let pb = self.primary_block()?;
        pb.destination = destination;
        self.generation += 1;
        Ok(())
    }

    /// Sets the report_to [`eid::Eid`] for this [`Editor`].
    ///
    /// # Errors
    ///
    /// [`Error::PrimaryBlockHasBib`] while a BIB targets the primary block,
    /// and [`Error::PrimaryInSecurityScope`] while a remaining operation's
    /// scope includes it (see [`Editor`]).
    pub fn set_report_to(&mut self, report_to: eid::Eid) -> Result<(), Error> {
        let pb = self.primary_block()?;
        pb.report_to = report_to;
        self.generation += 1;
        Ok(())
    }

    /// Sets the lifetime for this [`Editor`].
    ///
    /// # Errors
    ///
    /// [`Error::PrimaryBlockHasBib`] while a BIB targets the primary block,
    /// and [`Error::PrimaryInSecurityScope`] while a remaining operation's
    /// scope includes it (see [`Editor`]).
    pub fn set_lifetime(&mut self, lifetime: core::time::Duration) -> Result<(), Error> {
        let pb = self.primary_block()?;
        pb.lifetime = lifetime.min(core::time::Duration::from_millis(u64::MAX));
        self.generation += 1;
        Ok(())
    }

    /// Sets the fragment info for this [`Editor`], and the fragment flag
    /// with it.
    ///
    /// Exempt from the other setters' security refusals, so it does not
    /// fail: no BIB or BCB is added to a bundle fragment (RFC 9172 §5.2),
    /// so every operation a bundle carries covers its unfragmented primary,
    /// and the fragment fields belong to fragmentation itself (RFC 9171
    /// §5.8). Clearing them at reassembly restores the primary those
    /// operations were computed over.
    pub fn set_fragment_info(
        &mut self,
        fragment_info: Option<bundle::FragmentInfo>,
    ) -> Result<(), Error> {
        let pb = self.primary_block_unchecked();
        pb.flags.is_fragment = fragment_info.is_some();
        pb.id.fragment_info = fragment_info;
        self.generation += 1;
        Ok(())
    }

    /// Add a new block into the bundle.
    ///
    /// The new block will be assigned the next available block
    /// number.  Be very careful about adding duplicate blocks that should not be duplicated
    ///
    pub fn push_block(&mut self, block_type: block::Type) -> Result<BlockBuilder<'_, 'a>, Error> {
        // Canonicalized so an `Unrecognised` alias of a reserved code cannot
        // bypass the variant-matched refusals and singleton rules below.
        let block_type = block_type.canonicalize();
        match block_type {
            block::Type::Primary => {
                return Err(Error::PrimaryBlock);
            }
            block::Type::BlockIntegrity | block::Type::BlockSecurity => {
                return Err(Error::SecurityBlock);
            }
            block::Type::Payload
            | block::Type::BundleAge
            | block::Type::HopCount
            | block::Type::PreviousNode => {
                for template in self.blocks.values() {
                    match template {
                        BlockTemplate::Keep(t) if t == &block_type => {
                            return Err(Error::IllegalDuplicate(block_type));
                        }
                        BlockTemplate::Insert(template) | BlockTemplate::Update(template)
                            if template.block.block_type == block_type =>
                        {
                            return Err(Error::IllegalDuplicate(block_type));
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }

        self.alloc_block(block_type)
    }

    /// Allocate the next available block number and return a builder.
    /// No policy checks — used internally and by Signer/Encryptor.
    pub(crate) fn alloc_block(
        &mut self,
        block_type: block::Type,
    ) -> Result<BlockBuilder<'_, 'a>, Error> {
        let mut block_number = 2u64;
        while self.blocks.contains_key(&block_number) {
            block_number = block_number
                .checked_add(1)
                .ok_or(Error::OutOfBlockNumbers)?;
        }
        Ok(BlockBuilder::new(self, block_number, block_type))
    }

    /// Insert a new block into the bundle.
    ///
    /// If a block of the same type already exists, the new block will replace
    /// it. Otherwise, the new block will be assigned the next available block
    /// number.
    ///
    /// Replacing a block that is a security target behaves exactly like
    /// [`update_block`](Self::update_block): the block is stripped from its
    /// covering BIB/BCB target lists (the old signature could not survive a
    /// body change), and the replace is refused with
    /// [`BibIsEncrypted`](Error::BibIsEncrypted) /
    /// [`MaybeHasBib`](Error::MaybeHasBib) where the coverage cannot
    /// be safely updated, a [`Maybe`](block::BibCoverage::Maybe) being first
    /// resolved with the editor's key source and plaintext cache
    /// ([`with_key_source`](Self::with_key_source),
    /// [`with_decrypted`](Self::with_decrypted)). As with `update_block`, a replaced block that was
    /// BCB-covered comes back with no data (its body was ciphertext), so
    /// the caller must supply fresh data with
    /// [`with_data`](BlockBuilder::with_data); rebuilding without it fails
    /// with [`NoBlockData`](builder::Error::NoBlockData).
    pub fn insert_block(&mut self, block_type: block::Type) -> Result<BlockBuilder<'_, 'a>, Error> {
        // Canonicalized so an `Unrecognised` alias of a reserved code cannot
        // bypass the refusals below, and the replace-by-type match compares
        // canonical types.
        let block_type = block_type.canonicalize();
        match block_type {
            block::Type::Primary => return Err(Error::PrimaryBlock),
            block::Type::BlockIntegrity | block::Type::BlockSecurity => {
                return Err(Error::SecurityBlock);
            }
            _ => {}
        }

        if let Some((block_number, staged)) =
            self.blocks
                .iter()
                .find_map(|(block_number, template)| match template {
                    // A kept wire block may carry live BIB/BCB coverage; the
                    // staged templates cannot (update_block already stripped
                    // it, and a fresh insert was never covered).
                    BlockTemplate::Keep(t) if &block_type == t => Some((*block_number, None)),
                    BlockTemplate::Insert(template) if template.block.block_type == block_type => {
                        Some((*block_number, Some((true, template.clone()))))
                    }
                    BlockTemplate::Update(template) if template.block.block_type == block_type => {
                        Some((*block_number, Some((false, template.clone()))))
                    }
                    _ => None,
                })
        {
            return match staged {
                // Replace-by-type on a live wire block is an update: the
                // same coverage stripping and BibIsEncrypted/MaybeHasBib
                // refusals apply, or the emitted bundle would keep a BIB
                // over a body its signature can no longer match.
                None => self.update_block(block_number),
                Some((is_new, template)) => Ok(BlockBuilder::reuse_template(
                    self,
                    block_number,
                    is_new,
                    template,
                )),
            };
        }

        self.alloc_block(block_type)
    }

    /// Update an existing block in the bundle.
    ///
    /// This will return a `BlockBuilder` that can be used to manipulate the
    /// existing block. If the block is a security target of a BIB or BCB, it
    /// will be automatically removed from those target lists first (since the
    /// signature/encryption would be invalid after modification). A block
    /// that was BCB-covered comes back with no data (its body was
    /// ciphertext), so the caller must supply fresh data with
    /// [`with_data`](BlockBuilder::with_data); rebuilding without it fails
    /// with [`NoBlockData`](builder::Error::NoBlockData). A block under a BIB
    /// that is itself encrypted is refused with
    /// [`BibIsEncrypted`](Error::BibIsEncrypted), and one whose coverage
    /// reads [`Maybe`](block::BibCoverage::Maybe) is first resolved with the
    /// editor's key source and plaintext cache
    /// ([`with_key_source`](Self::with_key_source),
    /// [`with_decrypted`](Self::with_decrypted)), or refused with
    /// [`MaybeHasBib`](Error::MaybeHasBib).
    pub fn update_block(&mut self, block_number: u64) -> Result<BlockBuilder<'_, 'a>, Error> {
        // The block's type, then its current security references
        let Some(block_type) = self.block(block_number).map(|(block, _)| block.block_type) else {
            return Err(Error::NoSuchBlock(block_number));
        };
        match block_type {
            block::Type::Primary => {
                return Err(Error::PrimaryBlock);
            }
            block::Type::BlockIntegrity | block::Type::BlockSecurity => {
                return Err(Error::SecurityBlock);
            }
            _ => {}
        }
        let Some((mut bib, bcb)) = self.current_coverage(block_number) else {
            return Err(Error::NoSuchBlock(block_number));
        };
        if matches!(bib, block::BibCoverage::Maybe) {
            bib = bpsec::edit::resolve_maybe(self, block_number)?;
        }

        // Handle BIB coverage — must remove from target list if present
        match bib {
            block::BibCoverage::Maybe => {
                return Err(Error::MaybeHasBib(block_number));
            }
            block::BibCoverage::Some(bib_num) => {
                if self
                    .current_coverage(bib_num)
                    .is_some_and(|(_, bcb)| bcb.is_some())
                {
                    return Err(Error::BibIsEncrypted(block_number));
                }
                self.remove_from_bib_targets(block_number, bib_num)?;
            }
            block::BibCoverage::None => {}
        }

        // Remove from BCB target list if present
        if let Some(bcb_num) = bcb {
            self.remove_from_bcb_targets(block_number, bcb_num)?;
        }

        self.update_block_inner(block_number)
    }

    /// Update an existing block, keeping the security operations over it.
    ///
    /// [`update_block`](Self::update_block) removes the edited block from
    /// every BIB and BCB target list, since an edit invalidates the
    /// operations computed over its data. This door keeps them, for a caller
    /// restoring the data they were computed over: reassembly, which
    /// restores the payload a fragmenter split (RFC 9171 §5.8), fragment 0
    /// carrying the operations over the whole (RFC 9172 §5.2), for the
    /// reassembled bundle's receiver to check. The caller answers for the
    /// data: an operation checked against anything else fails. Refuses the
    /// primary block and the security blocks, as `update_block` does, and
    /// resolves no [`Maybe`](block::BibCoverage::Maybe) coverage, there
    /// being nothing to remove.
    pub fn restore_block(&mut self, block_number: u64) -> Result<BlockBuilder<'_, 'a>, Error> {
        match self.block(block_number).map(|(block, _)| block.block_type) {
            None => Err(Error::NoSuchBlock(block_number)),
            Some(block::Type::Primary) => Err(Error::PrimaryBlock),
            Some(block::Type::BlockIntegrity | block::Type::BlockSecurity) => {
                Err(Error::SecurityBlock)
            }
            Some(_) => self.update_block_inner(block_number),
        }
    }

    /// Update an existing block without automatic security target removal.
    ///
    /// This is for internal use by encryptor/signer which need to update blocks
    /// that are security targets without removing them from those target lists.
    pub(crate) fn update_block_inner(
        &mut self,
        block_number: u64,
    ) -> Result<BlockBuilder<'_, 'a>, Error> {
        let (is_new, template) = match self.blocks.get(&block_number) {
            None => return Err(Error::NoSuchBlock(block_number)),
            Some(BlockTemplate::Keep(t)) => {
                if let &block::Type::Primary = t {
                    return Err(Error::PrimaryBlock);
                }
                let block = match self.original.blocks.get(&block_number) {
                    Some(b) => b,
                    None => return Err(Error::NoSuchBlock(block_number)),
                };

                let mut tmpl = builder::BlockTemplate::new(
                    *t,
                    block.flags.clone(),
                    block.crc_type,
                    if block.bcb.is_some() {
                        // Block is encrypted, caller MUST provide fresh data
                        None
                    } else {
                        block.payload(self.source_data).map(Cow::Borrowed)
                    },
                );
                // Updating a block's body doesn't change which security
                // blocks protect it — preserve the bib/bcb metadata so
                // later cascades (and the final rebuild) see the right
                // relationships. Without this, e.g. `remove_block_inner`
                // recursively dropping a BIB whose body we just staged
                // would miss the protecting BCB and leave it orphaned. The
                // data decision above stays on the original's BCB: the
                // source bytes are ciphertext even after a strip.
                let (bib, bcb) = self
                    .current_coverage(block_number)
                    .unwrap_or((block.bib, block.bcb));
                tmpl.block.bib = bib;
                tmpl.block.bcb = bcb;
                (false, tmpl)
            }
            Some(BlockTemplate::Insert(template)) => (true, template.clone()),
            Some(BlockTemplate::Update(template)) => (false, template.clone()),
        };

        Ok(BlockBuilder::reuse_template(
            self,
            block_number,
            is_new,
            template,
        ))
    }

    /// Remove a block from the bundle.
    ///
    /// Note that the primary and payload blocks cannot be removed. A block
    /// under a BIB that is itself encrypted is refused with
    /// [`BibIsEncrypted`](Error::BibIsEncrypted), and one whose coverage
    /// reads [`Maybe`](block::BibCoverage::Maybe) is first resolved with the
    /// editor's key source and plaintext cache
    /// ([`with_key_source`](Self::with_key_source),
    /// [`with_decrypted`](Self::with_decrypted)), or refused with
    /// [`MaybeHasBib`](Error::MaybeHasBib).
    pub fn remove_block(&mut self, block_number: u64) -> Result<(), Error> {
        if block_number == 0 {
            return Err(Error::PrimaryBlock);
        }
        if block_number == 1 {
            return Err(Error::PayloadBlock);
        }

        // Reject security blocks and check BIB coverage of the target.
        if let Some((block, _)) = self.block(block_number) {
            // Security blocks are managed only via Signer/Encryptor (and
            // remove_integrity/remove_encryption), as push/insert/update_block
            // also enforce. Removing a BCB here would leave its targets holding
            // ciphertext with no covering BCB — surfacing ciphertext as
            // plaintext on reparse — and removing a BIB would silently strip
            // integrity protection.
            if matches!(
                block.block_type,
                block::Type::BlockIntegrity | block::Type::BlockSecurity
            ) {
                return Err(Error::SecurityBlock);
            }
        }
        if let Some((mut bib, _)) = self.current_coverage(block_number) {
            if matches!(bib, block::BibCoverage::Maybe) {
                bib = bpsec::edit::resolve_maybe(self, block_number)?;
            }
            match bib {
                block::BibCoverage::Maybe => {
                    return Err(Error::MaybeHasBib(block_number));
                }
                block::BibCoverage::Some(bib) => {
                    // Check if the BIB is encrypted
                    if self
                        .current_coverage(bib)
                        .is_some_and(|(_, bcb)| bcb.is_some())
                    {
                        return Err(Error::BibIsEncrypted(block_number));
                    }
                }
                block::BibCoverage::None => {}
            }
        }
        // Note: BCB case is fine - we can silently update the BCB's target list
        // without needing keys since we're just removing a target, not decrypting.

        self.remove_block_inner(block_number)
    }

    pub(crate) fn remove_block_inner(&mut self, block_number: u64) -> Result<(), Error> {
        // The block's current security references, read BEFORE removing it.
        let (bib, bcb) = self
            .current_coverage(block_number)
            .unwrap_or((block::BibCoverage::None, None));

        // Removing a BIB outright orphans its targets' coverage stamps —
        // `remove_from_bib_targets` only handles the target-removed
        // direction. Enumerate the OperationSet (readable only when the
        // body is plaintext) and clear each target still marked as covered
        // by this BIB. Targets of an *encrypted* BIB carry
        // `BibCoverage::Maybe` and keep it: another encrypted BIB may
        // cover them, so `Maybe` remains the accurate answer.
        //
        // The targets keep their CRC type as it stands: no security
        // operation changes a target's CRC.
        let mut bib_targets: Vec<u64> = Vec::new();
        if let Some((block, Some(payload))) = self.block(block_number)
            && matches!(block.block_type, block::Type::BlockIntegrity)
            && let Ok(opset) = parse_exact::<bpsec::bib::OperationSet>(payload)
        {
            bib_targets.extend(opset.operations.keys().copied());
        }
        for target in bib_targets {
            let covered_by_this = match self.bib_overrides.get(&target) {
                Some(cov) => matches!(cov, block::BibCoverage::Some(n) if *n == block_number),
                None => self.block(target).is_some_and(
                    |(b, _)| matches!(b.bib, block::BibCoverage::Some(n) if n == block_number),
                ),
            };
            if covered_by_this {
                self.bib_overrides.insert(target, block::BibCoverage::None);
            }
        }

        // Strip the block from its security blocks' target lists, then
        // remove it from the templates, so a failed strip returns an editor
        // that still holds the block.
        if self.blocks.contains_key(&block_number) {
            // If there is a BIB, remove the block from the list of targets
            // If the BIB is now empty, recursively call this function.
            if let block::BibCoverage::Some(bib) = bib {
                self.remove_from_bib_targets(block_number, bib)?;
            }

            // If there is a BCB, remove the block from the list of targets
            // If the BCB is now empty, recursively call this function.
            if let Some(bcb) = bcb {
                self.remove_from_bcb_targets(block_number, bcb)?;
            }
            self.blocks.remove(&block_number);
            self.generation += 1;
        }
        Ok(())
    }

    /// Remove a target block from a BIB's operation set.
    /// If the BIB becomes empty, recursively remove it.
    ///
    /// Reads the BIB body from the current template, so the caller must
    /// have ensured it's plaintext (e.g. via `BPSecEditor::remove_blocks`
    /// staging) — this function will not decrypt ciphertext OpSets.
    pub(crate) fn remove_from_bib_targets(
        &mut self,
        target_block: u64,
        bib_block: u64,
    ) -> Result<(), Error> {
        if let Some((_, Some(bib_payload))) = self.block(bib_block) {
            let mut opset = parse_exact::<bpsec::bib::OperationSet>(bib_payload).map_err(|e| {
                Error::from(error::Error::InvalidField {
                    field: "BIB Abstract Syntax Block",
                    source: Box::new(e.into()),
                })
            })?;

            // Remove the target from the BIB
            if opset.operations.remove(&target_block).is_some() {
                if opset.operations.is_empty() {
                    // BIB is now empty, recursively remove it
                    self.remove_block_inner(bib_block)?;
                } else {
                    // Rewrite BIB with updated operation set
                    self.update_block_inner(bib_block)?
                        .with_data(hardy_cbor::encode::emit(&opset).0.into())
                        .rebuild();
                }

                // The target is no longer covered by this BIB. Clear its
                // coverage so `rebuild_bundle()` does not report a dangling
                // reference to a BIB that has been removed or rewritten.
                self.bib_overrides
                    .insert(target_block, block::BibCoverage::None);
            }
        }
        Ok(())
    }

    /// Remove a target block from a BCB's operation set.
    /// If the BCB becomes empty, recursively remove it.
    pub(crate) fn remove_from_bcb_targets(
        &mut self,
        target_block: u64,
        bcb_block: u64,
    ) -> Result<(), Error> {
        if let Some((_, Some(bcb_payload))) = self.block(bcb_block) {
            let mut opset = parse_exact::<bpsec::bcb::OperationSet>(bcb_payload).map_err(|e| {
                Error::from(error::Error::InvalidField {
                    field: "BCB Abstract Syntax Block",
                    source: Box::new(e.into()),
                })
            })?;

            // Remove the target from the BCB
            if opset.operations.remove(&target_block).is_some() {
                if opset.operations.is_empty() {
                    // BCB is now empty, recursively remove it
                    self.remove_block_inner(bcb_block)?;
                } else {
                    // Rewrite BCB with updated operation set
                    self.update_block_inner(bcb_block)?
                        .with_data(hardy_cbor::encode::emit(&opset).0.into())
                        .rebuild();
                }

                // The target is no longer covered by this BCB. Clear its
                // coverage so `rebuild_bundle()` does not report a dangling
                // reference (mirrors `remove_from_bib_targets`).
                self.bcb_overrides.insert(target_block, None);
            }
        }
        Ok(())
    }

    /// Resolve a block number to its current `Block` header + payload
    /// view, accounting for any in-flight Keep / Update / Insert template.
    pub(crate) fn block(
        &'a self,
        block_number: u64,
    ) -> Option<(&'a block::Block, Option<&'a [u8]>)> {
        match self.blocks.get(&block_number)? {
            BlockTemplate::Keep(_) => {
                let block = self.original.blocks.get(&block_number)?;
                Some((block, block.payload(self.source_data)))
            }
            BlockTemplate::Update(template) | BlockTemplate::Insert(template) => {
                Some((&template.block, template.data.as_deref()))
            }
        }
    }

    /// Iterate the current set of block numbers in the editor's view.
    /// Used by `bpsec::edit` to scan for BCB-encrypted BIBs without
    /// exposing the template map.
    pub(crate) fn block_numbers(&self) -> impl Iterator<Item = u64> + '_ {
        self.blocks.keys().copied()
    }

    // `remove_integrity` and `remove_encryption` live on
    // [`crate::bpsec::edit::BPSecEditor`] (pull the trait into scope for
    // method syntax), reading the key source and plaintext cache set here:
    //
    //     use hardy_bpv7::bpsec::edit::BPSecEditor;
    //     editor.remove_integrity(block_number)?;
    //     let editor = editor.remove_encryption(block_number)?;

    /// Rebuild the bundle, applying all of the modifications.
    ///
    /// Returns the updated `Bundle` (with block extents and BPSec coverage
    /// pointing into the new data) and the serialized representation.
    ///
    /// BPSec coverage is correct by construction:
    /// - The public API prevents adding/updating security blocks
    /// - Cascade deletes preserve or remove security block references
    /// - Signer/Encryptor set bib/bcb overrides explicitly
    ///
    /// # Errors
    ///
    /// [`ReportOnFailureForbidden`](Error::ReportOnFailureForbidden) when the
    /// final primary forbids `report_on_failure` (RFC 9171 §4.2.3-4/-5) and
    /// a block carries it, whether an edit set the flag or a kept block
    /// carried it under the primary the edits replaced.
    pub fn rebuild_bundle(mut self) -> Result<(bundle::Bundle, Vec<Chunk>), Error> {
        self.check_report_on_failure()?;
        let mut blocks_out: HashMap<u64, block::Block> = HashMap::new();

        self.blocks.remove(&0).expect("No primary block!");

        // Build primary chunk + final PrimaryBlock for the output Bundle.
        let (primary_out, primary_chunk) = if let Some(primary) = self.primary.take() {
            let primary_bytes = primary.emit()?;
            let len = primary_bytes.len();
            // `extent` here is a placeholder — the offset-fixup loop in
            // `assemble` rewrites it once all chunks are placed. Only
            // `data` (= 0..len) needs to be correct in the meantime.
            blocks_out.insert(
                0,
                primary_block::PrimaryBlock::as_block(primary.crc_type, 0..len),
            );
            (primary, (0u64, Chunk::New(primary_bytes.into())))
        } else {
            // Keep original primary verbatim.
            let block = self
                .original
                .blocks
                .get(&0)
                .ok_or(Error::from(error::Error::Altered))?;
            blocks_out.insert(0, block.clone());
            let extent = usize::try_from(block.extent.start)
                .and_then(|s| usize::try_from(block.extent.end).map(|e| s..e))
                .map_err(|_| Error::from(error::Error::Altered))?;
            (
                self.original.primary.clone(),
                (0u64, Chunk::Unchanged(extent)),
            )
        };

        let payload_block = self.blocks.remove(&1).expect("No payload block!");

        // Build extension chunks. Extension-field extraction (previous_node /
        // age / hop_count) is not Editor's job — `Bundle`
        // doesn't carry those slots. Callers that want the extracted values
        // re-interpret the rebuilt blocks themselves (a hardy-bpa concern).
        let mut ext_chunks = Vec::with_capacity(self.blocks.len());
        for (block_number, block_template) in core::mem::take(&mut self.blocks) {
            let (block, chunk) = self.build_chunk(block_number, block_template)?;
            blocks_out.insert(block_number, block);
            ext_chunks.push((block_number, chunk));
        }

        // Build payload chunk
        let (block, payload_chunk) = self.build_chunk(1, payload_block)?;
        blocks_out.insert(1, block);

        // Apply security metadata overrides from Signer/Encryptor
        for (block_number, bib) in &self.bib_overrides {
            if let Some(block) = blocks_out.get_mut(block_number) {
                block.bib = *bib;
            }
        }
        for (block_number, bcb) in &self.bcb_overrides {
            if let Some(block) = blocks_out.get_mut(block_number) {
                block.bcb = *bcb;
            }
        }

        let mut bundle_out = bundle::Bundle {
            primary: primary_out,
            blocks: blocks_out,
        };

        // Assemble, order, set extents, and merge
        let chunks = Chunk::assemble(
            primary_chunk,
            ext_chunks,
            (1, payload_chunk),
            Some(&mut bundle_out),
        );

        Ok((bundle_out, chunks))
    }

    /// Rebuild a bundle whose payload streams past the editor
    /// ([`with_streamed_payload`](Self::with_streamed_payload)): every
    /// header as [`rebuild_bundle`](Self::rebuild_bundle) builds it, then
    /// the payload block's header for the declared plaintext length, with
    /// the payload block's index entry spanning its full wire form. The
    /// caller writes the outer array head (`0x9F`), the chunks, the
    /// plaintext as it streams, feeding each run to the returned
    /// [`PayloadTrailer`](builder::PayloadTrailer), and the bytes the
    /// trailer's [`finish`](builder::PayloadTrailer::finish) returns: the
    /// payload's CRC field, if it carries one, and the outer break.
    ///
    /// # Errors
    ///
    /// As [`rebuild_bundle`](Self::rebuild_bundle);
    /// [`NoBlockData`](builder::Error::NoBlockData) when the editor was not
    /// told the payload streams, or the payload is encrypted and its
    /// encryption was not removed for it;
    /// [`StagedPayload`](Error::StagedPayload) when the payload's data was
    /// staged, which cannot stream; and
    /// [`PayloadLengthOverflow`](builder::Error::PayloadLengthOverflow)
    /// for a declared length whose wire form would pass `u64::MAX`.
    #[allow(clippy::type_complexity)]
    pub fn rebuild_streamed(
        mut self,
    ) -> Result<(bundle::Bundle, Vec<Chunk>, builder::PayloadTrailer), Error> {
        let plaintext_len = self
            .streamed_payload
            .ok_or(Error::Builder(builder::Error::NoBlockData))?;
        // The payload streams in plaintext: an encrypted payload only once
        // its encryption is removed for the stream, since its received data
        // is ciphertext. A payload received in plaintext streams as it
        // stands, whatever else was edited around it.
        let payload = match self.blocks.get(&1) {
            Some(BlockTemplate::Update(template)) if template.data.is_none() => {
                template.block.clone()
            }
            // Staged data never reaches the wire of a streamed rebuild:
            // refused rather than dropped.
            Some(BlockTemplate::Update(_)) => return Err(Error::StagedPayload),
            Some(BlockTemplate::Keep(_)) => {
                let payload = self
                    .original
                    .blocks
                    .get(&1)
                    .ok_or(Error::from(error::Error::Altered))?;
                if payload.bcb.is_some() {
                    return Err(Error::Builder(builder::Error::NoBlockData));
                }
                payload.clone()
            }
            _ => return Err(Error::Builder(builder::Error::NoBlockData)),
        };
        let (header, trailer) =
            builder::streamed_payload_header(&payload.flags, payload.crc_type, plaintext_len)?;
        // Every header but the payload rebuilds as usual, the payload's
        // header standing in for its block.
        self.blocks.insert(
            1,
            BlockTemplate::Update(builder::BlockTemplate::new(
                block::Type::Payload,
                payload.flags.clone(),
                payload.crc_type,
                Some(Cow::Borrowed(&[])),
            )),
        );
        let (mut bundle, mut chunks) = self.rebuild_bundle()?;
        let last = chunks.pop().expect("the payload's chunk is last");
        debug_assert!(matches!(last, Chunk::New(_)));
        let extent_start = bundle.blocks[&1].extent.start;
        let mut entry = builder::streamed_payload_block(
            payload.flags,
            payload.crc_type,
            header.len() as u64,
            extent_start,
            plaintext_len,
        )?;
        entry.bib = bundle.blocks[&1].bib;
        entry.bcb = bundle.blocks[&1].bcb;
        bundle.blocks.insert(1, entry);
        chunks.push(Chunk::New(header.into()));
        Ok((bundle, chunks, trailer))
    }

    /// Rebuild the bundle as a list of chunks.
    ///
    /// `Chunk::Unchanged` references ranges in the original `source_data`,
    /// `Chunk::New` contains freshly encoded bytes. Use `Chunk::flatten()`
    /// to concatenate into contiguous bytes.
    ///
    /// # Errors
    ///
    /// [`ReportOnFailureForbidden`](Error::ReportOnFailureForbidden), as
    /// [`rebuild_bundle`](Self::rebuild_bundle) refuses it.
    pub fn rebuild(mut self) -> Result<Vec<Chunk>, Error> {
        self.check_report_on_failure()?;
        self.blocks.remove(&0).expect("No primary block!");

        // Build primary chunk
        let primary_chunk = if let Some(primary) = self.primary.take() {
            (0u64, Chunk::New(primary.emit()?.into()))
        } else {
            let block = self
                .original
                .blocks
                .get(&0)
                .ok_or(Error::from(error::Error::Altered))?;
            let extent = usize::try_from(block.extent.start)
                .and_then(|s| usize::try_from(block.extent.end).map(|e| s..e))
                .map_err(|_| Error::from(error::Error::Altered))?;
            (0u64, Chunk::Unchanged(extent))
        };

        let payload_block = self.blocks.remove(&1).expect("No payload block!");

        // Build extension chunks
        let mut ext_chunks = Vec::with_capacity(self.blocks.len());
        for (block_number, block_template) in core::mem::take(&mut self.blocks) {
            let (_, chunk) = self.build_chunk(block_number, block_template)?;
            ext_chunks.push((block_number, chunk));
        }

        // Build payload chunk
        let (_, payload_chunk) = self.build_chunk(1, payload_block)?;

        // Assemble, order, and merge (no extent tracking)
        Ok(Chunk::assemble(
            primary_chunk,
            ext_chunks,
            (1, payload_chunk),
            None,
        ))
    }

    // RFC 9171 §4.2.3-4/-5 against the final primary: checked when the
    // bundle is rebuilt, not at the setters, since the primary can change
    // after a block flag is set. Block 0's flags are nominal and skipped.
    // The lowest flagged block is named, so the error does not depend on
    // the map's iteration order.
    fn check_report_on_failure(&self) -> Result<(), Error> {
        let primary = self.primary.as_ref().unwrap_or(&self.original.primary);
        if !primary.forbids_report_on_failure() {
            return Ok(());
        }
        let flagged = self
            .blocks
            .iter()
            .filter(|&(&block_number, template)| {
                block_number != 0
                    && match template {
                        BlockTemplate::Update(template) | BlockTemplate::Insert(template) => {
                            template.block.flags.report_on_failure
                        }
                        BlockTemplate::Keep(_) => self
                            .original
                            .blocks
                            .get(&block_number)
                            .is_some_and(|block| block.flags.report_on_failure),
                    }
            })
            .map(|(&block_number, _)| block_number)
            .min();
        match flagged {
            Some(block_number) => Err(Error::ReportOnFailureForbidden(block_number)),
            None => Ok(()),
        }
    }

    fn build_chunk(
        &self,
        block_number: u64,
        template: BlockTemplate,
    ) -> Result<(block::Block, Chunk), Error> {
        if let BlockTemplate::Update(template) | BlockTemplate::Insert(template) = template {
            let (block, bytes) = template.build_to_vec(block_number).map_err(Error::from)?;
            Ok((block, Chunk::New(bytes.into())))
        } else {
            let block = self
                .original
                .blocks
                .get(&block_number)
                .ok_or(Error::from(error::Error::Altered))?
                .clone();
            let extent = usize::try_from(block.extent.start)
                .and_then(|s| usize::try_from(block.extent.end).map(|e| s..e))
                .map_err(|_| Error::from(error::Error::Altered))?;
            Ok((block, Chunk::Unchanged(extent)))
        }
    }
}

impl<'e, 'a> BlockBuilder<'e, 'a> {
    fn new(editor: &'e mut Editor<'a>, block_number: u64, block_type: block::Type) -> Self {
        Self {
            template: builder::BlockTemplate::new(
                block_type,
                block::Flags::default(),
                editor.original.primary.crc_type,
                None,
            ),
            is_new: true,
            block_number,
            editor,
        }
    }

    fn reuse_template(
        editor: &'e mut Editor<'a>,
        block_number: u64,
        is_new: bool,
        template: builder::BlockTemplate<'a>,
    ) -> Self {
        Self {
            template,
            block_number,
            is_new,
            editor,
        }
    }

    /// Set the `Flags` for this block, canonicalized so an alias bit in
    /// `unrecognised` counts as the flag it encodes.
    pub fn with_flags(mut self, flags: block::Flags) -> Self {
        self.template.block.flags = flags.canonicalize();
        self
    }

    /// Set the `CrcType` for this block.
    pub fn with_crc_type(mut self, crc_type: crc::CrcType) -> Self {
        self.template.block.crc_type = crc_type;
        self
    }

    /// Set the payload data for this block.
    pub fn with_data(mut self, data: Cow<'a, [u8]>) -> Self {
        self.template.data = Some(data);
        self
    }

    /// Get the block number for this block.
    pub fn block_number(&self) -> u64 {
        self.block_number
    }

    /// Stages the block into the editor.
    pub fn rebuild(self) {
        self.editor.blocks.insert(
            self.block_number,
            if self.is_new {
                BlockTemplate::Insert(self.template)
            } else {
                BlockTemplate::Update(self.template)
            },
        );
        self.editor.generation += 1;
    }
}

/// An editor reads as the bundle its staged edits leave: a staged block's
/// data, or a kept block's wire bytes, each plain. A read borrows the
/// editor only for its own duration, so edits follow it. The headers keep
/// their parsed coverage stamps; [`Editor::current_bundle`] re-stamps them.
/// A staged block given no data yet reads as
/// [`NotResident`](Availability::NotResident): no state says "staged without
/// data", and the rebuild refuses such a block with
/// [`NoBlockData`](builder::Error::NoBlockData).
impl<'a> Reader<'a> for Editor<'a> {
    fn block(&'a self, block_number: u64) -> Option<(&'a block::Block, Availability<'a>)> {
        let (block, payload) = Editor::block(self, block_number)?;
        Some((
            block,
            payload
                .map(block::Payload::Borrowed)
                .map_or(Availability::NotResident, Availability::Available),
        ))
    }
}

// The accessors the BPSec editor's keyed paths need (`bpsec::edit::keyed`).
#[cfg(feature = "bpsec")]
impl<'a> Editor<'a> {
    /// The payload plaintext's length, where it streams
    /// ([`with_streamed_payload`](Self::with_streamed_payload)).
    pub(crate) fn streamed_payload(&self) -> Option<u64> {
        self.streamed_payload
    }

    /// Record that a BIB covers the given target block.
    ///
    /// Used by `Signer` to set `bib` metadata on target blocks so that
    /// `rebuild_bundle()` returns a correct `Bundle` without reparsing, and
    /// by `BPSecEditor::remove_blocks` once a decrypt shows which targets an
    /// encrypted BIB covers.
    pub(crate) fn set_bib_target(&mut self, target_block: u64, bib_block: u64) {
        self.bib_overrides
            .insert(target_block, block::BibCoverage::Some(bib_block));
    }

    /// Record that no BIB covers the given target block: used by
    /// `BPSecEditor::remove_encryption` once decrypting the bundle's
    /// encrypted BIBs shows that none covers a block read as `Maybe`.
    pub(crate) fn clear_bib_target(&mut self, target_block: u64) {
        self.bib_overrides
            .insert(target_block, block::BibCoverage::None);
    }

    /// Record that a BCB covers the given target block.
    ///
    /// Used by `Encryptor` to set `bcb` metadata on target blocks so that
    /// `rebuild_bundle()` returns a correct `Bundle` without reparsing.
    pub(crate) fn set_bcb_target(&mut self, target_block: u64, bcb_block: u64) {
        self.bcb_overrides.insert(target_block, Some(bcb_block));
    }
}
