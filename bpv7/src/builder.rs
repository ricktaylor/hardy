use alloc::{borrow::Cow, boxed::Box, vec::Vec};
use core::time::Duration;

use hardy_cbor::encode::{Array, BytesHeader, Raw, emit, emit_array};
use thiserror::Error;

use crate::{HashMap, block, bundle, crc, creation_timestamp, eid, error, hop_info, primary_block};
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
    lifetime: Duration,
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
            lifetime: Duration::new(24 * 60 * 60, 0),
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
    pub fn with_lifetime(mut self, lifetime: Duration) -> Self {
        self.lifetime = lifetime.min(Duration::from_millis(u64::MAX));
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
            .build(emit(hop_info).0.into())
    }

    /// Builds the bundle with the given timestamp, returning the parsed
    /// [`Bundle`](crate::Bundle) view (primary block + blocks map) alongside
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
            let extent = a.emit(&Raw(&primary_bytes));
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

    /// Builds the bundle's wire *prefix* for a payload supplied as a
    /// stream, alongside the parsed [`Bundle`](crate::Bundle) view and the
    /// [`PayloadTrailer`] continuation.
    ///
    /// The prefix runs from the outer array head through the payload
    /// block's byte-string head; the caller emits exactly `payload_len`
    /// payload bytes after it — feeding each run to
    /// [`PayloadTrailer::update`] — and terminates the wire form with the
    /// bytes [`PayloadTrailer::finish`] returns (the payload block's CRC
    /// field, if any, then the outer break). `prefix ++ payload ++ trailer`
    /// is byte-for-byte the [`build`](Self::build) output for the same
    /// inputs, so the assembled form parses canonically.
    ///
    /// The payload block itself is assembled here, not from resident data:
    /// with no [`with_payload`](Self::with_payload) call the block gets
    /// exactly the shape `with_payload` would give it (its flags recipe and
    /// the builder-level CRC type); a template configured through
    /// `with_payload` keeps its flags and CRC type, and only its resident
    /// payload bytes are ignored.
    pub fn build_stream(
        self,
        payload_len: u64,
        timestamp: creation_timestamp::CreationTimestamp,
    ) -> Result<StreamBuild, Error> {
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

        let (payload_flags, payload_crc_type) = if self.payload.data.is_some() {
            (self.payload.block.flags, self.payload.block.crc_type)
        } else {
            (
                block::Flags {
                    delete_bundle_on_failure: true,
                    ..Default::default()
                },
                self.crc_type,
            )
        };

        let mut blocks = HashMap::new();
        let mut trailer = PayloadTrailer { digest: None };
        let mut data = hardy_cbor::encode::try_emit_array(None, |a| {
            // Emit primary block — extent captured in the outer wire
            // stream, exactly as `build` does.
            let primary_bytes = primary.emit()?;
            let extent = a.emit(&Raw(&primary_bytes));
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

            // The payload block header, hand-assembled to stop at the
            // byte-string head: the payload bytes and the CRC value are
            // supplied by the stream and the trailer.
            let mut header = alloc::vec![if matches!(payload_crc_type, crc::CrcType::None) {
                0x85
            } else {
                0x86
            }];
            header.extend(emit(&block::Type::Payload).0);
            header.extend(emit(&1u64).0);
            header.extend(emit(&payload_flags).0);
            header.extend(emit(&payload_crc_type).0);
            header.extend(emit(&BytesHeader(payload_len)).0);
            let data_start = header.len() as u64;

            // The trailer digest covers the block's bytes exactly as
            // `crc::append_crc_value` digests them: the header now, the
            // payload as it streams, the CRC head byte and zeroed value at
            // finish. An unrecognised CRC type errors here.
            if !matches!(payload_crc_type, crc::CrcType::None) {
                let mut digest = crc::Digest::new(payload_crc_type)
                    .map_err(|e| Error::InternalError(e.into()))?;
                digest.push(&header);
                trailer.digest = Some(digest);
            }

            let extent = a.emit(&Raw(&header));

            // The block's extents span the full future wire form (the
            // parser's `Partial` convention): the payload bytes and the CRC
            // field (head byte + big-endian value) follow the prefix on the
            // wire, so `encoded_len`/`payload_range` are exact while the
            // resident-slice accessors return `None` on the prefix alone.
            let crc_field_len = match payload_crc_type {
                crc::CrcType::CRC16_X25 => 3,
                crc::CrcType::CRC32_CASTAGNOLI => 5,
                _ => 0,
            };
            blocks.insert(
                1,
                block::Block {
                    block_type: block::Type::Payload,
                    flags: payload_flags,
                    crc_type: payload_crc_type,
                    data: data_start..data_start + payload_len,
                    extent: extent.start as u64
                        ..extent.start as u64 + data_start + payload_len + crc_field_len,
                    ..Default::default()
                },
            );
            Ok::<_, Error>(())
        })?;

        // `try_emit_array(None, …)` closes the indefinite outer array; the
        // break belongs after the streamed payload, where the trailer
        // re-emits it.
        let _break = data.pop();
        debug_assert_eq!(_break, Some(0xFF));

        Ok(StreamBuild {
            bundle: bundle::Bundle { primary, blocks },
            prefix: data.into(),
            trailer,
        })
    }
}

/// The output of [`Builder::build_stream`]: the parsed bundle view, the
/// resident wire prefix, and the payload-trailer continuation.
pub struct StreamBuild {
    /// The parsed bundle view. Block extents span the full future wire
    /// form — the payload block's extent covers bytes that are not in
    /// [`prefix`](Self::prefix) — so
    /// [`encoded_len`](bundle::Bundle::encoded_len) and
    /// [`payload_range`](block::Block::payload_range) are exact, while
    /// [`Block::payload`](block::Block::payload) against the prefix alone
    /// returns `None` for the payload block.
    pub bundle: bundle::Bundle,
    /// The wire bytes from the outer array head through the payload
    /// block's byte-string head.
    pub prefix: Box<[u8]>,
    /// The emit-side continuation for the streamed payload.
    pub trailer: PayloadTrailer,
}

/// The emit-side continuation for a streamed payload: feed the payload
/// bytes through [`update`](Self::update) as they are emitted, then
/// [`finish`](Self::finish) yields the bytes that terminate the wire form —
/// the payload block's CRC field (when the block carries a CRC) followed by
/// the outer array's break.
pub struct PayloadTrailer {
    digest: Option<crc::Digest>,
}

impl PayloadTrailer {
    /// Absorbs a run of streamed payload bytes into the CRC digest. A
    /// no-op when the payload block carries no CRC.
    pub fn update(&mut self, data: &[u8]) {
        if let Some(digest) = &mut self.digest {
            digest.push(data);
        }
    }

    /// Terminates the wire form: the payload block's CRC field (its head
    /// byte and big-endian value over the block's bytes), then the outer
    /// array's break. The caller must have fed exactly the declared
    /// payload through [`update`](Self::update).
    pub fn finish(self) -> Vec<u8> {
        let Some(mut digest) = self.digest else {
            return alloc::vec![0xFF];
        };
        let head = digest.cbor_head();
        digest.push(&[head]);
        digest.push_zeros();
        let mut out = alloc::vec![head];
        out.extend_from_slice(&digest.finalize());
        out.push(0xFF);
        out
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
            emit_array(
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
    pub fn build(mut self, block_number: u64, array: &mut Array) -> Result<block::Block, Error> {
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
    pub lifetime: Option<Duration>,
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
        .build(emit(&prev_node).0.into())
        .add_extension_block(block::Type::BundleAge)
        .unwrap()
        .build(emit(&0u64).0.into())
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
