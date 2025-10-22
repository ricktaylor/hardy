use super::*;

pub struct Editor<'a> {
    original: &'a bundle::Bundle,
    source_data: &'a [u8],
    blocks: HashMap<u64, BlockTemplate>,
}

enum BlockTemplate {
    Keep(block::Type),
    Add(builder::BlockTemplate),
}

pub struct BlockBuilder<'a> {
    editor: Editor<'a>,
    block_number: u64,
    template: builder::BlockTemplate,
}

impl<'a> Editor<'a> {
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

    pub fn add_block(self, block_type: block::Type) -> BlockBuilder<'a> {
        if let block::Type::Primary | block::Type::Payload = block_type {
            panic!("Don't add primary or payload blocks!");
        }

        // Find the lowest unused block_number
        let mut block_number = 2u64;
        loop {
            if !self.blocks.contains_key(&block_number) {
                return BlockBuilder::new(self, block_number, block_type);
            }
            block_number += 1;
        }
    }

    pub fn replace_block(self, block_type: block::Type) -> BlockBuilder<'a> {
        // TODO:  This is a pretty bad API

        if let block::Type::Primary = block_type {
            panic!("Don't replace primary block!");
        }

        if let Some((block_number, template)) = self
            .blocks
            .iter()
            .find(|(_, block)| match block {
                BlockTemplate::Keep(t) => *t == block_type,
                BlockTemplate::Add(t) => t.block_type == block_type,
            })
            .and_then(|(block_number, template)| match template {
                BlockTemplate::Keep(_) => self.original.blocks.get(block_number).map(|block| {
                    (
                        *block_number,
                        builder::BlockTemplate::new(
                            block_type,
                            block.flags.clone(),
                            block.crc_type,
                        ),
                    )
                }),
                BlockTemplate::Add(template) => Some((*block_number, template.clone())),
            })
        {
            BlockBuilder::new_from_template(self, block_number, template)
        } else {
            self.add_block(block_type)
        }
    }

    pub fn remove_block(mut self, block_number: u64) -> Self {
        if block_number == 0 || block_number == 1 {
            panic!("Don't remove primary or payload blocks!");
        }
        self.blocks.remove(&block_number);
        self
    }

    pub fn rebuild(mut self) -> (bundle::Bundle, Box<[u8]>) {
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

        let data = hardy_cbor::encode::emit_array(None, |a| {
            // Emit primary block
            let primary_block = self.blocks.remove(&0).expect("No primary block!");
            bundle
                .blocks
                .insert(0, self.build_block(0, primary_block, a));

            // Stash payload block
            let payload_block = self.blocks.remove(&1).expect("No payload block!");

            // Emit extension blocks
            for (block_number, block_template) in core::mem::take(&mut self.blocks) {
                bundle.blocks.insert(
                    block_number,
                    self.build_block(block_number, block_template, a),
                );
            }

            // Emit payload block
            bundle
                .blocks
                .insert(1, self.build_block(1, payload_block, a));
        });

        (bundle, data.into())
    }

    fn build_block(
        &self,
        block_number: u64,
        template: BlockTemplate,
        array: &mut hardy_cbor::encode::Array,
    ) -> block::Block {
        match template {
            BlockTemplate::Keep(_) => {
                let mut block = self
                    .original
                    .blocks
                    .get(&block_number)
                    .expect("Mismatched block in bundle!")
                    .clone();
                block.r#move(self.source_data, array);
                block
            }
            BlockTemplate::Add(template) => template.build(block_number, array),
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

    pub fn with_flags(mut self, flags: block::Flags) -> Self {
        self.template.flags = flags;
        self
    }

    pub fn with_crc_type(mut self, crc_type: crc::CrcType) -> Self {
        self.template.crc_type = crc_type;
        self
    }

    pub fn build<T: AsRef<[u8]>>(mut self, data: T) -> Editor<'a> {
        self.template.data = Some(data.as_ref().into());

        self.editor
            .blocks
            .insert(self.block_number, BlockTemplate::Add(self.template));

        self.editor
    }
}
