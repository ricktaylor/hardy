use super::*;
use alloc::borrow::Cow;
use bytes::Bytes;
use core::ops::Range;
use thiserror::Error;

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

    #[error(
        "Cannot decrypt BIB {0} that targets the decrypted block; this would violate RFC 9172 §3.8"
    )]
    CannotDecryptRelatedBib(u64),

    #[error("Security blocks (BIB/BCB) must be managed via Signer/Encryptor, not the Editor")]
    SecurityBlock,

    #[error(transparent)]
    Builder(#[from] builder::Error),
}

impl From<bpsec::Error> for Error {
    fn from(e: bpsec::Error) -> Self {
        Error::Builder(builder::Error::InternalError(e.into()))
    }
}

impl From<error::Error> for Error {
    fn from(e: error::Error) -> Self {
        Error::Builder(builder::Error::InternalError(e))
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
    /// indefinite-length array (0x9F prefix, 0xFF suffix).
    pub fn flatten(chunks: Vec<Self>, source: &[u8]) -> Box<[u8]> {
        let total_len = 2 + chunks.iter().map(|c| c.len()).sum::<usize>();
        let mut result = Vec::with_capacity(total_len);
        result.push(0x9F);
        for c in chunks {
            match c {
                Chunk::Unchanged(extent) => {
                    debug_assert!(extent.end <= source.len());
                    result.extend_from_slice(&source[extent]);
                }
                Chunk::New(items) => {
                    result.extend(items);
                }
            }
        }
        result.push(0xFF);
        debug_assert_eq!(result.len(), total_len);
        result.into()
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
        let mut unchanged: Vec<(u64, Range<usize>)> = Vec::new();
        let mut new_chunks: Vec<(u64, Box<[u8]>)> = Vec::new();

        for (block_number, chunk) in extensions {
            match chunk {
                Chunk::Unchanged(range) => unchanged.push((block_number, range)),
                Chunk::New(data) => new_chunks.push((block_number, data)),
            }
        }

        // Sort unchanged by source position
        unchanged.sort_by_key(|(_, range)| range.start);

        // Sort new chunks by size descending for best-fit gap filling
        new_chunks.sort_by_key(|(_, b)| core::cmp::Reverse(b.len()));

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
}

enum BlockTemplate<'a> {
    Keep(block::Type),
    Update(builder::BlockTemplate<'a>),
    Insert(builder::BlockTemplate<'a>),
}

/// The `BlockBuilder` is used to construct a new or replacement block for a
/// bundle.
pub struct BlockBuilder<'a> {
    editor: Editor<'a>,
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
        }
    }

    fn primary_block(&mut self) -> Result<&mut primary_block::PrimaryBlock, Error> {
        // Check if primary block is still protected by an untouched BIB
        if let Some(primary) = self.original.blocks.get(&0) {
            match primary.bib {
                block::BibCoverage::Some(bib_num)
                    if matches!(self.blocks.get(&bib_num), Some(BlockTemplate::Keep(_))) =>
                {
                    return Err(Error::PrimaryBlockHasBib);
                }
                block::BibCoverage::Maybe => {
                    return Err(bpsec::Error::MaybeHasBib(0).into());
                }
                _ => {}
            }
        }

        if self.primary.is_none() {
            self.primary = Some(self.original.primary.clone());
        }
        Ok(self.primary.as_mut().unwrap())
    }

    /// Sets the bundle flags for this [`Editor`].
    ///
    /// On error, returns the editor along with the error so it can be reused for recovery.
    #[allow(clippy::result_large_err)]
    pub fn with_bundle_flags(mut self, flags: bundle::Flags) -> Result<Self, (Self, Error)> {
        match self.primary_block() {
            Ok(pb) => {
                pb.flags = flags;
                Ok(self)
            }
            Err(e) => Err((self, e)),
        }
    }

    /// Sets the [`crc::CrcType`] for this [`Editor`].
    ///
    /// On error, returns the editor along with the error so it can be reused for recovery.
    #[allow(clippy::result_large_err)]
    pub fn with_bundle_crc_type(mut self, crc_type: crc::CrcType) -> Result<Self, (Self, Error)> {
        match self.primary_block() {
            Ok(pb) => {
                pb.crc_type = crc_type;
                Ok(self)
            }
            Err(e) => Err((self, e)),
        }
    }

