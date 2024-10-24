use super::*;
use std::collections::HashMap;

pub struct Editor<'a> {
    original: &'a Bundle,
    blocks: HashMap<u64, BlockTemplate>,
}

enum BlockTemplate {
    Keep(BlockType),
    Add(builder::BlockTemplate),
}

pub struct BlockBuilder<'a> {
    editor: Editor<'a>,
    block_number: u64,
    template: builder::BlockTemplate,
}

impl<'a> Editor<'a> {
    pub fn new(original: &'a Bundle) -> Self {
        Self {
            blocks: original
                .blocks
                .iter()
                .map(|(block_number, block)| (*block_number, BlockTemplate::Keep(block.block_type)))
                .collect(),
            original,
        }
    }

    pub fn add_extension_block(self, block_type: BlockType) -> BlockBuilder<'a> {
        if let BlockType::Primary = block_type {
            panic!("Don't add primary blocks!");
        }
        if let BlockType::Payload = block_type {
            panic!("Don't add payload blocks!");
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

    pub fn replace_extension_block(self, block_type: BlockType) -> BlockBuilder<'a> {
        if let BlockType::Primary = block_type {
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
                        builder::BlockTemplate {
                            block_type,
                            flags: block.flags.clone(),
                            crc_type: block.crc_type,
                            data: Vec::new(),
                        },
                    )
                }),
                BlockTemplate::Add(template) => Some((*block_number, template.clone())),
            })
        {
            BlockBuilder::new_from_template(self, block_number, template)
        } else {
            self.add_extension_block(block_type)
        }
    }

    pub fn remove_extension_block(mut self, block_number: u64) -> Self {
        if block_number == 0 || block_number == 1 {
            panic!("Don't remove primary or payload blocks!");
        }
        self.blocks.remove(&block_number);
        self
    }

    pub fn build(mut self, source_data: &[u8]) -> Result<(Bundle, Vec<u8>), BundleError> {
        // Begin indefinite array
        let mut data = vec![(4 << 5) | 31u8];

        // Create new bundle
        let mut bundle = Bundle {
            id: self.original.id.clone(),
            flags: self.original.flags.clone(),
            crc_type: self.original.crc_type,
            destination: self.original.destination.clone(),
            report_to: self.original.report_to.clone(),
            lifetime: self.original.lifetime,
            ..Default::default()
        };

        // Emit primary block
        let primary_block = self.blocks.remove(&0).expect("No primary block!");
        let (block, block_data) = self.build_block(0, primary_block, source_data, data.len());
        bundle.blocks.insert(0, block);
        data.extend(block_data);

        // Stash payload block for last
        let payload_block = self.blocks.remove(&1).expect("No payload block!");

        // Emit extension blocks
        for (block_number, block) in std::mem::take(&mut self.blocks).into_iter() {
            let (block, block_data) =
                self.build_block(block_number, block, source_data, data.len());
            data.extend(block_data);

            builder::update_extension_blocks(&block, &mut bundle, &data);

            bundle.blocks.insert(block_number, block);
        }

        // Emit payload block
        let (block, block_data) = self.build_block(1, payload_block, source_data, data.len());
        bundle.blocks.insert(1, block);
        data.extend(block_data);

        // End indefinite array
        data.push(0xFF);

        Ok((bundle, data))
    }

    fn build_block(
        &self,
        block_number: u64,
        template: BlockTemplate,
        source_data: &[u8],
        offset: usize,
    ) -> (Block, Vec<u8>) {
        match template {
            BlockTemplate::Keep(_) => {
                let mut block = self
                    .original
                    .blocks
                    .get(&block_number)
                    .expect("Mismatched block in bundle!")
                    .clone();

                let block_data =
                    source_data[block.data_start..block.data_start + block.data_len].to_vec();
                block.data_start = offset;
                (block, block_data)
            }
            BlockTemplate::Add(template) => template.build(block_number, offset),
        }
    }
}

impl<'a> BlockBuilder<'a> {
    fn new(editor: Editor<'a>, block_number: u64, block_type: BlockType) -> Self {
        Self {
            template: builder::BlockTemplate::new(block_type, editor.original.crc_type),
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

    pub fn flags(mut self, flags: BlockFlags) -> Self {
        self.template.flags = flags;
        self
    }

    /*
    pub fn crc_type(mut self, crc_type: CrcType) -> Self {
        self.template.crc_type = crc_type;
        self
    }*/

    pub fn data(mut self, data: Vec<u8>) -> Self {
        self.template.data = data;
        self
    }

    pub fn build(mut self) -> Editor<'a> {
        self.editor
            .blocks
            .insert(self.block_number, BlockTemplate::Add(self.template));
        self.editor
    }
}
