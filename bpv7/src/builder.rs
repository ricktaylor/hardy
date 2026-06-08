use super::*;
use alloc::borrow::Cow;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Cannot add a primary block")]
    PrimaryBlock,

    #[error("No block specific data set")]
    NoBlockData,

    #[error(transparent)]
    InternalError(#[from] error::Error),
}

/// A builder for creating a new bundle.
///
/// [`Builder::build`] returns the parsed [`bundle::Bundle`]
/// view alongside the encoded wire bytes.
///
/// See [`Builder::new()`] for more information.
pub struct Builder<'a> {
    bundle_flags: bundle::Flags,
    crc_type: crc::CrcType,
    source: eid::Eid,
    destination: eid::Eid,
    report_to: Option<eid::Eid>,
    lifetime: core::time::Duration,
    payload: BlockTemplate<'a>,
    extensions: Vec<BlockTemplate<'a>>,
}

impl<'a> Builder<'a> {
    /// Creates a new [`Builder`] for creating a bundle.
    ///
    /// # Examples
    /// ```
    /// use hardy_bpv7::{block, builder::Builder, creation_timestamp::CreationTimestamp};
    ///
    /// let (bundle, data) = Builder::new("ipn:1.0".parse().unwrap(), "ipn:2.0".parse().unwrap())
    ///     .with_report_to("ipn:3.0".parse().unwrap())
    ///     .with_payload("Hello".as_bytes().into())
    ///     .build(CreationTimestamp::now()).unwrap();
    /// ```
    pub fn new(source: eid::Eid, destination: eid::Eid) -> Self {
        Self {
            source,
            destination,
            bundle_flags: bundle::Flags::default(),
            crc_type: crc::CrcType::CRC32_CASTAGNOLI,
            report_to: None,
            lifetime: core::time::Duration::new(24 * 60 * 60, 0),
            payload: BlockTemplate::new(
                block::Type::Payload,
                block::Flags::default(),
                crc::CrcType::CRC32_CASTAGNOLI,
                None,
            ),
            extensions: Vec::new(),
        }
    }

    /// Sets the [`bundle::Flags`] for this [`Builder`].
    pub fn with_flags(mut self, flags: bundle::Flags) -> Self {
        self.bundle_flags = flags;

        // The fragment flag is owned by the fragmentation logic, not the
        // caller: flag it in debug builds to catch API misuse, but always
        // normalise rather than panic in release (this is public API).
        debug_assert!(!self.bundle_flags.is_fragment);
        self.bundle_flags.is_fragment = false;

        self
    }

    /// Sets the [`crc::CrcType`] for this [`Builder`].
    pub fn with_crc_type(mut self, crc_type: crc::CrcType) -> Self {
        self.crc_type = crc_type;
        self
    }

    /// Sets the report_to [`eid::Eid`] for this [`Builder`].
    pub fn with_report_to(mut self, report_to: eid::Eid) -> Self {
        self.report_to = Some(report_to);
        self
    }

    /// Sets the lifetime for this [`Builder`].
    pub fn with_lifetime(mut self, lifetime: core::time::Duration) -> Self {
        self.lifetime = lifetime.min(core::time::Duration::from_millis(u64::MAX));
        self
    }

