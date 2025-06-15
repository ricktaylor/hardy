use super::*;

#[cfg(feature = "std")]
use std::collections::HashMap;

#[cfg(not(feature = "std"))]
use hashbrown::HashMap;

pub struct Editor<'a> {
    original: &'a bundle::Bundle,
    source_data: &'a [u8],
    blocks: HashMap<u64, BlockTemplate>,
}

enum BlockTemplate {
    Keep(block::Type),
    Add(builder::BlockTemplate),
}

pub struct BlockBuilder<'a, 'b> {
    editor: &'b mut Editor<'a>,
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

    pub fn add_extension_block(&mut self, block_type: block::Type) -> BlockBuilder<'a, '_> {
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

    pub fn replace_extension_block(&mut self, block_type: block::Type) -> BlockBuilder<'a, '_> {
        if let block::Type::Primary = block_type {
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

    pub fn remove_extension_block(&mut self, block_number: u64) {
        if block_number == 0 || block_number == 1 {
            panic!("Don't remove primary or payload blocks!");
        }
        self.blocks.remove(&block_number);
    }

    pub fn build(mut self) -> Vec<u8> {
        hardy_cbor::encode::emit_array(None, |a| {
            let primary_block = self.blocks.remove(&0).expect("No primary block!");
            let payload_block = self.blocks.remove(&1).expect("No payload block!");

            // Emit primary block
            self.build_block(0, primary_block, a);

            // Emit extension blocks
            for (block_number, block) in core::mem::take(&mut self.blocks) {
                self.build_block(block_number, block, a);
            }

            // Emit payload block
            self.build_block(1, payload_block, a);
        })
    }

    fn build_block(
        &self,
        block_number: u64,
        template: BlockTemplate,
        array: &mut hardy_cbor::encode::Array,
    ) {
        match template {
            BlockTemplate::Keep(_) => {
                self.original
                    .blocks
                    .get(&block_number)
                    .expect("Mismatched block in bundle!")
                    .copy(self.source_data, array);
            }
            BlockTemplate::Add(template) => {
                template.build(block_number, array);
            }
        }
    }
}

impl<'a, 'b> BlockBuilder<'a, 'b> {
    fn new(editor: &'b mut Editor<'a>, block_number: u64, block_type: block::Type) -> Self {
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
        editor: &'b mut Editor<'a>,
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

    pub fn crc_type(mut self, crc_type: crc::CrcType) -> Self {
        self.template.crc_type(crc_type);
        self
    }

    pub fn data(mut self, data: Vec<u8>) -> Self {
        self.template.data(data);
        self
    }

    pub fn build(self) -> &'b mut Editor<'a> {
        self.editor
            .blocks
            .insert(self.block_number, BlockTemplate::Add(self.template));
        self.editor
    }
}
