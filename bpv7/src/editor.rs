use super::*;
use alloc::borrow::Cow;
use core::ops::Range;
use smallvec::SmallVec;
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

    /// Modify the source buffer in place to produce the rebuilt bundle.
    ///
    /// This avoids allocation when the assembled chunks fit within the
    /// original buffer. Unchanged ranges that are already at the correct
    /// position are left untouched; New chunks overwrite the gaps.
    /// The buffer is resized (truncated or extended) if the total output
    /// length differs from the source.
    pub fn flatten_inplace(chunks: Vec<Self>, source: &mut Vec<u8>) {
        // Single pass: compute total length and check if backward copy is needed
        let mut content_len: usize = 0;
        let mut write_pos: usize = 1; // after 0x9F
        let mut needs_backward = false;
        for chunk in &chunks {
            let len = chunk.len();
            if let Chunk::Unchanged(range) = chunk {
                debug_assert!(range.end <= source.len());
                if !needs_backward && write_pos > range.start {
                    needs_backward = true;
                }
            }
            content_len += len;
            write_pos += len;
        }
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
        new_chunks.sort_by_key(|(_, b)| std::cmp::Reverse(b.len()));

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
                    block.extent = offset..offset + len;
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
    bundle: Option<BundleUpdate>,
    blocks: HashMap<u64, BlockTemplate<'a>>,
    bib_overrides: HashMap<u64, block::BibCoverage>,
    bcb_overrides: HashMap<u64, Option<u64>>,
}