    /// Adds an extension block to this [`Builder`].
    pub fn add_extension_block(self, block_type: block::Type) -> Result<BlockBuilder<'a>, Error> {
        if let block::Type::Primary = block_type {
            Err(Error::PrimaryBlock)
        } else {
            Ok(BlockBuilder::new(self, block_type))
        }
    }

    /// Adds the payload block to this [`Builder`].
    pub fn with_payload(self, data: Cow<'a, [u8]>) -> Self {
        self.add_extension_block(block::Type::Payload)
            .expect("Failed to add payload block")
            .with_flags(block::Flags {
                delete_bundle_on_failure: true,
                ..Default::default()
            })
            .build(data)
    }

    /// Adds the HopCount block to this [`Builder`].
    pub fn with_hop_count(self, hop_info: &hop_info::HopInfo) -> Self {
        self.add_extension_block(block::Type::HopCount)
            .expect("Failed to add HopCount block")
            .with_flags(block::Flags {
                report_on_failure: true,
                must_replicate: true,
                ..Default::default()
            })
            .build(hardy_cbor::encode::emit(hop_info).0.into())
    }

    /// Builds the bundle with the given timestamp, returning the parsed
    /// [`Bundle`](crate::bundle::Bundle) view (primary block + blocks map) alongside
    /// the encoded wire bytes.
    pub fn build(
        self,
        timestamp: creation_timestamp::CreationTimestamp,
    ) -> Result<(bundle::Bundle, Box<[u8]>), Error> {
        let primary = primary_block::PrimaryBlock {
            flags: self.bundle_flags,
            id: bundle::Id {
                source: self.source.clone(),
                timestamp,
                ..Default::default()
            },
            crc_type: self.crc_type,
            destination: self.destination,
            report_to: self.report_to.unwrap_or(self.source),
            lifetime: self.lifetime,
        };

        let mut blocks = HashMap::new();
        let data = hardy_cbor::encode::try_emit_array(None, |a| {
            // Emit primary block — capture the actual extent in the
            // outer wire stream (after the `0x9F` array head). Discarding
            // this and using `0..primary_bytes.len()` would leave
            // `block.extent` pointing one byte too early; downstream
            // slices via `block.extent` would read the outer array head
            // instead of the primary's own array head.
            let primary_bytes = primary.emit()?;
            let extent = a.emit(&hardy_cbor::encode::Raw(&primary_bytes));
            blocks.insert(
                0,
                primary_block::PrimaryBlock::as_block(primary.crc_type, extent),
            );

            // Emit extension blocks, numbered from 2 (primary is 0, payload
            // is 1).
            for (index, block) in self.extensions.into_iter().enumerate() {
                let block_number = index as u64 + 2;
                blocks.insert(block_number, block.build(block_number, a)?);
            }

            // Emit payload
            blocks.insert(1, self.payload.build(1, a)?);
            Ok::<_, Error>(())
        })?;

        Ok((bundle::Bundle { primary, blocks }, data.into()))
    }
}

/// A builder for creating a new [`block::Block`].
pub struct BlockBuilder<'a> {
    builder: Builder<'a>,
    template: BlockTemplate<'a>,
}

impl<'a> BlockBuilder<'a> {
    /// Creates a new [`BlockBuilder`] for creating a [`block::Block`].
    fn new(builder: Builder<'a>, block_type: block::Type) -> Self {
        Self {
            template: BlockTemplate::new(
                block_type,
                block::Flags::default(),
                builder.crc_type,
                None,
            ),
            builder,
        }
    }

    /// Sets the [`block::Flags`] for this [`BlockBuilder`].
    pub fn with_flags(mut self, flags: block::Flags) -> Self {
        self.template.block.flags = flags;
        self
    }

    /// Sets the [`crc::CrcType`] for this [`BlockBuilder`].
    pub fn with_crc_type(mut self, crc_type: crc::CrcType) -> Self {
        self.template.block.crc_type = crc_type;
        self
    }

    /// Builds the [`block::Block`] with the given data.
    pub fn build(mut self, data: Cow<'a, [u8]>) -> Builder<'a> {
        self.template.data = Some(data);

        if let block::Type::Payload = self.template.block.block_type {
            self.builder.payload = self.template;
        } else {
            self.builder.extensions.push(self.template);
        }
        self.builder
    }
}

/// A template for creating a new [`block::Block`].
#[derive(Clone)]
pub(crate) struct BlockTemplate<'a> {
    pub block: block::Block,
    pub data: Option<Cow<'a, [u8]>>,
}

impl<'a> BlockTemplate<'a> {
    /// Creates a new [`BlockTemplate`] for creating a [`block::Block`].
    pub fn new(
        block_type: block::Type,
        flags: block::Flags,
        crc_type: crc::CrcType,
        data: Option<Cow<'a, [u8]>>,
    ) -> Self {
        Self {
            block: block::Block {
                block_type,
                flags,
                crc_type,
                ..Default::default()
            },
            data,
        }
    }

