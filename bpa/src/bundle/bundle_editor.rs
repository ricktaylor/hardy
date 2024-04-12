use super::*;
use crate::store;

pub struct BundleEditor {
    source_bundle: Bundle,
    source_metadata: Metadata,
    blocks: HashMap<u64, BlockTemplate>,
}

enum BlockTemplate {
    Keep(BlockType),
    Add(bundle_builder::BlockTemplate),
}

pub struct BlockBuilder {
    editor: BundleEditor,
    block_number: u64,
    template: bundle_builder::BlockTemplate,
}

impl BundleEditor {
    pub fn new(metadata: Metadata, bundle: Bundle) -> Self {
        Self {
            source_metadata: metadata,
            blocks: bundle
                .blocks
                .iter()
                .map(|(block_number, block)| (*block_number, BlockTemplate::Keep(block.block_type)))
                .collect(),
            source_bundle: bundle,
        }
    }

    pub fn add_extension_block(self, block_type: BlockType) -> BlockBuilder {
        // Find the lowest unused block_number
        let mut block_number = 2u64;
        loop {
            if self.blocks.get(&block_number).is_none() {
                return BlockBuilder::new(self, block_number, block_type);
            }
            block_number += 1;
        }
    }

    pub fn replace_extension_block(self, block_type: BlockType) -> BlockBuilder {
        if let Some((block_number, block)) =
            self.blocks
                .iter()
                .find(|(block_number, block)| match block {
                    BlockTemplate::Keep(t) => *t == block_type,
                    BlockTemplate::Add(t) => t.block_type == block_type,
                })
        {
            let block_number = *block_number;
            BlockBuilder::new(self, block_number, block_type)
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

    pub async fn build(
        mut self,
        store: &store::Store,
    ) -> Result<(Metadata, Bundle), anyhow::Error> {
        // Load up the source bundle data
        let source_data = store.load_data(&self.source_metadata.storage_name).await?;

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
            id: self.source_bundle.id,
            flags: self.source_bundle.flags,
            crc_type: self.source_bundle.crc_type,
            destination: self.source_bundle.destination,
            report_to: self.source_bundle.report_to,
            lifetime: self.source_bundle.lifetime,
            blocks,
            ..Default::default()
        };

        // Update values from supported extension blocks
        parse::check_bundle_blocks(&mut bundle, &data)?;

        // Replace current bundle in store
        let metadata = store
            .replace_data(&self.source_metadata.storage_name, &bundle, data)
            .await?;

        Ok((metadata, bundle))
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
                let Some(block) = self.source_bundle.blocks.get(&block_number) else {
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

impl BlockBuilder {
    fn new(editor: BundleEditor, block_number: u64, block_type: BlockType) -> Self {
        Self {
            template: bundle_builder::BlockTemplate::new(block_type, editor.source_bundle.crc_type),
            block_number,
            editor,
        }
    }

    pub fn must_replicate(mut self, must_replicate: bool) -> Self {
        self.template.flags.must_replicate = must_replicate;
        self
    }

    pub fn report_on_failure(mut self, report_on_failure: bool) -> Self {
        self.template.flags.report_on_failure = report_on_failure;
        self
    }

    pub fn delete_bundle_on_failure(mut self, delete_bundle_on_failure: bool) -> Self {
        self.template.flags.delete_bundle_on_failure = delete_bundle_on_failure;
        self
    }

    pub fn delete_block_on_failure(mut self, delete_block_on_failure: bool) -> Self {
        self.template.flags.delete_block_on_failure = delete_block_on_failure;
        self
    }

    pub fn crc_type(mut self, crc_type: CrcType) -> Self {
        self.template.crc_type = crc_type;
        self
    }

    pub fn build(mut self, data: Vec<u8>) -> BundleEditor {
        // Just copy the data for now
        self.template.data = data;
        self.editor
            .blocks
            .insert(self.block_number, BlockTemplate::Add(self.template));
        self.editor
    }
}
