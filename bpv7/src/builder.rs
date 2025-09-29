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

impl Builder {
    pub fn new(source: eid::Eid, destination: eid::Eid) -> Self {
        Self {
            source,
            destination,
            bundle_flags: bundle::Flags::default(),
            crc_type: crc::CrcType::CRC32_CASTAGNOLI,
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

    pub fn with_flags(&mut self, flags: bundle::Flags) -> &mut Self {
        self.bundle_flags = flags;
        self
    }

    pub fn with_crc_type(&mut self, crc_type: crc::CrcType) -> &mut Self {
        self.crc_type = crc_type;
        self
    }

    pub fn with_report_to(&mut self, report_to: eid::Eid) -> &mut Self {
        self.report_to = Some(report_to);
        self
    }

    pub fn with_lifetime(&mut self, lifetime: core::time::Duration) -> &mut Self {
        self.lifetime = lifetime.min(core::time::Duration::from_millis(u64::MAX));
        self
    }

    pub fn add_extension_block(&mut self, block_type: block::Type) -> BlockBuilder<'_> {
        BlockBuilder::new(self, block_type)
    }

    pub fn build<T: AsRef<[u8]>>(
        mut self,
        payload: T,
        timestamp: creation_timestamp::CreationTimestamp,
    ) -> (bundle::Bundle, Box<[u8]>) {
        self.add_extension_block(block::Type::Payload)
            .build(payload);

        let mut bundle = bundle::Bundle {
            report_to: self.report_to.unwrap_or(self.source.clone()),
            id: bundle::Id {
                source: self.source,
                timestamp,
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
            bundle
                .emit_primary_block(a)
                .expect("Failed to emit primary block");

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

    pub fn with_flags(&mut self, flags: block::Flags) -> &mut Self {
        self.template.flags = flags;
        self
    }

    pub fn with_crc_type(&mut self, crc_type: crc::CrcType) -> &mut Self {
        self.template.crc_type = crc_type;
        self
    }

    pub fn build<T: AsRef<[u8]>>(mut self, data: T) -> &'a mut Builder {
        self.template.data = Some(data.as_ref().into());

        if let block::Type::Payload = self.template.block_type {
            self.builder.payload = self.template;
        } else {
            self.builder.extensions.push(self.template);
        }
        self.builder
    }
}

#[derive(Clone)]
pub(crate) struct BlockTemplate {
    pub block_type: block::Type,
    pub flags: block::Flags,
    pub crc_type: crc::CrcType,
    pub data: Option<Box<[u8]>>,
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
        block
            .emit(
                block_number,
                &self.data.expect("No block specific data set"),
                array,
            )
            .expect("Failed to emit block");
        block
    }
}

#[cfg_attr(feature = "serde", derive(serde::Deserialize))]
pub struct BundleTemplate {
    pub source: eid::Eid,
    pub destination: eid::Eid,
    pub report_to: Option<eid::Eid>,
    pub flags: Option<bundle::Flags>,
    pub crc_type: Option<crc::CrcType>,
    pub lifetime: Option<core::time::Duration>,
    pub hop_limit: Option<u64>,
}

impl From<BundleTemplate> for Builder {
    fn from(value: BundleTemplate) -> Self {
        let mut builder = Builder::new(value.source, value.destination);

        if let Some(report_to) = value.report_to {
            builder.with_report_to(report_to);
        }

        if let Some(flags) = value.flags {
            builder.with_flags(flags);
        }

        if let Some(crc_type) = value.crc_type {
            builder.with_crc_type(crc_type);
        }

        if let Some(lifetime) = value.lifetime {
            builder.with_lifetime(lifetime);
        }

        if let Some(hop_limit) = value.hop_limit {
            let mut builder = builder.add_extension_block(block::Type::HopCount);
            builder.with_flags(block::Flags {
                must_replicate: true,
                delete_bundle_on_failure: true,
                ..Default::default()
            });
            builder.build(
                hardy_cbor::encode::emit(&hop_info::HopInfo {
                    limit: hop_limit,
                    count: 0,
                })
                .0,
            );
        }

        builder
    }
}

#[test]
fn test_builder() {
    let mut b = Builder::new("ipn:1.0".parse().unwrap(), "ipn:2.0".parse().unwrap());
    b.with_report_to("ipn:3.0".parse().unwrap());
    b.build("Hello", creation_timestamp::CreationTimestamp::now());
}

#[cfg(feature = "serde")]
#[test]
fn test_template() {
    let b: Builder = serde_json::from_value::<BundleTemplate>(serde_json::json!({
        "source": "ipn:1.0",
        "destination": "ipn:2.0",
        "report_to": "ipn:3.0"
    }))
    .unwrap()
    .into();

    b.build("Hello", creation_timestamp::CreationTimestamp::now());
}
