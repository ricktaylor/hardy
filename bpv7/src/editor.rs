use super::*;
use alloc::borrow::Cow;
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

/// The `Editor` provides an interface for modifying a bundle.
///
/// The editor is designed to allow for efficient modification of a bundle by
/// reusing the unmodified portions of the original bundle.
pub struct Editor<'a> {
    original: &'a bundle::Bundle,
    source_data: &'a [u8],
    bundle: Option<BundleUpdate>,
    blocks: HashMap<u64, BlockTemplate<'a>>,
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
        if let block::Type::Primary
        | block::Type::Payload
        | block::Type::BundleAge
        | block::Type::HopCount
        | block::Type::PreviousNode = block_type
        {
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

        self.add_block(block_type)
    }

    #[allow(clippy::result_large_err)]
    fn add_block(self, block_type: block::Type) -> Result<BlockBuilder<'a>, (Self, Error)> {
        // Find the lowest unused block_number
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
        if let block::Type::Primary = block_type {
            return Err((self, Error::PrimaryBlock));
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

        self.add_block(block_type)
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
        // Get security references before any modifications
        let (bib, bcb) = match self.original.blocks.get(&block_number) {
            Some(block) => (block.bib.clone(), block.bcb),
            None => (block::BibCoverage::None, None),
        };

        // Handle BIB coverage - must remove from target list if present
        match &bib {
            block::BibCoverage::Maybe => {
                return Err((self, bpsec::Error::MaybeHasBib(block_number).into()));
            }
            block::BibCoverage::Some(bib_num) => {
                // Check if the BIB is encrypted
                if let Some(bib_block) = self.original.blocks.get(bib_num)
                    && bib_block.bcb.is_some()
                {
                    return Err((self, Error::BibIsEncrypted(block_number)));
                }
                // Remove from BIB target list (signature will be invalid after update)
                self = self.remove_from_bib_targets(block_number, *bib_num)?;
            }
            block::BibCoverage::None => {}
        }

        // Remove from BCB target list if present (encryption will be invalid after update)
        if let Some(bcb_num) = bcb {
            self = self.remove_from_bcb_targets(block_number, bcb_num)?;
        }

        self.update_block_inner(block_number)
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
    fn remove_block_inner(mut self, block_number: u64) -> Result<Self, (Self, Error)> {
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
            BlockTemplate::Update(template) | BlockTemplate::Insert(template) => Some((
                &template.block,
                template.data.as_ref().map(|data| data.as_ref()),
            )),
        }
    }

    /// Remove the integrity check from a block in the bundle.
    ///
    /// Note that this will rewrite (or remove) the BIB block.
    ///
    /// On error, returns the editor along with the error so it can be reused for recovery.
    #[allow(clippy::result_large_err)]
    pub fn remove_integrity(mut self, block_number: u64) -> Result<Self, (Self, Error)> {
        let Some((target_block, _)) = self.block(block_number) else {
            return Err((self, Error::NoSuchBlock(block_number)));
        };

        let block::BibCoverage::Some(bib) = target_block.bib else {
            return Err((self, bpsec::Error::NotSigned.into()));
        };

        let target_block = target_block.clone();

        // Use the helper function to remove from BIB targets
        self = self.remove_from_bib_targets(block_number, bib)?;

        // Ensure we have a CRC if there's no BCB
        if target_block.bcb.is_none() && matches!(target_block.crc_type, crc::CrcType::None) {
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
                let target_payload: Box<[u8]> = std::mem::take(&mut target_payload);

                // Replace the block payload
                let mut block = block_set
                    .editor
                    .update_block_inner(block_number)?
                    .with_data(target_payload.into_vec().into());
                if matches!(original_block.bib, block::BibCoverage::None)
                    && matches!(original_block.crc_type, crc::CrcType::None)
                {
                    // Ensure we have a CRC
                    block = block.with_crc_type(crc::CrcType::CRC32_CASTAGNOLI);
                }
                self = block.rebuild();

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
    /// This will return the new `Bundle` and its serialized representation.
    pub fn rebuild(mut self) -> Result<Box<[u8]>, Error> {
        hardy_cbor::encode::try_emit_array(None, |a| {
            // Emit primary block
            let primary_block = self.blocks.remove(&0).expect("No primary block!");

            if let Some(mut update) = self.bundle.take() {
                // Sync the is_fragment flags to the presence of fragment info
                update.bundle_flags.is_fragment = update.fragment_info.is_some();

                let mut bundle = bundle::Bundle {
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
                // Emit primary block
                bundle.emit_primary_block(a)?;
            } else {
                self.build_block(0, primary_block, a)?;
            }

            // Stash payload block
            let payload_block = self.blocks.remove(&1).expect("No payload block!");

            // Emit extension blocks
            for (block_number, block_template) in core::mem::take(&mut self.blocks) {
                self.build_block(block_number, block_template, a)?;
            }

            // Emit payload block
            self.build_block(1, payload_block, a)?;

            Ok::<_, Error>(())
        })
        .map(Into::into)
    }

    fn build_block(
        &self,
        block_number: u64,
        template: BlockTemplate,
        array: &mut hardy_cbor::encode::Array,
    ) -> Result<block::Block, Error> {
        if let BlockTemplate::Update(template) | BlockTemplate::Insert(template) = template {
            template.build(block_number, array).map_err(Into::into)
        } else {
            let mut block = self
                .original
                .blocks
                .get(&block_number)
                .ok_or(Error::from(error::Error::Altered))?
                .clone();
            block.copy_whole(self.source_data, array);
            Ok(block)
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
