use super::*;

pub struct Builder {
    bundle_flags: bundle::Flags,
    crc_type: crc::CrcType,
    source: eid::Eid,
    destination: eid::Eid,
    report_to: Option<eid::Eid>,
    lifetime: core::time::Duration,
    payload: BlockTemplate,
    extensions: Vec<BlockTemplate>,
}

impl Default for Builder {
    fn default() -> Self {
        Self {
            bundle_flags: bundle::Flags::default(),
            crc_type: crc::CrcType::CRC32_CASTAGNOLI,
            source: eid::Eid::default(),
            destination: eid::Eid::default(),
            report_to: None,
            lifetime: core::time::Duration::new(24 * 60 * 60 * 60, 0),
            payload: BlockTemplate::new(
                block::Type::Payload,
                block::Flags::default(),
                crc::CrcType::CRC32_CASTAGNOLI,
            ),
            extensions: Vec::new(),
        }
    }
}

impl Builder {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn flags(&mut self, flags: bundle::Flags) -> &mut Self {
        self.bundle_flags = flags;
        self
    }

    pub fn crc_type(&mut self, crc_type: crc::CrcType) -> &mut Self {
        self.crc_type = crc_type;
        self
    }

    pub fn source(&mut self, source: eid::Eid) -> &mut Self {
        self.source = source;
        self
    }

    pub fn destination(&mut self, destination: eid::Eid) -> &mut Self {
        self.destination = destination;
        self
    }

    pub fn report_to(&mut self, report_to: eid::Eid) -> &mut Self {
        self.report_to = Some(report_to);
        self
    }

    pub fn lifetime(&mut self, lifetime: core::time::Duration) -> &mut Self {
        self.lifetime = lifetime.min(core::time::Duration::from_millis(u64::MAX));
        self
    }

    pub fn add_extension_block(&mut self, block_type: block::Type) -> BlockBuilder<'_> {
        BlockBuilder::new(self, block_type)
    }

    pub fn add_payload_block<T: AsRef<[u8]>>(&mut self, data: T) -> &mut Self {
        self.add_extension_block(block::Type::Payload)
            .data(data)
            .build()
    }

    pub fn build(self) -> (bundle::Bundle, Box<[u8]>) {
        let mut bundle = bundle::Bundle {
            report_to: self.report_to.unwrap_or(self.source.clone()),
            id: bundle::Id {
                source: self.source,
                timestamp: creation_timestamp::CreationTimestamp::now(),
                ..Default::default()
            },
            flags: self.bundle_flags.clone(),
            crc_type: self.crc_type,
            destination: self.destination,
            lifetime: self.lifetime,
            ..Default::default()
        };

        let data = hardy_cbor::encode::emit_array(None, |a| {
            // Emit primary block
            bundle.emit_primary_block(a);

            // Emit extension blocks
            for (block_number, block) in self.extensions.into_iter().enumerate() {
                bundle
                    .blocks
                    .insert(block_number as u64, block.build(block_number as u64 + 2, a));
            }

            // Emit payload
            bundle.blocks.insert(1, self.payload.build(1, a));
        });

        (bundle, data.into())
    }
}

pub struct BlockBuilder<'a> {
    builder: &'a mut Builder,
    template: BlockTemplate,
}

impl<'a> BlockBuilder<'a> {
    fn new(builder: &'a mut Builder, block_type: block::Type) -> Self {
        Self {
            template: BlockTemplate::new(block_type, block::Flags::default(), builder.crc_type),
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

    pub fn crc_type(mut self, crc_type: crc::CrcType) -> Self {
        self.template.crc_type(crc_type);
        self
    }

    pub fn data<T: AsRef<[u8]>>(mut self, data: T) -> Self {
        self.template.data(data);
        self
    }

    pub fn build(self) -> &'a mut Builder {
        if let block::Type::Payload = self.template.block_type {
            self.builder.payload = self.template;
        } else {
            self.builder.extensions.push(self.template);
        }
        self.builder
    }
}

#[derive(Clone)]
pub struct BlockTemplate {
    block_type: block::Type,
    flags: block::Flags,
    crc_type: crc::CrcType,
    data: Option<Box<[u8]>>,
}

impl BlockTemplate {
    pub fn new(block_type: block::Type, flags: block::Flags, crc_type: crc::CrcType) -> Self {
        Self {
            block_type,
            flags,
            crc_type,
            data: None,
        }
    }

    pub fn block_type(&self) -> block::Type {
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

    pub fn crc_type(&mut self, crc_type: crc::CrcType) {
        self.crc_type = crc_type;
    }

    pub fn data<T: AsRef<[u8]>>(&mut self, data: T) {
        // Just copy the data for now
        self.data = Some(data.as_ref().into());
    }

    pub fn build(self, block_number: u64, array: &mut hardy_cbor::encode::Array) -> block::Block {
        let mut block = block::Block {
            block_type: self.block_type,
            flags: self.flags,
            crc_type: self.crc_type,
            extent: 0..0,
            data: 0..0,
            bib: None,
            bcb: None,
        };
        block.emit(
            block_number,
            &self.data.expect("No block specific data set"),
            array,
        );
        block
    }
}

#[test]
fn test() {
    let mut b = Builder::new();

    b.source("ipn:1.0".parse().unwrap())
        .destination("ipn:2.0".parse().unwrap())
        .report_to("ipn:3.0".parse().unwrap())
        .add_payload_block("Hello");

    b.build();
}
