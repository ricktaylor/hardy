use super::*;
use hardy_cbor as cbor;
use std::collections::HashMap;

// Default values
const DEFAULT_CRC_TYPE: CrcType = CrcType::CRC32_CASTAGNOLI;
const DEFAULT_LIFETIME: time::Duration = time::Duration::new(24 * 60 * 60, 0);

pub struct BundleBuilder {
    bundle_flags: BundleFlags,
    crc_type: CrcType,
    source: Eid,
    destination: Eid,
    report_to: Eid,
    lifetime: time::Duration,
    payload: BlockTemplate,
    extensions: Vec<BlockTemplate>,
}

struct BlockTemplate {
    block_type: BlockType,
    flags: BlockFlags,
    crc_type: CrcType,
    data: Vec<u8>,
}

pub struct BlockBuilder {
    builder: BundleBuilder,
    template: BlockTemplate,
}

impl BundleBuilder {
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

    #[allow(clippy::wrong_self_convention)]
    pub fn is_admin_record(mut self, is_admin_record: bool) -> Self {
        self.bundle_flags.is_admin_record = is_admin_record;
        self
    }

    pub fn do_not_fragment(mut self, do_not_fragment: bool) -> Self {
        self.bundle_flags.do_not_fragment = do_not_fragment;
        self
    }

    pub fn app_ack_requested(mut self, app_ack_requested: bool) -> Self {
        self.bundle_flags.app_ack_requested = app_ack_requested;
        self
    }

    pub fn report_status_time(mut self, report_status_time: bool) -> Self {
        self.bundle_flags.report_status_time = report_status_time;
        self
    }

    pub fn receipt_report_requested(mut self, receipt_report_requested: bool) -> Self {
        self.bundle_flags.receipt_report_requested = receipt_report_requested;
        self
    }

    pub fn forward_report_requested(mut self, forward_report_requested: bool) -> Self {
        self.bundle_flags.forward_report_requested = forward_report_requested;
        self
    }

    pub fn delivery_report_requested(mut self, delivery_report_requested: bool) -> Self {
        self.bundle_flags.delivery_report_requested = delivery_report_requested;
        self
    }

    pub fn delete_report_requested(mut self, delete_report_requested: bool) -> Self {
        self.bundle_flags.delete_report_requested = delete_report_requested;
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
        self.report_to = report_to;
        self
    }

    pub fn lifetime(mut self, lifetime: time::Duration) -> Self {
        self.lifetime = lifetime;
        self
    }

    pub fn add_extension_block(self, block_type: BlockType) -> BlockBuilder {
        BlockBuilder::new(self, block_type)
    }

    pub fn add_payload_block(self, data: Vec<u8>) -> Self {
        self.add_extension_block(BlockType::Payload).build(data)
    }

    pub fn build(self) -> (Bundle, Vec<u8>) {
        // Begin indefinite array
        let mut data = vec![(4 << 5) | 31u8];

        // Emit primary block
        let (mut bundle, block_data) = self.build_primary_block();
        data.extend(block_data);

        // Emit extension blocks
        for (block_number, block) in self.extensions.into_iter().enumerate() {
            let (block, block_data) = block.build(block_number + 2, data.len());
            bundle.blocks.insert(block_number as u64, block);
            data.extend(block_data);
        }

        // Emit payload
        let (block, block_data) = self.payload.build(1, data.len());
        bundle.blocks.insert(1, block);
        data.extend(block_data);

        // End indefinite array
        data.push(0xFF);

        (bundle, data)
    }

    fn build_primary_block(&self) -> (Bundle, Vec<u8>) {
        let timestamp = time::OffsetDateTime::now_utc();
        let timestamp = (
            dtn_time(&timestamp),
            (timestamp.nanosecond() % 1_000_000) as u64,
        );

        (
            Bundle {
                id: BundleId {
                    source: self.source.clone(),
                    timestamp,
                    fragment_info: None,
                },
                flags: self.bundle_flags,
                crc_type: self.crc_type,
                destination: self.destination.clone(),
                report_to: self.report_to.clone(),
                lifetime: self.lifetime.whole_milliseconds() as u64,
                blocks: HashMap::new(),
            },
            emit_crc_value(
                vec![
                    // Version
                    cbor::encode::emit(7u8),
                    // Flags
                    cbor::encode::emit(self.bundle_flags),
                    // CRC
                    cbor::encode::emit(self.crc_type),
                    // EIDs
                    cbor::encode::emit(&self.destination),
                    cbor::encode::emit(&self.source),
                    cbor::encode::emit(&self.report_to),
                    // Timestamp
                    cbor::encode::emit([
                        cbor::encode::emit(timestamp.0),
                        cbor::encode::emit(timestamp.1),
                    ]),
                    // Lifetime
                    cbor::encode::emit(self.lifetime.whole_milliseconds() as u64),
                ],
                self.crc_type,
            ),
        )
    }
}

impl BlockBuilder {
    fn new(builder: BundleBuilder, block_type: BlockType) -> Self {
        Self {
            template: BlockTemplate::new(block_type, builder.crc_type),
            builder,
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

    pub fn build(mut self, data: Vec<u8>) -> BundleBuilder {
        // Just copy the data for now
        self.template.data = data;

        if let BlockType::Payload = self.template.block_type {
            self.builder.payload = self.template;
        } else {
            self.builder.extensions.push(self.template);
        }
        self.builder
    }
}

impl BlockTemplate {
    fn new(block_type: BlockType, crc_type: CrcType) -> Self {
        Self {
            block_type,
            flags: BlockFlags::default(),
            crc_type,
            data: Vec::new(),
        }
    }

    fn build(self, block_number: usize, offset: usize) -> (Block, Vec<u8>) {
        (
            Block {
                block_type: self.block_type,
                flags: self.flags,
                crc_type: self.crc_type,
                data_offset: offset,
                data_len: self.data.len(),
            },
            emit_crc_value(
                vec![
                    // Block Type
                    cbor::encode::emit(self.block_type),
                    // Block Number
                    cbor::encode::emit(block_number),
                    // Flags
                    cbor::encode::emit(self.flags),
                    // CRC Type
                    cbor::encode::emit(self.crc_type),
                    // Payload
                    cbor::encode::emit(self.data),
                ],
                self.crc_type,
            ),
        )
    }
}
