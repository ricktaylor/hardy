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
                            flags: block.flags,
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
            panic!("Don't edit primary or payload blocks!");
        }
        self.blocks.remove(&block_number);
        self
    }

    pub fn build(mut self, source_data: &[u8]) -> Result<(Bundle, Box<[u8]>), BundleError> {
        // Begin indefinite array
        let mut data = vec![(4 << 5) | 31u8];
        let mut blocks = HashMap::new();

        // Build Primary Block first
        let Some(primary_block) = self.blocks.remove(&0) else {
            panic!("No Primary Block!")
        };
        let (block, block_data) =
            self.build_block(0, primary_block, (*source_data).as_ref(), data.len());
        blocks.insert(0, block);
        data.extend(block_data);

        // Stash payload block for last
        let Some(payload_block) = self.blocks.remove(&1) else {
            panic!("No payload block!")
        };

        // Emit extension blocks
        for (block_number, block) in std::mem::take(&mut self.blocks).into_iter() {
            let (block, block_data) =
                self.build_block(block_number, block, (*source_data).as_ref(), data.len());
            blocks.insert(block_number, block);
            data.extend(block_data);
        }

        // Emit payload block
        let (block, block_data) =
            self.build_block(1, payload_block, (*source_data).as_ref(), data.len());
        blocks.insert(1, block);
        data.extend(block_data);

        // End indefinite array
        data.push(0xFF);

        // Compose bundle
        let mut bundle = Bundle {
            id: self.original.id.clone(),
            flags: self.original.flags,
            crc_type: self.original.crc_type,
            destination: self.original.destination.clone(),
            report_to: self.original.report_to.clone(),
            lifetime: self.original.lifetime,
            blocks,
            ..Default::default()
        };

        // Update values from supported extension blocks
        bundle.parse_extension_blocks(&data)?;

        Ok((bundle, Box::from(data)))
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
                let Some(block) = self.original.blocks.get(&block_number) else {
                    panic!("Mismatched block in bundle!")
                };
                (
                    Block {
                        block_type: block.block_type,
                        flags: block.flags,
                        crc_type: block.crc_type,
                        data_offset: offset,
                        data_len: block.data_len,
                    },
                    source_data[block.data_offset..(block.data_offset + block.data_len)].to_vec(),
                )
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
