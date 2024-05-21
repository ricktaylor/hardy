use super::*;
use std::collections::HashMap;

// Default values
const DEFAULT_CRC_TYPE: CrcType = CrcType::CRC32_CASTAGNOLI;
const DEFAULT_LIFETIME: u64 = time::Duration::new(24 * 60 * 60, 0).whole_milliseconds() as u64;

pub struct Builder {
    bundle_flags: BundleFlags,
    crc_type: CrcType,
    source: Eid,
    destination: Eid,
    report_to: Eid,
    lifetime: u64,
    payload: BlockTemplate,
    extensions: Vec<BlockTemplate>,
}

#[derive(Clone)]
pub struct BlockTemplate {
    pub block_type: BlockType,
    pub flags: BlockFlags,
    pub crc_type: CrcType,
    pub data: Vec<u8>,
}

pub struct BlockBuilder {
    builder: Builder,
    template: BlockTemplate,
}

impl Builder {
    pub fn new() -> Self {
        Self {
            bundle_flags: BundleFlags::default(),
            crc_type: DEFAULT_CRC_TYPE,
            source: Eid::default(),
            destination: Eid::default(),
            report_to: Eid::default(),
            lifetime: DEFAULT_LIFETIME,
            payload: BlockTemplate::new(BlockType::Payload, DEFAULT_CRC_TYPE),
            extensions: Vec::new(),
        }
    }

    pub fn flags(mut self, flags: bundle::BundleFlags) -> Self {
        self.bundle_flags = flags;
        self
    }

    /*pub fn crc_type(mut self, crc_type: CrcType) -> Self {
        self.crc_type = crc_type;
        self
    }*/

    pub fn source(mut self, source: &Eid) -> Self {
        self.source = source.clone();
        self
    }

    pub fn destination(mut self, destination: &Eid) -> Self {
        self.destination = destination.clone();
        self
    }

    pub fn report_to(mut self, report_to: &Eid) -> Self {
        self.report_to = report_to.clone();
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

    pub fn build(self) -> Result<(Bundle, Vec<u8>), Error> {
        // Begin indefinite array
        let mut data = vec![(4 << 5) | 31u8];

        // Emit primary block
        let (mut bundle, block_data) = self.build_primary_block();
        data.extend(block_data);

        // Emit extension blocks
        for (block_number, block) in self.extensions.into_iter().enumerate() {
            let (block, block_data) = block.build(block_number as u64 + 2, data.len());
            bundle.blocks.insert(block_number as u64, block);
            data.extend(block_data);
        }

        // Emit payload
        let (block, block_data) = self.payload.build(1, data.len());
        bundle.blocks.insert(1, block);
        data.extend(block_data);

        // End indefinite array
        data.push(0xFF);

        // Update values from supported extension blocks
        parse::check_blocks(&mut bundle, &data)?;

        Ok((bundle, data))
    }

    fn build_primary_block(&self) -> (Bundle, Vec<u8>) {
        let timestamp = time::OffsetDateTime::now_utc();
        let timestamp = CreationTimestamp {
            creation_time: to_dtn_time(&timestamp),
            sequence_number: (timestamp.nanosecond() % 1_000_000) as u64,
        };

        let block_data = crc::emit_crc_value(
            self.crc_type,
            cbor::encode::emit_array(
                Some(if let CrcType::None = self.crc_type {
                    8
                } else {
                    9
                }),
                |a| {
                    // Version
                    a.emit(7);
                    // Flags
                    a.emit::<u64>(self.bundle_flags.into());
                    // CRC
                    a.emit::<u64>(self.crc_type.into());
                    // EIDs
                    a.emit(&self.destination);
                    a.emit(&self.source);
                    a.emit(&self.report_to);
                    // Timestamp
                    a.emit(&timestamp);
                    // Lifetime
                    a.emit(self.lifetime);
                },
            ),
        );

        (
            Bundle {
                id: BundleId {
                    source: self.source.clone(),
                    timestamp,
                    ..Default::default()
                },
                flags: self.bundle_flags,
                crc_type: self.crc_type,
                destination: self.destination.clone(),
                report_to: self.report_to.clone(),
                lifetime: self.lifetime,
                blocks: HashMap::from([(
                    0,
                    Block {
                        block_type: BlockType::Primary,
                        flags: BlockFlags {
                            report_on_failure: true,
                            delete_bundle_on_failure: true,
                            ..Default::default()
                        },
                        crc_type: self.crc_type,
                        data_offset: 1,
                        data_len: block_data.len(),
                    },
                )]),
                ..Default::default()
            },
            block_data,
        )
    }
}

impl BlockBuilder {
    fn new(builder: Builder, block_type: BlockType) -> Self {
        Self {
            template: BlockTemplate::new(block_type, builder.crc_type),
            builder,
        }
    }

    /*pub fn must_replicate(mut self, must_replicate: bool) -> Self {
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
    }*/

    pub fn data(mut self, data: Vec<u8>) -> Self {
        // Just copy the data for now
        self.template.data = data;
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

impl BlockTemplate {
    pub fn new(block_type: BlockType, crc_type: CrcType) -> Self {
        Self {
            block_type,
            flags: BlockFlags::default(),
            crc_type,
            data: Vec::new(),
        }
    }

    pub fn build(self, block_number: u64, offset: usize) -> (Block, Vec<u8>) {
        let block_data = crc::emit_crc_value(
            self.crc_type,
            cbor::encode::emit_array(
                Some(if let CrcType::None = self.crc_type {
                    5
                } else {
                    6
                }),
                |a| {
                    // Block Type
                    a.emit::<u64>(self.block_type.into());
                    // Block Number
                    a.emit(block_number);
                    // Flags
                    a.emit::<u64>(self.flags.into());
                    // CRC Type
                    a.emit::<u64>(self.crc_type.into());
                    // Payload
                    a.emit(&self.data);

                    match self.crc_type {
                        CrcType::None => {}
                        CrcType::CRC16_X25 => a.emit(&[0u8; 2]),
                        CrcType::CRC32_CASTAGNOLI => a.emit(&[0u8; 4]),
                    }
                },
            ),
        );

        (
            Block {
                block_type: self.block_type,
                flags: self.flags,
                crc_type: self.crc_type,
                data_offset: offset,
                data_len: self.data.len(),
            },
            block_data,
        )
    }
}