    /// Sets the creation timestamp for this [`Editor`].
    ///
    /// On error, returns the editor along with the error so it can be reused for recovery.
    #[allow(clippy::result_large_err)]
    pub fn with_timestamp(
        mut self,
        timestamp: creation_timestamp::CreationTimestamp,
    ) -> Result<Self, (Self, Error)> {
        match self.primary_block() {
            Ok(pb) => {
                pb.id.timestamp = timestamp;
                Ok(self)
            }
            Err(e) => Err((self, e)),
        }
    }

    /// Sets the source [`eid::Eid`] for this [`Editor`].
    ///
    /// On error, returns the editor along with the error so it can be reused for recovery.
    #[allow(clippy::result_large_err)]
    pub fn with_source(mut self, source: eid::Eid) -> Result<Self, (Self, Error)> {
        match self.primary_block() {
            Ok(pb) => {
                pb.id.source = source;
                Ok(self)
            }
            Err(e) => Err((self, e)),
        }
    }

    /// Sets the destination [`eid::Eid`] for this [`Editor`].
    ///
    /// On error, returns the editor along with the error so it can be reused for recovery.
    #[allow(clippy::result_large_err)]
    pub fn with_destination(mut self, destination: eid::Eid) -> Result<Self, (Self, Error)> {
        match self.primary_block() {
            Ok(pb) => {
                pb.destination = destination;
                Ok(self)
            }
            Err(e) => Err((self, e)),
        }
    }

    /// Sets the report_to [`eid::Eid`] for this [`Editor`].
    ///
    /// On error, returns the editor along with the error so it can be reused for recovery.
    #[allow(clippy::result_large_err)]
    pub fn with_report_to(mut self, report_to: eid::Eid) -> Result<Self, (Self, Error)> {
        match self.primary_block() {
            Ok(pb) => {
                pb.report_to = report_to;
                Ok(self)
            }
            Err(e) => Err((self, e)),
        }
    }

    /// Sets the lifetime for this [`Editor`].
    ///
    /// On error, returns the editor along with the error so it can be reused for recovery.
    #[allow(clippy::result_large_err)]
    pub fn with_lifetime(mut self, lifetime: core::time::Duration) -> Result<Self, (Self, Error)> {
        match self.primary_block() {
            Ok(pb) => {
                pb.lifetime = lifetime.min(core::time::Duration::from_millis(u64::MAX));
                Ok(self)
            }
            Err(e) => Err((self, e)),
        }
    }

    /// Sets the fragment_info for this [`Editor`].
    ///
    /// On error, returns the editor along with the error so it can be reused for recovery.
    #[allow(clippy::result_large_err)]
    pub fn with_fragment_info(
        mut self,
        fragment_info: Option<bundle::FragmentInfo>,
    ) -> Result<Self, (Self, Error)> {
        match self.primary_block() {
            Ok(pb) => {
                pb.id.fragment_info = fragment_info;
                Ok(self)
            }
            Err(e) => Err((self, e)),
        }
    }