struct BundleUpdate {
    bundle_flags: bundle::Flags,
    crc_type: crc::CrcType,
    timestamp: creation_timestamp::CreationTimestamp,
    source: eid::Eid,
    destination: eid::Eid,
    report_to: eid::Eid,
    lifetime: core::time::Duration,
    fragment_info: Option<bundle::FragmentInfo>,
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
            bundle: None,
            bib_overrides: HashMap::new(),
            bcb_overrides: HashMap::new(),
        }
    }

    fn primary_block(&mut self) -> Result<&mut BundleUpdate, Error> {
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

        if self.bundle.is_none() {
            self.bundle = Some(BundleUpdate {
                bundle_flags: self.original.flags.clone(),
                crc_type: self.original.crc_type,
                timestamp: self.original.id.timestamp.clone(),
                source: self.original.id.source.clone(),
                destination: self.original.destination.clone(),
                report_to: self.original.report_to.clone(),
                lifetime: self.original.lifetime,
                fragment_info: self.original.id.fragment_info.clone(),
            });
        }
        Ok(self.bundle.as_mut().unwrap())
    }

    /// Sets the bundle flags for this [`Editor`].
    ///
    /// On error, returns the editor along with the error so it can be reused for recovery.
    #[allow(clippy::result_large_err)]
    pub fn with_bundle_flags(mut self, flags: bundle::Flags) -> Result<Self, (Self, Error)> {
        match self.primary_block() {
            Ok(pb) => {
                pb.bundle_flags = flags;
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
                pb.timestamp = timestamp;
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
                pb.source = source;
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
                pb.fragment_info = fragment_info;
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
    pub(crate) fn set_bib_target(&mut self, target_block: u64, bib_block: u64) {
        self.bib_overrides
            .insert(target_block, block::BibCoverage::Some(bib_block));
    }

    /// Record that a BCB covers the given target block.
    ///
    /// Used by `Encryptor` to set `bcb` metadata on target blocks so that
    /// `rebuild_bundle()` returns a correct `Bundle` without reparsing.
    pub(crate) fn set_bcb_target(&mut self, target_block: u64, bcb_block: u64) {
        self.bcb_overrides.insert(target_block, Some(bcb_block));
    }

    /// Set a pre-encoded canonical primary block for re-emission.
    ///
    /// Used by the parser when the primary block needs re-encoding for
    /// canonicalization. The data is the complete encoded primary block,
    /// emitted as raw bytes during rebuild.
    pub(crate) fn set_canonical_primary(&mut self, data: Cow<'a, [u8]>) {
        self.blocks.insert(
            0,
            BlockTemplate::Update(builder::BlockTemplate::new(
                block::Type::Primary,
                block::Flags::default(),
                crc::CrcType::None,
                Some(data),
            )),
        );
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

                (
                    false,
                    builder::BlockTemplate::new(
                        *t,
                        block.flags.clone(),
                        block.crc_type,
                        if block.bcb.is_some() {
                            // Block is encrypted, caller MUST provide fresh data
                            None
                        } else {
                            block.payload(self.source_data).map(Cow::Borrowed)
                        },
                    ),
                )
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

        // Check BIB coverage
        if let Some((block, _)) = self.block(block_number) {
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
    #[allow(clippy::result_large_err)]
    fn remove_from_bib_targets(
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

    // Helper to get the inner Block
    fn block(&'a self, block_number: u64) -> Option<(&'a block::Block, Option<&'a [u8]>)> {
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

    /// Remove the integrity check from a block in the bundle.
    ///
    /// Note that this will rewrite (or remove) the BIB block.
    ///
    /// On error, returns the editor along with the error so it can be reused for recovery.
    #[allow(clippy::result_large_err)]
    pub fn remove_integrity(self, block_number: u64) -> Result<Self, (Self, Error)> {
        let Some((target_block, _)) = self.block(block_number) else {
            return Err((self, Error::NoSuchBlock(block_number)));
        };

        let block::BibCoverage::Some(bib) = target_block.bib else {
            return Err((self, bpsec::Error::NotSigned.into()));
        };

        self.remove_integrity_inner(block_number, bib)
    }

    /// Remove integrity from a block when the BIB block number is already known.
    ///
    /// This removes the target from the BIB and restores the CRC if needed.
    #[allow(clippy::result_large_err)]
    fn remove_integrity_inner(
        mut self,
        block_number: u64,
        bib_block_num: u64,
    ) -> Result<Self, (Self, Error)> {
        // Get block info for CRC restoration decision
        let (has_bcb, needs_crc) = self
            .block(block_number)
            .map(|(b, _)| (b.bcb.is_some(), matches!(b.crc_type, crc::CrcType::None)))
            .unwrap_or((false, false));

        // Remove the target from the BIB
        self = self.remove_from_bib_targets(block_number, bib_block_num)?;

        // Ensure we have a CRC if there's no BCB
        if !has_bcb && needs_crc {
            if block_number == 0 {
                // Primary block: use with_bundle_crc_type
                self = self.with_bundle_crc_type(crc::CrcType::CRC32_CASTAGNOLI)?;
            } else {
                // Extension block: use update_block_inner
                self = self
                    .update_block_inner(block_number)?
                    .with_crc_type(crc::CrcType::CRC32_CASTAGNOLI)
                    .rebuild();
            }
        }

        Ok(self)
    }

    /// Remove the encryption from a block in the bundle.
    ///
    /// Note that this will rewrite (or remove) the target and the BCB block.
    ///
    /// Per RFC 9172 Section 3.8: "A BCB MUST NOT target a BIB unless it shares a
    /// security target with that BIB." Therefore, when decrypting a block, any
    /// encrypted BIB that targets that block must also be decrypted and the
    /// signature removed.
    ///
    /// On error, returns the editor along with the error so it can be reused for recovery.
    #[allow(clippy::result_large_err)]
    pub fn remove_encryption<K>(
        mut self,
        block_number: u64,
        key_source: &K,
    ) -> Result<Self, (Self, Error)>
    where
        K: bpsec::key::KeySource + ?Sized,
    {
        if block_number == 0 {
            return Err((self, Error::PrimaryBlock));
        }

        let Some((target_block, _)) = self.block(block_number) else {
            return Err((self, Error::NoSuchBlock(block_number)));
        };

        let Some(bcb) = target_block.bcb else {
            return Err((self, bpsec::Error::NotEncrypted.into()));
        };

        if let Some((_, Some(bcb_payload))) = self.block(bcb) {
            let original_block = target_block.clone();

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

            if let Some(op) = opset.operations.remove(&block_number) {
                // Decrypt the target payload
                let block_set = EditorBlockSet { editor: self };
                let mut target_payload = match op.decrypt(
                    key_source,
                    bpsec::bcb::OperationArgs {
                        bpsec_source: &opset.source,
                        target: block_number,
                        source: bcb,
                        blocks: &block_set,
                    },
                ) {
                    Ok(t) => t,
                    Err(e) => {
                        return Err((block_set.editor, e.into()));
                    }
                };

                // Steal the content of the decrypted payload
                // This is safe as this function is an explicit 'remove the encryption', hence
                // removing the Zeroizing<> is valid
                let target_payload: Box<[u8]> = core::mem::take(&mut target_payload);

                // Replace the block payload
                let mut block = block_set
                    .editor
                    .update_block_inner(block_number)?
                    .with_data(target_payload.into_vec().into());
                // Only add CRC if there's no BIB for integrity protection
                if !matches!(original_block.bib, block::BibCoverage::Some(_))
                    && matches!(original_block.crc_type, crc::CrcType::None)
                {
                    block = block.with_crc_type(crc::CrcType::CRC32_CASTAGNOLI);
                }
                self = block.rebuild();

                // RFC 9172 Section 3.8: "A BCB MUST NOT target a BIB unless it shares a
                // security target with that BIB."
                //
                // Now that we've decrypted block_number and removed it from the BCB targets,
                // any encrypted BIB that targets block_number would violate this rule.
                // We must decrypt such BIBs and remove the signature.

                // Handle BIBs within this same BCB's targets.
                // Note: BCB-AES-GCM (RFC 9173) cannot have multiple targets due to IV
                // uniqueness requirements, so this code path is for future security
                // contexts (e.g., COSE-based) that may support multi-target BCBs.
                if opset.can_share() {
                    let bib_targets: SmallVec<[u64; 4]> = opset
                        .operations
                        .keys()
                        .filter(|&&target| {
                            if let Some((blk, _)) = self.block(target) {
                                matches!(blk.block_type, block::Type::BlockIntegrity)
                            } else {
                                false
                            }
                        })
                        .copied()
                        .collect();

                    for bib_block_num in bib_targets {
                        let Some(bib_op) = opset.operations.get(&bib_block_num) else {
                            continue;
                        };

                        // Decrypt the BIB to inspect its targets
                        let block_set = EditorBlockSet { editor: self };
                        let mut decrypted_bib = match bib_op.decrypt(
                            key_source,
                            bpsec::bcb::OperationArgs {
                                bpsec_source: &opset.source,
                                target: bib_block_num,
                                source: bcb,
                                blocks: &block_set,
                            },
                        ) {
                            Ok(t) => t,
                            Err(_) => {
                                // We can't decrypt the BIB - this would leave the bundle in an
                                // invalid state per RFC 9172 Section 3.8
                                return Err((
                                    block_set.editor,
                                    Error::CannotDecryptRelatedBib(bib_block_num),
                                ));
                            }
                        };
                        self = block_set.editor;

                        // Parse the decrypted BIB to check its targets
                        let bib_opset = match hardy_cbor::decode::parse::<bpsec::bib::OperationSet>(
                            &decrypted_bib,
                        ) {
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

                        // Check if the BIB targets the block we just decrypted
                        if bib_opset.operations.contains_key(&block_number) {
                            // The BIB targets our decrypted block - decrypt the BIB and remove signature
                            let decrypted_bib: Box<[u8]> = core::mem::take(&mut decrypted_bib);
                            self = self
                                .update_block_inner(bib_block_num)?
                                .with_data(decrypted_bib.into_vec().into())
                                .rebuild();

                            // Remove the BIB from the BCB's target list
                            opset.operations.remove(&bib_block_num);

                            // Now remove the signature from the decrypted block (and restore CRC)
                            self = self.remove_integrity_inner(block_number, bib_block_num)?;
                        }
                        // If BIB doesn't target our block, leave it encrypted
                    }
                }

                // Update/remove the current BCB
                if opset.operations.is_empty() {
                    self = self.remove_block_inner(bcb)?;
                } else {
                    // Rewrite BCB
                    self = self
                        .update_block_inner(bcb)?
                        .with_data(hardy_cbor::encode::emit(&opset).0.into())
                        .rebuild();
                }
            }
        }

        Ok(self)
    }

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
        let mut bundle_out = bundle::Bundle::default();

        let primary_block = self.blocks.remove(&0).expect("No primary block!");

        // Build primary chunk
        let primary_chunk = if let Some(mut update) = self.bundle.take() {
            update.bundle_flags.is_fragment = update.fragment_info.is_some();

            bundle_out.id.source = update.source;
            bundle_out.id.timestamp = update.timestamp;
            bundle_out.id.fragment_info = update.fragment_info;
            bundle_out.flags = update.bundle_flags;
            bundle_out.crc_type = update.crc_type;
            bundle_out.destination = update.destination;
            bundle_out.report_to = update.report_to;
            bundle_out.lifetime = update.lifetime;

            let primary_bytes = bundle::primary_block::PrimaryBlock::emit(&bundle_out)?;
            let len = primary_bytes.len();
            bundle_out.blocks.insert(
                0,
                bundle::primary_block::PrimaryBlock::as_block(bundle_out.crc_type, 0..len),
            );
            (0u64, Chunk::New(primary_bytes.into()))
        } else {
            bundle_out.id = self.original.id.clone();
            bundle_out.flags = self.original.flags.clone();
            bundle_out.crc_type = self.original.crc_type;
            bundle_out.destination = self.original.destination.clone();
            bundle_out.report_to = self.original.report_to.clone();
            bundle_out.lifetime = self.original.lifetime;

            if let BlockTemplate::Update(template) = primary_block {
                let primary_bytes = template
                    .data
                    .ok_or(Error::Builder(builder::Error::NoBlockData))?;
                let len = primary_bytes.len();
                bundle_out.blocks.insert(
                    0,
                    bundle::primary_block::PrimaryBlock::as_block(bundle_out.crc_type, 0..len),
                );
                (0u64, Chunk::New(primary_bytes.into_owned().into()))
            } else {
                let block = self
                    .original
                    .blocks
                    .get(&0)
                    .ok_or(Error::from(error::Error::Altered))?;
                bundle_out.blocks.insert(0, block.clone());
                (0u64, Chunk::Unchanged(block.extent.clone()))
            }
        };

        let payload_block = self.blocks.remove(&1).expect("No payload block!");

        // Build extension chunks
        let mut ext_chunks = Vec::new();
        for (block_number, block_template) in core::mem::take(&mut self.blocks) {
            match &block_template {
                BlockTemplate::Keep(block::Type::PreviousNode) => {
                    bundle_out.previous_node = self.original.previous_node.clone();
                }
                BlockTemplate::Keep(block::Type::BundleAge) => {
                    bundle_out.age = self.original.age;
                }
                BlockTemplate::Keep(block::Type::HopCount) => {
                    bundle_out.hop_count = self.original.hop_count.clone();
                }
                BlockTemplate::Update(template) | BlockTemplate::Insert(template) => {
                    if let Some(ref payload) = template.data {
                        match template.block.block_type {
                            block::Type::PreviousNode => {
                                if let Ok(eid) = hardy_cbor::decode::parse::<eid::Eid>(payload) {
                                    bundle_out.previous_node = Some(eid);
                                }
                            }
                            block::Type::BundleAge => {
                                if let Ok(millis) = hardy_cbor::decode::parse::<u64>(payload) {
                                    bundle_out.age =
                                        Some(core::time::Duration::from_millis(millis));
                                }
                            }
                            block::Type::HopCount => {
                                if let Ok(hop) =
                                    hardy_cbor::decode::parse::<hop_info::HopInfo>(payload)
                                {
                                    bundle_out.hop_count = Some(hop);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            let (block, chunk) = self.build_chunk(block_number, block_template)?;
            bundle_out.blocks.insert(block_number, block);
            ext_chunks.push((block_number, chunk));
        }

        // Build payload chunk
        let (block, payload_chunk) = self.build_chunk(1, payload_block)?;
        bundle_out.blocks.insert(1, block);

        // Apply security metadata overrides from Signer/Encryptor
        for (block_number, bib) in &self.bib_overrides {
            if let Some(block) = bundle_out.blocks.get_mut(block_number) {
                block.bib = bib.clone();
            }
        }
        for (block_number, bcb) in &self.bcb_overrides {
            if let Some(block) = bundle_out.blocks.get_mut(block_number) {
                block.bcb = *bcb;
            }
        }

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
        let primary_chunk = if let Some(mut update) = self.bundle.take() {
            update.bundle_flags.is_fragment = update.fragment_info.is_some();

            let bundle = bundle::Bundle {
                id: bundle::Id {
                    source: update.source,
                    timestamp: update.timestamp,
                    fragment_info: update.fragment_info,
                },
                flags: update.bundle_flags,
                crc_type: update.crc_type,
                destination: update.destination,
                report_to: update.report_to,
                lifetime: update.lifetime,
                ..Default::default()
            };
            (
                0u64,
                Chunk::New(bundle::primary_block::PrimaryBlock::emit(&bundle)?.into()),
            )
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
            (0u64, Chunk::Unchanged(block.extent.clone()))
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
            let extent = block.extent.clone();
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
                editor.original.crc_type,
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

#[cfg(test)]
mod test {
    use super::*;

    // Build a bundle, parse it, return (bundle, data) ready for editing.
    fn make_bundle() -> (bundle::Bundle, Box<[u8]>) {
        builder::Builder::new("ipn:1.0".parse().unwrap(), "ipn:2.0".parse().unwrap())
            .with_report_to("ipn:3.0".parse().unwrap())
            .with_payload("Hello".as_bytes().into())
            .build(creation_timestamp::CreationTimestamp::now())
            .unwrap()
    }

    // Build a bundle with a hop count block, then re-parse so block keys match wire numbers.
    fn make_bundle_with_hop_count() -> (bundle::Bundle, Box<[u8]>) {
        let (_, data) =
            builder::Builder::new("ipn:1.0".parse().unwrap(), "ipn:2.0".parse().unwrap())
                .with_hop_count(&hop_info::HopInfo {
                    limit: 30,
                    count: 0,
                })
                .with_payload("Hello".as_bytes().into())
                .build(creation_timestamp::CreationTimestamp::now())
                .unwrap();
        let bundle = reparse(&data);
        (bundle, data)
    }

    // Unwrap a Result<T, (Editor, Error)> — panics with the error on failure.
    fn ok<T>(result: Result<T, (Editor, Error)>) -> T {
        result.unwrap_or_else(|(_, e)| panic!("Editor operation failed: {e}"))
    }

    // Edit a bundle, rebuild, re-parse, and return the parsed bundle.
    fn reparse(data: &[u8]) -> bundle::Bundle {
        let keys = bpsec::key::KeySet::new(vec![]);
        match bundle::RewrittenBundle::parse_with_keys(data, &keys).unwrap() {
            bundle::RewrittenBundle::Valid { bundle, .. }
            | bundle::RewrittenBundle::Rewritten { bundle, .. } => bundle,
            bundle::RewrittenBundle::Invalid { error, .. } => panic!("Re-parse failed: {error}"),
        }
    }

    #[test]
    fn no_op_rebuild() {
        let (bundle, data) = make_bundle();
        let new_data = Editor::new(&bundle, &data)
            .rebuild()
            .map(|c| Chunk::flatten(c, &data))
            .unwrap();
        let reparsed = reparse(&new_data);
        assert_eq!(reparsed.id.source, bundle.id.source);
        assert_eq!(reparsed.destination, bundle.destination);
    }

    #[test]
    fn change_destination() {
        let (bundle, data) = make_bundle();
        let new_dest: eid::Eid = "ipn:99.0".parse().unwrap();
        let new_data = ok(Editor::new(&bundle, &data).with_destination(new_dest.clone()))
            .rebuild()
            .map(|c| Chunk::flatten(c, &data))
            .unwrap();
        let reparsed = reparse(&new_data);
        assert_eq!(reparsed.destination, new_dest);
        assert_eq!(reparsed.id.source, bundle.id.source);
    }

    #[test]
    fn change_source() {
        let (bundle, data) = make_bundle();
        let new_src: eid::Eid = "ipn:50.0".parse().unwrap();
        let new_data = ok(Editor::new(&bundle, &data).with_source(new_src.clone()))
            .rebuild()
            .map(|c| Chunk::flatten(c, &data))
            .unwrap();
        let reparsed = reparse(&new_data);
        assert_eq!(reparsed.id.source, new_src);
    }

    #[test]
    fn change_report_to() {
        let (bundle, data) = make_bundle();
        let new_rt: eid::Eid = "ipn:77.0".parse().unwrap();
        let new_data = ok(Editor::new(&bundle, &data).with_report_to(new_rt.clone()))
            .rebuild()
            .map(|c| Chunk::flatten(c, &data))
            .unwrap();
        let reparsed = reparse(&new_data);
        assert_eq!(reparsed.report_to, new_rt);
    }

    #[test]
    fn change_lifetime() {
        let (bundle, data) = make_bundle();
        let new_lifetime = core::time::Duration::from_secs(7200);
        let new_data = ok(Editor::new(&bundle, &data).with_lifetime(new_lifetime))
            .rebuild()
            .map(|c| Chunk::flatten(c, &data))
            .unwrap();
        let reparsed = reparse(&new_data);
        assert_eq!(reparsed.lifetime, new_lifetime);
    }

    #[test]
    fn change_crc_type() {
        let (bundle, data) = make_bundle();
        let new_data =
            ok(Editor::new(&bundle, &data).with_bundle_crc_type(crc::CrcType::CRC16_X25))
                .rebuild()
                .map(|c| Chunk::flatten(c, &data))
                .unwrap();
        let reparsed = reparse(&new_data);
        assert!(matches!(reparsed.crc_type, crc::CrcType::CRC16_X25));
    }

    #[test]
    fn add_extension_block() {
        let (bundle, data) = make_bundle();
        let new_data = ok(Editor::new(&bundle, &data).push_block(block::Type::Unrecognised(200)))
            .with_data((&[0xCA, 0xFE][..]).into())
            .rebuild()
            .rebuild()
            .map(|c| Chunk::flatten(c, &data))
            .unwrap();
        let reparsed = reparse(&new_data);
        assert!(reparsed.blocks.contains_key(&2));
    }

    #[test]
    fn remove_extension_block() {
        let (bundle, data) = make_bundle_with_hop_count();
        let hop_block = bundle
            .blocks
            .iter()
            .find(|(_, b)| matches!(b.block_type, block::Type::HopCount))
            .map(|(n, _)| *n)
            .expect("Should have hop count block");

        let new_data = ok(Editor::new(&bundle, &data).remove_block(hop_block))
            .rebuild()
            .map(|c| Chunk::flatten(c, &data))
            .unwrap();
        let reparsed = reparse(&new_data);
        assert!(reparsed.hop_count.is_none());
    }

    #[test]
    fn cannot_remove_payload() {
        let (bundle, data) = make_bundle();
        let result = Editor::new(&bundle, &data).remove_block(1);
        assert!(matches!(result, Err((_, Error::PayloadBlock))));
    }

    #[test]
    fn cannot_remove_primary() {
        let (bundle, data) = make_bundle();
        let result = Editor::new(&bundle, &data).remove_block(0);
        assert!(matches!(result, Err((_, Error::PrimaryBlock))));
    }

    #[test]
    fn cannot_add_duplicate_hop_count() {
        let (bundle, data) = make_bundle_with_hop_count();
        let result = Editor::new(&bundle, &data).push_block(block::Type::HopCount);
        assert!(matches!(result, Err((_, Error::IllegalDuplicate(_)))));
    }

    #[test]
    fn multiple_primary_changes() {
        let (bundle, data) = make_bundle();
        let new_dest: eid::Eid = "ipn:99.0".parse().unwrap();
        let new_lifetime = core::time::Duration::from_secs(600);
        let editor = ok(Editor::new(&bundle, &data).with_destination(new_dest.clone()));
        let new_data = ok(editor.with_lifetime(new_lifetime))
            .rebuild()
            .map(|c| Chunk::flatten(c, &data))
            .unwrap();
        let reparsed = reparse(&new_data);
        assert_eq!(reparsed.destination, new_dest);
        assert_eq!(reparsed.lifetime, new_lifetime);
        assert_eq!(reparsed.id.source, bundle.id.source);
    }

    #[test]
    fn insert_new_block_type() {
        let (bundle, data) = make_bundle();
        // insert_block with a new type should add it
        let new_data = ok(Editor::new(&bundle, &data).insert_block(block::Type::Unrecognised(200)))
            .with_data((&[0x01, 0x02][..]).into())
            .rebuild()
            .rebuild()
            .map(|c| Chunk::flatten(c, &data))
            .unwrap();
        let reparsed = reparse(&new_data);
        assert!(reparsed.blocks.contains_key(&2));
    }

    /// Asserts that the Bundle returned by `rebuild_bundle()` matches a fresh
    /// parse of the same data — same block set, same extents, same primary
    /// block fields.
    fn assert_rebuild_matches_parse(bundle: &bundle::Bundle, data: &[u8]) {
        let reparsed = reparse(data);

        // Primary block fields
        assert_eq!(bundle.id.source, reparsed.id.source);
        assert_eq!(bundle.id.timestamp, reparsed.id.timestamp);
        assert_eq!(bundle.id.fragment_info, reparsed.id.fragment_info);
        assert_eq!(bundle.destination, reparsed.destination);
        assert_eq!(bundle.report_to, reparsed.report_to);
        assert_eq!(bundle.lifetime, reparsed.lifetime);
        assert!(
            matches!(
                (&bundle.crc_type, &reparsed.crc_type),
                (crc::CrcType::None, crc::CrcType::None)
                    | (crc::CrcType::CRC16_X25, crc::CrcType::CRC16_X25)
                    | (
                        crc::CrcType::CRC32_CASTAGNOLI,
                        crc::CrcType::CRC32_CASTAGNOLI
                    )
            ),
            "CRC type mismatch"
        );
        assert_eq!(bundle.flags, reparsed.flags);

        // Same set of block numbers
        assert_eq!(
            bundle.blocks.keys().collect::<HashSet<_>>(),
            reparsed.blocks.keys().collect::<HashSet<_>>(),
            "Block sets differ"
        );

        // Block fields match and ranges index validly into the data
        for (block_number, block) in &bundle.blocks {
            let reparsed_block = reparsed.blocks.get(block_number).unwrap();
            assert_eq!(
                block.block_type, reparsed_block.block_type,
                "Block {block_number} type mismatch"
            );
            assert_eq!(
                block.flags, reparsed_block.flags,
                "Block {block_number} flags mismatch"
            );
            assert!(
                matches!(
                    (&block.crc_type, &reparsed_block.crc_type),
                    (crc::CrcType::None, crc::CrcType::None)
                        | (crc::CrcType::CRC16_X25, crc::CrcType::CRC16_X25)
                        | (
                            crc::CrcType::CRC32_CASTAGNOLI,
                            crc::CrcType::CRC32_CASTAGNOLI
                        )
                ),
                "Block {block_number} CRC type mismatch"
            );
            assert_eq!(
                block.bib, reparsed_block.bib,
                "Block {block_number} BIB coverage mismatch"
            );
            assert_eq!(
                block.bcb, reparsed_block.bcb,
                "Block {block_number} BCB mismatch"
            );
            assert_eq!(
                block.extent, reparsed_block.extent,
                "Block {block_number} extent mismatch"
            );
            assert_eq!(
                block.data, reparsed_block.data,
                "Block {block_number} data range mismatch"
            );
            assert!(
                block.extent.end <= data.len(),
                "Block {block_number} extent exceeds data length"
            );
            assert!(
                block.data.end <= data.len(),
                "Block {block_number} data range exceeds data length"
            );
        }
    }

    #[test]
    fn rebuild_bundle_no_op() {
        let (bundle, data) = make_bundle();
        let (new_bundle, new_data) = Editor::new(&bundle, &data)
            .rebuild_bundle()
            .map(|(b, c)| (b, Chunk::flatten(c, &data)))
            .unwrap();
        assert_rebuild_matches_parse(&new_bundle, &new_data);
    }

    #[test]
    fn rebuild_bundle_change_destination() {
        let (bundle, data) = make_bundle();
        let new_dest: eid::Eid = "ipn:99.0".parse().unwrap();
        let (new_bundle, new_data) =
            ok(Editor::new(&bundle, &data).with_destination(new_dest.clone()))
                .rebuild_bundle()
                .map(|(b, c)| (b, Chunk::flatten(c, &data)))
                .unwrap();
        assert_eq!(new_bundle.destination, new_dest);
        assert_eq!(new_bundle.id.source, bundle.id.source);
        assert_rebuild_matches_parse(&new_bundle, &new_data);
    }

    #[test]
    fn rebuild_bundle_multiple_primary_changes() {
        let (bundle, data) = make_bundle();
        let new_dest: eid::Eid = "ipn:99.0".parse().unwrap();
        let new_lifetime = core::time::Duration::from_secs(600);
        let editor = ok(Editor::new(&bundle, &data).with_destination(new_dest.clone()));
        let (new_bundle, new_data) = ok(editor.with_lifetime(new_lifetime))
            .rebuild_bundle()
            .map(|(b, c)| (b, Chunk::flatten(c, &data)))
            .unwrap();
        assert_eq!(new_bundle.destination, new_dest);
        assert_eq!(new_bundle.lifetime, new_lifetime);
        assert_rebuild_matches_parse(&new_bundle, &new_data);
    }

    #[test]
    fn rebuild_bundle_add_block() {
        let (bundle, data) = make_bundle();
        let (new_bundle, new_data) =
            ok(Editor::new(&bundle, &data).push_block(block::Type::Unrecognised(200)))
                .with_data((&[0xCA, 0xFE][..]).into())
                .rebuild()
                .rebuild_bundle()
                .map(|(b, c)| (b, Chunk::flatten(c, &data)))
                .unwrap();
        assert!(new_bundle.blocks.contains_key(&2));
        assert_rebuild_matches_parse(&new_bundle, &new_data);
    }

    #[test]
    fn rebuild_bundle_remove_block() {
        let (bundle, data) = make_bundle_with_hop_count();
        let hop_block = bundle
            .blocks
            .iter()
            .find(|(_, b)| matches!(b.block_type, block::Type::HopCount))
            .map(|(n, _)| *n)
            .expect("Should have hop count block");

        let (new_bundle, new_data) = ok(Editor::new(&bundle, &data).remove_block(hop_block))
            .rebuild_bundle()
            .map(|(b, c)| (b, Chunk::flatten(c, &data)))
            .unwrap();
        assert!(!new_bundle.blocks.contains_key(&hop_block));
        assert_rebuild_matches_parse(&new_bundle, &new_data);
    }

    #[test]
    fn flatten_inplace_no_op() {
        let (bundle, data) = make_bundle();
        let flattened = Editor::new(&bundle, &data)
            .rebuild()
            .map(|c| Chunk::flatten(c, &data))
            .unwrap();
        let chunks = Editor::new(&bundle, &data).rebuild().unwrap();
        let mut inplace = data.to_vec();
        Chunk::flatten_inplace(chunks, &mut inplace);
        assert_eq!(&*flattened, &*inplace);
    }

    #[test]
    fn flatten_inplace_change_destination() {
        let (bundle, data) = make_bundle();
        let new_dest: eid::Eid = "ipn:99.0".parse().unwrap();

        let flattened = ok(Editor::new(&bundle, &data).with_destination(new_dest.clone()))
            .rebuild()
            .map(|c| Chunk::flatten(c, &data))
            .unwrap();

        let chunks = ok(Editor::new(&bundle, &data).with_destination(new_dest))
            .rebuild()
            .unwrap();
        let mut inplace = data.to_vec();
        Chunk::flatten_inplace(chunks, &mut inplace);
        assert_eq!(&*flattened, &*inplace);
    }

    #[test]
    fn flatten_inplace_add_block() {
        let (bundle, data) = make_bundle();

        let flattened = ok(Editor::new(&bundle, &data).push_block(block::Type::Unrecognised(200)))
            .with_data((&[0xCA, 0xFE][..]).into())
            .rebuild()
            .rebuild()
            .map(|c| Chunk::flatten(c, &data))
            .unwrap();

        let chunks = ok(Editor::new(&bundle, &data).push_block(block::Type::Unrecognised(200)))
            .with_data((&[0xCA, 0xFE][..]).into())
            .rebuild()
            .rebuild()
            .unwrap();
        let mut inplace = data.to_vec();
        Chunk::flatten_inplace(chunks, &mut inplace);
        assert_eq!(&*flattened, &*inplace);
    }

    #[test]
    fn flatten_inplace_remove_block() {
        let (bundle, data) = make_bundle_with_hop_count();
        let hop_block = bundle
            .blocks
            .iter()
            .find(|(_, b)| matches!(b.block_type, block::Type::HopCount))
            .map(|(n, _)| *n)
            .expect("Should have hop count block");

        let flattened = ok(Editor::new(&bundle, &data).remove_block(hop_block))
            .rebuild()
            .map(|c| Chunk::flatten(c, &data))
            .unwrap();

        let chunks = ok(Editor::new(&bundle, &data).remove_block(hop_block))
            .rebuild()
            .unwrap();
        let mut inplace = data.to_vec();
        Chunk::flatten_inplace(chunks, &mut inplace);
        assert_eq!(&*flattened, &*inplace);
    }
}
