use super::*;

// Default values
const DEFAULT_CRC_TYPE: CrcType = CrcType::CRC32_CASTAGNOLI;
const DEFAULT_LIFETIME: u64 = time::Duration::new(24 * 60 * 60, 0).whole_milliseconds() as u64;

pub struct Builder {
    bundle_flags: BundleFlags,
    crc_type: CrcType,
    source: Eid,
    destination: Eid,
    report_to: Option<Eid>,
    lifetime: u64,
    payload: BlockTemplate,
    extensions: Vec<BlockTemplate>,
}

impl Default for Builder {
    fn default() -> Self {
        Self {
            bundle_flags: BundleFlags::default(),
            crc_type: DEFAULT_CRC_TYPE,
            source: Eid::default(),
            destination: Eid::default(),
            report_to: None,
            lifetime: DEFAULT_LIFETIME,
            payload: BlockTemplate::new(
                BlockType::Payload,
                BlockFlags::default(),
                DEFAULT_CRC_TYPE,
            ),
            extensions: Vec::new(),
        }
    }
}

impl Builder {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn flags(mut self, flags: BundleFlags) -> Self {
        self.bundle_flags = flags;
        self
    }

    pub fn crc_type(mut self, crc_type: CrcType) -> Self {
        self.crc_type = crc_type;
        self
    }

    pub fn source(mut self, source: Eid) -> Self {
        self.source = source;
        self
    }

    pub fn destination(mut self, destination: Eid) -> Self {
        self.destination = destination;
        self
    }

    pub fn report_to(mut self, report_to: Eid) -> Self {
        self.report_to = Some(report_to);
        self
    }

    pub fn lifetime(mut self, lifetime: u64) -> Self {
        self.lifetime = lifetime;
        self
    }

    pub fn add_extension_block(self, block_type: BlockType) -> BlockBuilder {
        BlockBuilder::new(self, block_type)
    }

    pub fn add_payload_block(self, data: Vec<u8>) -> Self {
        self.add_extension_block(BlockType::Payload)
            .data(data)
            .build()
    }

    pub fn build(mut self) -> Result<(Bundle, Vec<u8>), BundleError> {
        // Begin indefinite array
        let mut data = vec![(4 << 5) | 31u8];

        // Emit primary block
        let (mut bundle, block_data) = self.build_primary_block(data.len());
        data.extend(block_data);

        // Emit extension blocks
        for (block_number, block) in self.extensions.into_iter().enumerate() {
            let (block, block_data) = block.build(block_number as u64 + 2, data.len());
            data.extend(block_data);
            bundle.blocks.insert(block_number as u64, block);
        }

        // Emit payload
        let (block, block_data) = self.payload.build(1, data.len());
        bundle.blocks.insert(1, block);
        data.extend(block_data);

        // End indefinite array
        data.push(0xFF);

        Ok((bundle, data))
    }

    fn build_primary_block(&mut self, offset: usize) -> (Bundle, Vec<u8>) {
        let mut bundle = Bundle {
            report_to: if let Some(report_to) = &mut self.report_to {
                std::mem::take(report_to)
            } else {
                self.source.clone()
            },
            id: BundleId {
                source: std::mem::take(&mut self.source),
                timestamp: CreationTimestamp::now(),
                ..Default::default()
            },
            flags: self.bundle_flags.clone(),
            crc_type: self.crc_type,
            destination: std::mem::take(&mut self.destination),
            lifetime: self.lifetime,
            ..Default::default()
        };

        let block_data = bundle.emit_primary_block(offset);
        (bundle, block_data)
    }
}

pub struct BlockBuilder {
    builder: Builder,
    template: BlockTemplate,
}

impl BlockBuilder {
    fn new(builder: Builder, block_type: BlockType) -> Self {
        Self {
            template: BlockTemplate::new(block_type, BlockFlags::default(), builder.crc_type),
            builder,
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

    pub fn build(mut self) -> Builder {
        if let BlockType::Payload = self.template.block_type {
            self.builder.payload = self.template;
        } else {
            self.builder.extensions.push(self.template);
        }
        self.builder
    }
}

#[derive(Clone)]
pub struct BlockTemplate {
    block_type: BlockType,
    flags: BlockFlags,
    crc_type: CrcType,
    data: Vec<u8>,
}

impl BlockTemplate {
    pub fn new(block_type: BlockType, flags: BlockFlags, crc_type: CrcType) -> Self {
        Self {
            block_type,
            flags,
            crc_type,
            data: Vec::new(),
        }
    }

    pub fn block_type(&self) -> BlockType {
        self.block_type
    }

    pub fn must_replicate(&mut self, must_replicate: bool) {
        self.flags.must_replicate = must_replicate;
    }

    pub fn report_on_failure(&mut self, report_on_failure: bool) {
        self.flags.report_on_failure = report_on_failure;
    }

    pub fn delete_bundle_on_failure(&mut self, delete_bundle_on_failure: bool) {
        self.flags.delete_bundle_on_failure = delete_bundle_on_failure;
    }

    pub fn delete_block_on_failure(&mut self, delete_block_on_failure: bool) {
        self.flags.delete_block_on_failure = delete_block_on_failure;
    }

    pub fn crc_type(&mut self, crc_type: CrcType) {
        self.crc_type = crc_type;
    }

    pub fn data(&mut self, data: Vec<u8>) {
        // Just copy the data for now
        self.data = data;
    }

    pub fn build(self, block_number: u64, data_start: usize) -> (Block, Vec<u8>) {
        let mut block = Block {
            block_type: self.block_type,
            flags: self.flags,
            crc_type: self.crc_type,
            data_start: 0,
            payload_offset: 0,
            data_len: 0,
        };

        let block_data = block.emit(block_number, &self.data, data_start);
        (block, block_data)
    }
}

#[test]
fn test() {
    Builder::new()
        .source("ipn:1.0".parse().unwrap())
        .destination("ipn:2.0".parse().unwrap())
        .report_to("ipn:3.0".parse().unwrap())
        .build()
        .unwrap();
}
