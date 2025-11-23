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
    blocks: HashMap<u64, BlockTemplate<'a>>,
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
                    BlockTemplate::Insert(template) | BlockTemplate::Update(template)
                        if template.block.block_type == block_type =>
                    {
                        return Err(Error::IllegalDuplicate(block_type));
                    }
                    _ => {}
                }
            }
        }

        self.add_block(block_type)
    }

    fn add_block(self, block_type: block::Type) -> Result<BlockBuilder<'a>, Error> {
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

        if let Some((block_number, is_new, template)) =
            self.blocks
                .iter()
                .find_map(|(block_number, template)| match template {
                    BlockTemplate::Keep(t) if &block_type == t => {
                        let block = self.original.blocks.get(block_number)?;
                        Some((
                            *block_number,
                            false,
                            builder::BlockTemplate::new(*t, block.flags.clone(), block.crc_type),
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
    /// existing block.
    pub fn update_block(self, block_number: u64) -> Result<BlockBuilder<'a>, Error> {
        let (is_new, template) = match self
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
                (
                    false,
                    builder::BlockTemplate::new(*t, block.flags.clone(), block.crc_type),
                )
            }
            BlockTemplate::Insert(template) => (true, template.clone()),
            BlockTemplate::Update(template) => (false, template.clone()),
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
                block.copy_whole(self.source_data, array);
                Ok(block)
            }
            BlockTemplate::Update(template) => {
                let data = if template.data.is_some() {
                    None
                } else {
                    self.original
                        .blocks
                        .get(&block_number)
                        .and_then(|b| b.payload(self.source_data))
                };
                template
                    .build(block_number, data, array)
                    .map_err(Into::into)
            }
            BlockTemplate::Insert(template) => template
                .build(block_number, None, array)
                .map_err(Into::into),
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
    fn block(&self, block_number: u64) -> Option<&block::Block> {
        match self.editor.blocks.get(&block_number)? {
            BlockTemplate::Keep(_) => self.editor.original.blocks.get(&block_number),
            BlockTemplate::Update(template) | BlockTemplate::Insert(template) => {
                Some(&template.block)
            }
        }
    }

    fn block_payload(
        &'a self,
        block_number: u64,
        block: &block::Block,
    ) -> Option<block::Payload<'a>> {
        match self.editor.blocks.get(&block_number)? {
            BlockTemplate::Keep(_) => block
                .payload(self.editor.source_data)
                .map(block::Payload::Borrowed),
            BlockTemplate::Update(template) | BlockTemplate::Insert(template) => {
                match template
                    .data
                    .as_ref()
                    .map(|data| block::Payload::Borrowed(data.as_ref()))
                {
                    Some(data) => Some(data),
                    None => self
                        .editor
                        .original
                        .blocks
                        .get(&block_number)
                        .and_then(|b| {
                            b.payload(self.editor.source_data)
                                .map(block::Payload::Borrowed)
                        }),
                }
            }
        }
    }
}
