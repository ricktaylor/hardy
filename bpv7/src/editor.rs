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
                BlockTemplate::Add(t) => t.block_type() == block_type,
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

    pub fn build(mut self, source_data: &[u8]) -> Result<Vec<u8>, BundleError> {
        let primary_block = self.blocks.remove(&0).expect("No primary block!");
        let payload_block = self.blocks.remove(&1).expect("No payload block!");

        // Begin indefinite array
        let mut data = vec![(4 << 5) | 31u8];

        // Emit primary block
        data.extend(self.build_block(0, primary_block, source_data, data.len()));

        // Emit extension blocks
        for (block_number, block) in std::mem::take(&mut self.blocks) {
            data.extend(self.build_block(block_number, block, source_data, data.len()));
        }

        // Emit payload block
        data.extend(self.build_block(1, payload_block, source_data, data.len()));

        // End indefinite array
        data.push(0xFF);

        Ok(data)
    }

    fn build_block(
        &self,
        block_number: u64,
        template: BlockTemplate,
        data: &[u8],
        offset: usize,
    ) -> Vec<u8> {
        match template {
            BlockTemplate::Keep(_) => {
                let block = self
                    .original
                    .blocks
                    .get(&block_number)
                    .expect("Mismatched block in bundle!");
                data[block.data_start..block.data_start + block.data_len].to_vec()
            }
            BlockTemplate::Add(template) => template.build(block_number, offset).1,
        }
    }
}

impl<'a> BlockBuilder<'a> {
    fn new(editor: Editor<'a>, block_number: u64, block_type: BlockType) -> Self {
        Self {
            template: builder::BlockTemplate::new(
                block_type,
                BlockFlags::default(),
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

    pub fn must_replicate(mut self, must_replicate: bool) -> Self {
        self.template.must_replicate(must_replicate);
        self
    }

    pub fn report_on_failure(mut self, report_on_failure: bool) -> Self {
        self.template.report_on_failure(report_on_failure);
        self
    }

    pub fn delete_bundle_on_failure(mut self, delete_bundle_on_failure: bool) -> Self {
        self.template
            .delete_bundle_on_failure(delete_bundle_on_failure);
        self
    }

    pub fn delete_block_on_failure(mut self, delete_block_on_failure: bool) -> Self {
        self.template
            .delete_block_on_failure(delete_block_on_failure);
        self
    }

    pub fn crc_type(mut self, crc_type: CrcType) -> Self {
        self.template.crc_type(crc_type);
        self
    }

    pub fn data(mut self, data: Vec<u8>) -> Self {
        self.template.data(data);
        self
    }

    pub fn build(mut self) -> Editor<'a> {
        self.editor
            .blocks
            .insert(self.block_number, BlockTemplate::Add(self.template));
        self.editor
    }
}