    /// Add a new block into the bundle.
    ///
    /// The new block will be assigned the next available block
    /// number.  Be very careful about adding duplicate blocks that should not be duplicated
    ///
    /// On error, returns the editor along with the error so it can be reused for recovery.
    #[allow(clippy::result_large_err)]
    pub fn push_block(self, block_type: block::Type) -> Result<BlockBuilder<'a>, (Self, Error)> {
        match block_type {
            block::Type::Primary => {
                return Err((self, Error::PrimaryBlock));
            }
            block::Type::BlockIntegrity | block::Type::BlockSecurity => {
                return Err((self, Error::SecurityBlock));
            }
            block::Type::Payload
            | block::Type::BundleAge
            | block::Type::HopCount
            | block::Type::PreviousNode => {
                for template in self.blocks.values() {
                    match template {
                        BlockTemplate::Keep(t) if t == &block_type => {
                            return Err((self, Error::IllegalDuplicate(block_type)));
                        }
                        BlockTemplate::Insert(template) | BlockTemplate::Update(template)
                            if template.block.block_type == block_type =>
                        {
                            return Err((self, Error::IllegalDuplicate(block_type)));
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
    #[allow(clippy::result_large_err)]
    pub(crate) fn alloc_block(
        self,
        block_type: block::Type,
    ) -> Result<BlockBuilder<'a>, (Self, Error)> {
        let mut block_number = 2u64;
        while self.blocks.contains_key(&block_number) {
            block_number = match block_number.checked_add(1) {
                Some(n) => n,
                None => return Err((self, Error::OutOfBlockNumbers)),
            };
        }
        Ok(BlockBuilder::new(self, block_number, block_type))
    }

    /// Insert a new block into the bundle.
    ///
    /// If a block of the same type already exists, the new block will replace
    /// it. Otherwise, the new block will be assigned the next available block
    /// number.
    ///
    /// On error, returns the editor along with the error so it can be reused for recovery.
    #[allow(clippy::result_large_err)]
    pub fn insert_block(self, block_type: block::Type) -> Result<BlockBuilder<'a>, (Self, Error)> {
        match block_type {
            block::Type::Primary => return Err((self, Error::PrimaryBlock)),
            block::Type::BlockIntegrity | block::Type::BlockSecurity => {
                return Err((self, Error::SecurityBlock));
            }
            _ => {}
        }

        if let Some((block_number, is_new, template)) =
            self.blocks
                .iter()
                .find_map(|(block_number, template)| match template {
                    BlockTemplate::Keep(t) if &block_type == t => {
                        let block = self.original.blocks.get(block_number)?;
                        Some((
                            *block_number,
                            false,
                            builder::BlockTemplate::new(
                                *t,
                                block.flags.clone(),
                                block.crc_type,
                                block.payload(self.source_data).map(Cow::Borrowed),
                            ),
                        ))
                    }
                    BlockTemplate::Insert(template) if template.block.block_type == block_type => {
                        Some((*block_number, true, template.clone()))
                    }
                    BlockTemplate::Update(template) if template.block.block_type == block_type => {
                        Some((*block_number, false, template.clone()))
                    }
                    _ => None,
                })
        {
            return Ok(BlockBuilder::reuse_template(
                self,
                block_number,
                is_new,
                template,
            ));
        }

        self.alloc_block(block_type)
    }

    /// Update an existing block in the bundle.
    ///
    /// This will return a `BlockBuilder` that can be used to manipulate the
    /// existing block. If the block is a security target of a BIB or BCB, it
    /// will be automatically removed from those target lists first (since the
    /// signature/encryption would be invalid after modification).
    ///
    /// On error, returns the editor along with the error so it can be reused for recovery.
    #[allow(clippy::result_large_err)]
    pub fn update_block(mut self, block_number: u64) -> Result<BlockBuilder<'a>, (Self, Error)> {
        // Check block type and get security references in one lookup
        let (bib, bcb) = match self.block(block_number) {
            Some((block, _)) => {
                match block.block_type {
                    block::Type::Primary => {
                        return Err((self, Error::PrimaryBlock));
                    }
                    block::Type::BlockIntegrity | block::Type::BlockSecurity => {
                        return Err((self, Error::SecurityBlock));
                    }
                    _ => {}
                }
                (block.bib.clone(), block.bcb)
            }
            None => return Err((self, Error::NoSuchBlock(block_number))),
        };

        // Handle BIB coverage — must remove from target list if present
        match bib {
            block::BibCoverage::Maybe => {
                return Err((self, bpsec::Error::MaybeHasBib(block_number).into()));
            }
            block::BibCoverage::Some(bib_num) => {
                if let Some((bib_block, _)) = self.block(bib_num)
                    && bib_block.bcb.is_some()
                {
                    return Err((self, Error::BibIsEncrypted(block_number)));
                }
                self = self.remove_from_bib_targets(block_number, bib_num)?;
            }
            block::BibCoverage::None => {}
        }

        // Remove from BCB target list if present
        if let Some(bcb_num) = bcb {
            self = self.remove_from_bcb_targets(block_number, bcb_num)?;
        }

        self.update_block_inner(block_number)
    }

    /// Record that a BIB covers the given target block.
    ///
    /// Used by `Signer` to set `bib` metadata on target blocks so that
    /// `rebuild_bundle()` returns a correct `Bundle` without reparsing.
    #[cfg(feature = "bpsec")]
    pub(crate) fn set_bib_target(&mut self, target_block: u64, bib_block: u64) {
        self.bib_overrides
            .insert(target_block, block::BibCoverage::Some(bib_block));
    }

    /// Record that a BCB covers the given target block.
    ///
    /// Used by `Encryptor` to set `bcb` metadata on target blocks so that
    /// `rebuild_bundle()` returns a correct `Bundle` without reparsing.
    #[cfg(feature = "bpsec")]
    pub(crate) fn set_bcb_target(&mut self, target_block: u64, bcb_block: u64) {
        self.bcb_overrides.insert(target_block, Some(bcb_block));
    }

    /// Replace the primary block with a caller-supplied canonical encoding.
    ///
    /// Used by `Signer` for RFC 9173 Section 3.8.1 CRC removal: `primary` is
    /// the CRC-stripped `PrimaryBlock` used on rebuild, and `data` is its
    /// canonical encoding, served by `block(0)` so the IPPT and the rebuilt
    /// bundle agree on the CRC-removed form. Bypasses the BIB-protection
    /// check — the caller is adding that very BIB.
    #[cfg(feature = "bpsec")]
    pub(crate) fn set_canonical_primary(
        &mut self,
        primary: primary_block::PrimaryBlock,
        data: Cow<'a, [u8]>,
    ) {
        self.blocks.insert(
            0,
            BlockTemplate::Update(builder::BlockTemplate::new(
                block::Type::Primary,
                block::Flags::default(),
                crc::CrcType::None,
                Some(data),
            )),
        );
        self.primary = Some(primary);
    }

    /// Update an existing block without automatic security target removal.
    ///
    /// This is for internal use by encryptor/signer which need to update blocks
    /// that are security targets without removing them from those target lists.
    #[allow(clippy::result_large_err)]
    pub(crate) fn update_block_inner(
        self,
        block_number: u64,
    ) -> Result<BlockBuilder<'a>, (Self, Error)> {
        let (is_new, template) = match self.blocks.get(&block_number) {
            None => return Err((self, Error::NoSuchBlock(block_number))),
            Some(BlockTemplate::Keep(t)) => {
                if let &block::Type::Primary = t {
                    return Err((self, Error::PrimaryBlock));
                }
                let block = match self.original.blocks.get(&block_number) {
                    Some(b) => b,
                    None => return Err((self, Error::NoSuchBlock(block_number))),
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
                // would miss the protecting BCB and leave it orphaned.
                tmpl.block.bib = block.bib.clone();
                tmpl.block.bcb = block.bcb;
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
    /// Note that the primary and payload blocks cannot be removed.
    ///
    /// On error, returns the editor along with the error so it can be reused for recovery.
    #[allow(clippy::result_large_err)]
    pub fn remove_block(self, block_number: u64) -> Result<Self, (Self, Error)> {
        if block_number == 0 {
            return Err((self, Error::PrimaryBlock));
        }
        if block_number == 1 {
            return Err((self, Error::PayloadBlock));
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
                return Err((self, Error::SecurityBlock));
            }

            match block.bib {
                block::BibCoverage::Maybe => {
                    return Err((self, bpsec::Error::MaybeHasBib(block_number).into()));
                }
                block::BibCoverage::Some(bib) => {
                    // Check if the BIB is encrypted
                    if let Some((bib_block, _)) = self.block(bib)
                        && bib_block.bcb.is_some()
                    {
                        return Err((self, Error::BibIsEncrypted(block_number)));
                    }
                }
                block::BibCoverage::None => {}
            }
        }
        // Note: BCB case is fine - we can silently update the BCB's target list
        // without needing keys since we're just removing a target, not decrypting.

        self.remove_block_inner(block_number)
    }

    #[allow(clippy::result_large_err)]
    pub(crate) fn remove_block_inner(mut self, block_number: u64) -> Result<Self, (Self, Error)> {
        // Get the block's security references BEFORE removing it
        let (bib, bcb) = if let Some((block, _)) = self.block(block_number) {
            (block.bib.clone(), block.bcb)
        } else {
            (block::BibCoverage::None, None)
        };

        // Now remove the block from the templates
        if self.blocks.remove(&block_number).is_some() {
            // If there is a BIB, remove the block from the list of targets
            // If the BIB is now empty, recursively call this function.
            if let block::BibCoverage::Some(bib) = bib {
                self = self.remove_from_bib_targets(block_number, bib)?;
            }

            // If there is a BCB, remove the block from the list of targets
            // If the BCB is now empty, recursively call this function.
            if let Some(bcb) = bcb {
                self = self.remove_from_bcb_targets(block_number, bcb)?;
            }
        }
        Ok(self)
    }

    /// Remove a target block from a BIB's operation set.
    /// If the BIB becomes empty, recursively remove it.
    ///
    /// Reads the BIB body from the current template, so the caller must
    /// have ensured it's plaintext (e.g. via `BPSecEditor::remove_blocks`
    /// staging) — this function will not decrypt ciphertext OpSets.
    #[allow(clippy::result_large_err)]
    pub(crate) fn remove_from_bib_targets(
        mut self,
        target_block: u64,
        bib_block: u64,
    ) -> Result<Self, (Self, Error)> {
        if let Some((_, Some(bib_payload))) = self.block(bib_block) {
            let mut opset = match hardy_cbor::decode::parse::<bpsec::bib::OperationSet>(bib_payload)
            {
                Ok(opset) => opset,
                Err(e) => {
                    return Err((
                        self,
                        error::Error::InvalidField {
                            field: "BIB Abstract Syntax Block",
                            source: e.into(),
                        }
                        .into(),
                    ));
                }
            };

            // Remove the target from the BIB
            if opset.operations.remove(&target_block).is_some() {
                if opset.operations.is_empty() {
                    // BIB is now empty, recursively remove it
                    self = self.remove_block_inner(bib_block)?;
                } else {
                    // Rewrite BIB with updated operation set
                    self = self
                        .update_block_inner(bib_block)?
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
        Ok(self)
    }

    /// Remove a target block from a BCB's operation set.
    /// If the BCB becomes empty, recursively remove it.
    #[allow(clippy::result_large_err)]
    fn remove_from_bcb_targets(
        mut self,
        target_block: u64,
        bcb_block: u64,
    ) -> Result<Self, (Self, Error)> {
        if let Some((_, Some(bcb_payload))) = self.block(bcb_block) {
            let mut opset = match hardy_cbor::decode::parse::<bpsec::bcb::OperationSet>(bcb_payload)
            {
                Ok(opset) => opset,
                Err(e) => {
                    return Err((
                        self,
                        error::Error::InvalidField {
                            field: "BCB Abstract Syntax Block",
                            source: e.into(),
                        }
                        .into(),
                    ));
                }
            };

            // Remove the target from the BCB
            if opset.operations.remove(&target_block).is_some() {
                if opset.operations.is_empty() {
                    // BCB is now empty, recursively remove it
                    self = self.remove_block_inner(bcb_block)?;
                } else {
                    // Rewrite BCB with updated operation set
                    self = self
                        .update_block_inner(bcb_block)?
                        .with_data(hardy_cbor::encode::emit(&opset).0.into())
                        .rebuild();
                }
            }
        }
        Ok(self)
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

    // `remove_integrity` lives on [`crate::bpsec::edit::BPSecEditor`]
    // (pull the trait into scope for method syntax); `remove_encryption`
    // is the free fn [`crate::bpsec::edit::remove_encryption`]. Editor
    // itself stays BPSec-agnostic (no `KeySource`):
    //
    //     use hardy_bpv7::bpsec::edit::BPSecEditor;
    //     let editor = editor.remove_integrity(block_number)?;
    //     let editor = bpsec::edit::remove_encryption(editor, block_number, &keys)?;

    /// Rebuild the bundle, applying all of the modifications.
    ///
    /// Returns the updated `Bundle` (with block extents and BPSec coverage
    /// pointing into the new data) and the serialized representation.
    ///
    /// BPSec coverage is correct by construction:
    /// - The public API prevents adding/updating security blocks
    /// - Cascade deletes preserve or remove security block references
    /// - Signer/Encryptor set bib/bcb overrides explicitly
    pub fn rebuild_bundle(mut self) -> Result<(bundle::Bundle, Vec<Chunk>), Error> {
        let mut blocks_out: HashMap<u64, block::Block> = HashMap::new();

        let primary_block = self.blocks.remove(&0).expect("No primary block!");

        // Build primary chunk + final PrimaryBlock for the output Bundle.
        let (primary_out, primary_chunk) = if let Some(mut primary) = self.primary.take() {
            // is_fragment must match fragment_info presence.
            primary.flags.is_fragment = primary.id.fragment_info.is_some();
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
        } else if let BlockTemplate::Update(template) = primary_block {
            // Caller supplied raw primary bytes directly (e.g. via the
            // BPSec edit path). Reuse the original PrimaryBlock metadata.
            let primary_bytes = template
                .data
                .ok_or(Error::Builder(builder::Error::NoBlockData))?;
            let len = primary_bytes.len();
            blocks_out.insert(
                0,
                primary_block::PrimaryBlock::as_block(self.original.primary.crc_type, 0..len),
            );
            (
                self.original.primary.clone(),
                (0u64, Chunk::New(primary_bytes.into_owned().into())),
            )
        } else {
            // Keep original primary verbatim.
            let block = self
                .original
                .blocks
                .get(&0)
                .ok_or(Error::from(error::Error::Altered))?;
            blocks_out.insert(0, block.clone());
            (
                self.original.primary.clone(),
                (
                    0u64,
                    Chunk::Unchanged(block.extent.start as usize..block.extent.end as usize),
                ),
            )
        };

        let payload_block = self.blocks.remove(&1).expect("No payload block!");

        // Build extension chunks. Extension-field extraction (previous_node /
        // age / hop_count) is not Editor's job — `Bundle`
        // doesn't carry those slots. Callers that want the extracted values
        // re-interpret the rebuilt blocks themselves (a hardy-bpa concern).
        let mut ext_chunks = Vec::new();
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
                block.bib = bib.clone();
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

    /// Rebuild the bundle as a list of chunks.
    ///
    /// `Chunk::Unchanged` references ranges in the original `source_data`,
    /// `Chunk::New` contains freshly encoded bytes. Use `Chunk::flatten()`
    /// to concatenate into contiguous bytes.
    pub fn rebuild(mut self) -> Result<Vec<Chunk>, Error> {
        let primary_block = self.blocks.remove(&0).expect("No primary block!");

        // Build primary chunk
        let primary_chunk = if let Some(mut primary) = self.primary.take() {
            primary.flags.is_fragment = primary.id.fragment_info.is_some();
            (0u64, Chunk::New(primary.emit()?.into()))
        } else if let BlockTemplate::Update(template) = primary_block {
            let data = template
                .data
                .ok_or(Error::Builder(builder::Error::NoBlockData))?;
            (0u64, Chunk::New(data.into_owned().into()))
        } else {
            let block = self
                .original
                .blocks
                .get(&0)
                .ok_or(Error::from(error::Error::Altered))?;
            (
                0u64,
                Chunk::Unchanged(block.extent.start as usize..block.extent.end as usize),
            )
        };

        let payload_block = self.blocks.remove(&1).expect("No payload block!");

        // Build extension chunks
        let mut ext_chunks = Vec::new();
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
            let extent = block.extent.start as usize..block.extent.end as usize;
            Ok((block, Chunk::Unchanged(extent)))
        }
    }
}

impl<'a> BlockBuilder<'a> {
    fn new(editor: Editor<'a>, block_number: u64, block_type: block::Type) -> Self {
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
        editor: Editor<'a>,
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

    /// Set the `Flags` for this block.
    pub fn with_flags(mut self, flags: block::Flags) -> Self {
        self.template.block.flags = flags;
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

    /// Build the block and return the modified `Editor`.
    pub fn rebuild(mut self) -> Editor<'a> {
        self.editor.blocks.insert(
            self.block_number,
            if self.is_new {
                BlockTemplate::Insert(self.template)
            } else {
                BlockTemplate::Update(self.template)
            },
        );

        self.editor
    }
}

pub(crate) struct EditorBlockSet<'a> {
    pub editor: Editor<'a>,
}

impl<'a> bpsec::BlockSet<'a> for EditorBlockSet<'a> {
    fn block(
        &'a self,
        block_number: u64,
    ) -> Option<(&'a block::Block, Option<block::Payload<'a>>)> {
        let (block, payload) = self.editor.block(block_number)?;
        Some((block, payload.map(block::Payload::Borrowed)))
    }
}