    /// Builds the [`block::Block`] to standalone bytes.
    pub fn build_to_vec(mut self, block_number: u64) -> Result<(block::Block, Vec<u8>), Error> {
        let data = self.data.take().ok_or(Error::NoBlockData)?;
        let bytes = crc::append_crc_value(
            self.block.crc_type,
            hardy_cbor::encode::emit_array(
                Some(if matches!(self.block.crc_type, crc::CrcType::None) {
                    5
                } else {
                    6
                }),
                |a| {
                    a.emit(&self.block.block_type);
                    a.emit(&block_number);
                    a.emit(&self.block.flags);
                    a.emit(&self.block.crc_type);
                    let data_range = a.emit(&hardy_cbor::encode::Bytes(&data));
                    self.block.data = data_range.start as u64..data_range.end as u64;
                    if !matches!(self.block.crc_type, crc::CrcType::None) {
                        a.skip_value();
                    }
                },
            ),
        )
        .map_err(|e| Error::InternalError(e.into()))?;
        Ok((self.block, bytes))
    }

    /// Builds the [`block::Block`] with the given block number and array.
    pub fn build(
        mut self,
        block_number: u64,
        array: &mut hardy_cbor::encode::Array,
    ) -> Result<block::Block, Error> {
        self.block.emit(
            block_number,
            self.data
                .as_ref()
                .map(|data| data.as_ref())
                .ok_or(Error::NoBlockData)?,
            array,
        )?;
        Ok(self.block)
    }
}

/// A template for creating a new bundle via [`Builder`].
#[derive(Default)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize))]
pub struct BundleTemplate {
    /// The source of the bundle.
    pub source: eid::Eid,
    /// The destination of the bundle.
    pub destination: eid::Eid,
    /// The report_to of the bundle.
    pub report_to: Option<eid::Eid>,
    /// The flags of the bundle.
    pub flags: Option<bundle::Flags>,
    /// The crc_type of the bundle.
    pub crc_type: Option<crc::CrcType>,
    /// The lifetime of the bundle.
    pub lifetime: Option<core::time::Duration>,
    /// The hop_limit of the bundle.
    pub hop_limit: Option<u64>,
}

impl From<BundleTemplate> for Builder<'_> {
    fn from(value: BundleTemplate) -> Self {
        let mut builder = Builder::new(value.source, value.destination);

        if let Some(report_to) = value.report_to {
            builder = builder.with_report_to(report_to);
        }

        if let Some(flags) = value.flags {
            builder = builder.with_flags(flags);
        }

        if let Some(crc_type) = value.crc_type {
            builder = builder.with_crc_type(crc_type);
        }

        if let Some(lifetime) = value.lifetime {
            builder = builder.with_lifetime(lifetime);
        }

        if let Some(hop_limit) = value.hop_limit {
            builder = builder.with_hop_count(&hop_info::HopInfo {
                limit: hop_limit,
                count: 0,
            });
        }

        builder
    }
}

// Requirement: LLR 1.1.25
#[test]
fn test_builder() {
    Builder::new("ipn:1.0".parse().unwrap(), "ipn:2.0".parse().unwrap())
        .with_report_to("ipn:3.0".parse().unwrap())
        .with_payload("Hello".as_bytes().into())
        .build(creation_timestamp::CreationTimestamp::now())
        .unwrap();
}

// Requirement: LLR 1.1.25 — the returned `blocks` map is keyed by wire block
// number (primary 0, payload 1, extensions 2..), matching a parsed bundle.
#[test]
fn test_builder_block_map_keys() {
    let prev_node: eid::Eid = "ipn:3.0".parse().unwrap();
    let (bundle, _data) = Builder::new("ipn:1.0".parse().unwrap(), "ipn:2.0".parse().unwrap())
        .add_extension_block(block::Type::PreviousNode)
        .unwrap()
        .build(hardy_cbor::encode::emit(&prev_node).0.into())
        .add_extension_block(block::Type::BundleAge)
        .unwrap()
        .build(hardy_cbor::encode::emit(&0u64).0.into())
        .with_payload("Hello".as_bytes().into())
        .build(creation_timestamp::CreationTimestamp::now())
        .unwrap();

    assert_eq!(bundle.blocks.len(), 4);
    assert_eq!(bundle.blocks[&0].block_type, block::Type::Primary);
    assert_eq!(bundle.blocks[&1].block_type, block::Type::Payload);
    assert_eq!(bundle.blocks[&2].block_type, block::Type::PreviousNode);
    assert_eq!(bundle.blocks[&3].block_type, block::Type::BundleAge);
}

// Requirement: LLR 1.1.25
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

    b.with_payload("Hello".as_bytes().into())
        .build(creation_timestamp::CreationTimestamp::now())
        .unwrap();
}
