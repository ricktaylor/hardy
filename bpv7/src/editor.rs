use super::*;
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

    #[error(transparent)]
    Builder(#[from] builder::Error),
}

/// The `Editor` provides an interface for modifying a bundle.
///
/// The editor is designed to allow for efficient modification of a bundle by
/// reusing the unmodified portions of the original bundle.
pub struct Editor<'a> {
    original: &'a bundle::Bundle,
    source_data: &'a [u8],
    blocks: HashMap<u64, BlockTemplate>,
}

enum BlockTemplate {
    Keep(block::Type),
    Replace(builder::BlockTemplate),
}

/// The `BlockBuilder` is used to construct a new or replacement block for a
/// bundle.
pub struct BlockBuilder<'a> {
    editor: Editor<'a>,
    block_number: u64,
    template: builder::BlockTemplate,
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
        }
    }

    /// Add a new block into the bundle.
    ///
    /// The new block will be assigned the next available block
    /// number.  Be very careful about add duplicate blocks that should not be duplicated
    pub fn push_block(self, block_type: block::Type) -> Result<BlockBuilder<'a>, Error> {
        if let block::Type::Primary
        | block::Type::Payload
        | block::Type::BundleAge
        | block::Type::HopCount
        | block::Type::PreviousNode = block_type
        {
            for template in self.blocks.values() {
                match template {
                    BlockTemplate::Keep(t) if t == &block_type => {
                        return Err(Error::IllegalDuplicate(block_type));
                    }
                    BlockTemplate::Replace(template) if template.block_type == block_type => {
                        return Err(Error::IllegalDuplicate(block_type));
                    }
                    _ => {}
                }
            }
        }

        // Find the lowest unused block_number
        let mut block_number = 2u64;
        loop {
            if !self.blocks.contains_key(&block_number) {
                return Ok(BlockBuilder::new(self, block_number, block_type));
            }
            block_number = block_number
                .checked_add(1)
                .ok_or(Error::OutOfBlockNumbers)?;
        }
    }

    /// Insert a new block into the bundle.
    ///
    /// If a block of the same type already exists, the new block will replace
    /// it. Otherwise, the new block will be assigned the next available block
    /// number.
    pub fn insert_block(self, block_type: block::Type) -> Result<BlockBuilder<'a>, Error> {
        if let block::Type::Primary = block_type {
            return Err(Error::PrimaryBlock);
        }

        if let Some((block_number, template)) =
            self.blocks
                .iter()
                .find_map(|(block_number, template)| match template {
                    BlockTemplate::Keep(t) if &block_type == t => {
                        let block = self.original.blocks.get(block_number)?;
                        Some((
                            *block_number,
                            builder::BlockTemplate::new(*t, block.flags.clone(), block.crc_type),
                        ))
                    }
                    BlockTemplate::Replace(template) if template.block_type == block_type => {
                        Some((*block_number, template.clone()))
                    }
                    _ => None,
                })
        {
            return Ok(BlockBuilder::new_from_template(
                self,
                block_number,
                template,
            ));
        }

        // Find the lowest unused block_number
        let mut block_number = 2u64;
        loop {
            if !self.blocks.contains_key(&block_number) {
                return Ok(BlockBuilder::new(self, block_number, block_type));
            }
            block_number = block_number
                .checked_add(1)
                .ok_or(Error::OutOfBlockNumbers)?;
        }
    }

    /// Update an existing block in the bundle.
    ///
    /// This will return a `BlockBuilder` that can be used to manipulate the
    /// existing block.
    pub fn update_block(self, block_number: u64) -> Result<BlockBuilder<'a>, Error> {
        let template = match self
            .blocks
            .get(&block_number)
            .ok_or(Error::NoSuchBlock(block_number))?
        {
            BlockTemplate::Keep(t) => {
                if let &block::Type::Primary = t {
                    return Err(Error::PrimaryBlock);
                }
                let block = self
                    .original
                    .blocks
                    .get(&block_number)
                    .ok_or(Error::NoSuchBlock(block_number))?;
                builder::BlockTemplate::new(*t, block.flags.clone(), block.crc_type)
            }
            BlockTemplate::Replace(template) => template.clone(),
        };

        Ok(BlockBuilder::new_from_template(
            self,
            block_number,
            template,
        ))
    }

    /// Remove a block from the bundle.
    ///
    /// Note that the primary and payload blocks cannot be removed.
    pub fn remove_block(mut self, block_number: u64) -> Result<Self, Error> {
        if block_number == 0 {
            return Err(Error::PrimaryBlock);
        }
        if block_number == 1 {
            return Err(Error::PayloadBlock);
        }
        self.blocks.remove(&block_number);
        Ok(self)
    }

    /// Create a `Signer` to sign blocks in the bundle.
    ///
    /// Note that this consumes the `Editor`, so any modifications made to the
    /// bundle prior to calling this method will be completed prior to signing.
    pub fn signer(self) -> bpsec::signer::Signer<'a> {
        bpsec::signer::Signer::new(self.original, self.source_data)
    }

    /// Create an `Encryptor` to encrypt blocks in the bundle.
    ///
    /// Note that this consumes the `Editor`, so any modifications made to the
    /// bundle prior to calling this method will be completed prior to signing.
    pub fn encryptor(self) -> bpsec::encryptor::Encryptor<'a> {
        bpsec::encryptor::Encryptor::new(self.original, self.source_data)
    }

    /// Rebuild the bundle, applying all of the modifications.
    ///
    /// This will return the new `Bundle` and its serialized representation.
    pub fn rebuild(mut self) -> Result<(bundle::Bundle, Box<[u8]>), Error> {
        let mut bundle = bundle::Bundle {
            id: self.original.id.clone(),
            flags: self.original.flags.clone(),
            crc_type: self.original.crc_type,
            destination: self.original.destination.clone(),
            report_to: self.original.report_to.clone(),
            lifetime: self.original.lifetime,
            previous_node: self.original.previous_node.clone(),
            age: self.original.age,
            hop_count: self.original.hop_count.clone(),
            blocks: HashMap::new(),
        };

        let data = hardy_cbor::encode::try_emit_array(None, |a| {
            // Emit primary block
            let primary_block = self.blocks.remove(&0).expect("No primary block!");
            bundle
                .blocks
                .insert(0, self.build_block(0, primary_block, a)?);

            // Stash payload block
            let payload_block = self.blocks.remove(&1).expect("No payload block!");

            // Emit extension blocks
            for (block_number, block_template) in core::mem::take(&mut self.blocks) {
                bundle.blocks.insert(
                    block_number,
                    self.build_block(block_number, block_template, a)?,
                );
            }

            // Emit payload block
            bundle
                .blocks
                .insert(1, self.build_block(1, payload_block, a)?);

            Ok::<_, Error>(())
        })?;

        Ok((bundle, data.into()))
    }

    fn build_block(
        &self,
        block_number: u64,
        template: BlockTemplate,
        array: &mut hardy_cbor::encode::Array,
    ) -> Result<block::Block, Error> {
        match template {
            BlockTemplate::Keep(_) => {
                let mut block = self
                    .original
                    .blocks
                    .get(&block_number)
                    .expect("Mismatched block in bundle!")
                    .clone();
                block.copy_payload(self.source_data, array);
                Ok(block)
            }
            BlockTemplate::Replace(template) => {
                template.build(block_number, array).map_err(Into::into)
            }
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
            ),
            block_number,
            editor,
        }
    }

    fn new_from_template(
        editor: Editor<'a>,
        block_number: u64,
        template: builder::BlockTemplate,
    ) -> Self {
        Self {
            template,
            block_number,
            editor,
        }
    }

    /// Set the `Flags` for this block.
    pub fn with_flags(mut self, flags: block::Flags) -> Self {
        self.template.flags = flags;
        self
    }

    /// Set the `CrcType` for this block.
    pub fn with_crc_type(mut self, crc_type: crc::CrcType) -> Self {
        self.template.crc_type = crc_type;
        self
    }

    /// Get the block number for this block.
    pub fn block_number(&self) -> u64 {
        self.block_number
    }

    /// Build the block and return the modified `Editor`.
    pub fn build<T: AsRef<[u8]>>(mut self, data: T) -> Editor<'a> {
        if !data.as_ref().is_empty() {
            self.template.data = Some(data.as_ref().into());
        }

        self.editor
            .blocks
            .insert(self.block_number, BlockTemplate::Replace(self.template));

        self.editor
    }
}
