use super::*;
use std::collections::{HashMap, HashSet};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum BundleError {
    #[error("Bundle has additional data after end of CBOR array")]
    AdditionalData,

    #[error("Unsupported bundle protocol version {0}")]
    UnsupportedVersion(u64),

    #[error("Bundle has no payload block")]
    MissingPayload,

    #[error("Bundle payload block must be block number 1")]
    InvalidPayloadBlockNumber,

    #[error("Final block of bundle is not a payload block")]
    PayloadNotFinal,

    #[error("Bundle has more than one block with block number {0}")]
    DuplicateBlockNumber(u64),

    #[error("Block number {0} is invalid for a {1} block")]
    InvalidBlockNumber(u64, BlockType),

    #[error("Bundle has multiple {0} blocks")]
    DuplicateBlocks(BlockType),

    #[error("{0} block has additional data")]
    BlockAdditionalData(BlockType),

    #[error("Bundle source has no clock, and there is no Bundle Age extension block")]
    MissingBundleAge,

    #[error(transparent)]
    InvalidBPSec(#[from] bpsec::Error),

    #[error(transparent)]
    InvalidCrc(#[from] crc::Error),

    #[error(transparent)]
    InvalidEid(#[from] eid::EidError),

    #[error("Failed to parse {field}: {source}")]
    InvalidField {
        field: &'static str,
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error(transparent)]
    InvalidCBOR(#[from] cbor::decode::Error),
}

pub trait CaptureFieldErr<T> {
    fn map_field_err(self, field: &'static str) -> Result<T, BundleError>;
}

impl<T, E: Into<Box<dyn std::error::Error + Send + Sync>>> CaptureFieldErr<T>
    for std::result::Result<T, E>
{
    fn map_field_err(self, field: &'static str) -> Result<T, BundleError> {
        self.map_err(|e| BundleError::InvalidField {
            field,
            source: e.into(),
        })
    }
}

#[derive(Default, Debug, Clone)]
pub struct Bundle {
    // From Primary Block
    pub id: BundleId,
    pub flags: BundleFlags,
    pub crc_type: CrcType,
    pub destination: Eid,
    pub report_to: Eid,
    pub lifetime: u64,

    // Unpacked from extension blocks
    pub previous_node: Option<Eid>,
    pub age: Option<u64>,
    pub hop_count: Option<HopInfo>,

    // The extension blocks
    pub blocks: std::collections::HashMap<u64, Block>,
}

fn parse_primary_block(
    data: &[u8],
    block: &mut cbor::decode::Array,
    block_start: usize,
) -> Result<(Bundle, bool, bool), BundleError> {
    // Check version
    let (version, mut shortest) = block.parse().map_field_err("Version")?;
    if version != 7 {
        return Err(BundleError::UnsupportedVersion(version));
    }

    // Parse flags
    let flags = block
        .parse::<(BundleFlags, bool)>()
        .map(|(v, s)| {
            shortest = shortest && s;
            v
        })
        .map_field_err("Bundle Processing Control Flags")?;

    // Parse CRC Type
    let crc_type = block.parse();

    // Parse EIDs
    let dest_eid = block.parse();
    let source_eid = block.parse();
    let report_to = block
        .parse()
        .map(|(v, s)| {
            shortest = shortest && s;
            v
        })
        .map_field_err("Report-to EID")?;

    // Parse timestamp
    let timestamp = block.parse();

    // Parse lifetime
    let lifetime = block.parse();

    // Parse fragment parts
    let fragment_info = if !flags.is_fragment {
        Ok(None)
    } else {
        match (block.parse(), block.parse()) {
            (Ok((offset, s1)), Ok((total_len, s2))) => {
                shortest = shortest && s1 && s2;
                Ok(Some(FragmentInfo { offset, total_len }))
            }
            (Err(e), _) => Err(e),
            (_, Err(e)) => Err(e),
        }
    };

    // Try to parse and check CRC
    let crc_result = crc_type.map(|(crc_type, s1)| {
        match crc::parse_crc_value(&data[block_start..], block, crc_type) {
            Ok(s2) => (true, crc_type, s1 && s2),
            _ => (false, crc_type, s1),
        }
    });

    // By the time we get here we have just enough information to react to an invalid primary block
    match (
        dest_eid,
        source_eid,
        timestamp,
        lifetime,
        fragment_info,
        crc_result,
    ) {
        (
            Ok((destination, s1)),
            Ok((source, s2)),
            Ok((timestamp, s3)),
            Ok((lifetime, s4)),
            Ok(fragment_info),
            Ok((true, crc_type, s5)),
        ) => {
            let mut valid = true;

            // Check flags
            if let Eid::Null = source {
                if flags.is_fragment
                    || !flags.do_not_fragment
                    || flags.receipt_report_requested
                    || flags.forward_report_requested
                    || flags.delivery_report_requested
                    || flags.delete_report_requested
                {
                    // Invalid flag combination https://www.rfc-editor.org/rfc/rfc9171.html#section-4.2.3-5
                    valid = false;
                }
            } else if flags.is_admin_record
                && (flags.receipt_report_requested
                    || flags.forward_report_requested
                    || flags.delivery_report_requested
                    || flags.delete_report_requested)
            {
                // Invalid flag combination https://www.rfc-editor.org/rfc/rfc9171.html#section-4.2.3-4
                valid = false;
            }

            Ok((
                Bundle {
                    id: BundleId {
                        source,
                        timestamp,
                        fragment_info,
                    },
                    flags,
                    crc_type,
                    destination,
                    report_to,
                    lifetime,
                    ..Default::default()
                },
                valid,
                shortest && s1 && s2 && s3 && s4 && s5,
            ))
        }
        (dest_eid, source_eid, timestamp, lifetime, fragment_info, crc_result) => {
            Ok((
                // Compose something out of what we have!
                Bundle {
                    id: BundleId {
                        source: source_eid.map_or(Eid::Null, |(v, _)| v),
                        timestamp: timestamp.map_or(Default::default(), |(v, _)| v),
                        fragment_info: fragment_info.unwrap_or(None),
                    },
                    flags,
                    crc_type: crc_result.map_or(CrcType::None, |(_, t, _)| t),
                    destination: dest_eid.map_or(Eid::Null, |(v, _)| v),
                    report_to,
                    lifetime: lifetime.map_or(0, |(v, _)| v),
                    ..Default::default()
                },
                false,
                false,
            ))
        }
    }
}

fn parse_payload<T>(block: &Block, data: &[u8]) -> Result<(T, bool), BundleError>
where
    T: cbor::decode::FromCbor<Error: From<cbor::decode::Error>>,
    BundleError: From<<T as cbor::decode::FromCbor>::Error>,
{
    let data = block.block_data(data)?;
    let (v, s, len) = cbor::decode::parse(&data)?;
    if len != data.len() {
        Err(BundleError::BlockAdditionalData(block.block_type))
    } else {
        Ok((v, s))
    }
}

impl Bundle {
    fn parse_bcb_payload<'a, T, F>(
        &self,
        block: &Block,
        bcb_keys: &mut HashMap<(&'a Eid, bpsec::Context), Option<bpsec::KeyMaterial>>,
        eid: &'a Eid,
        operation: &'a bpsec::bcb::Operation,
        data: &[u8],
        f: &mut F,
    ) -> Result<T, BundleError>
    where
        T: cbor::decode::FromCbor<Error: From<cbor::decode::Error>>,
        BundleError: From<<T as cbor::decode::FromCbor>::Error>,
        F: FnMut(&Eid) -> Result<Option<bpsec::KeyMaterial>, bpsec::Error>,
    {
        let data = match bcb_keys.get(&(eid, operation.context_id())) {
            Some(Some(key)) => operation.decrypt(key, self, &block.block_data(data)?)?,
            Some(None) => return Err(bpsec::Error::NoKeys(eid.clone()).into()),
            None => {
                let Some(key) = f(eid)? else {
                    return Err(bpsec::Error::NoKeys(eid.clone()).into());
                };
                let data = operation.decrypt(&key, self, &block.block_data(data)?)?;
                bcb_keys.insert((eid, operation.context_id()), Some(key));
                data
            }
        };

        let (v, s, len) = cbor::decode::parse(&data)?;
        if len != data.len() {
            Err(BundleError::BlockAdditionalData(block.block_type))
        } else if !s {
            Err(bpsec::Error::NotCanonical(block.block_type).into())
        } else {
            Ok(v)
        }
    }

    fn verify_bib_payload<'a, F>(
        &self,
        bib_keys: &mut HashMap<(&'a Eid, bpsec::Context), Option<bpsec::KeyMaterial>>,
        eid: &'a Eid,
        operation: &'a bpsec::bib::Operation,
        data: &[u8],
        f: &mut F,
    ) -> Result<(), bpsec::Error>
    where
        F: FnMut(&Eid) -> Result<Option<bpsec::KeyMaterial>, bpsec::Error>,
    {
        match bib_keys.get(&(eid, operation.context_id())) {
            Some(Some(key)) => operation.verify(key, self, data),
            Some(None) => Ok(()),
            None => {
                let key = f(eid)?;
                if let Some(key) = &key {
                    operation.verify(key, self, data)?;
                }
                bib_keys.insert((eid, operation.context_id()), key);
                Ok(())
            }
        }
    }

    fn parse_bib_payload<'a, T, F>(
        &self,
        block: &Block,
        bib_keys: &mut HashMap<(&'a Eid, bpsec::Context), Option<bpsec::KeyMaterial>>,
        eid: &'a Eid,
        operation: &'a bpsec::bib::Operation,
        data: &[u8],
        f: &mut F,
    ) -> Result<T, BundleError>
    where
        T: cbor::decode::FromCbor<Error: From<cbor::decode::Error>>,
        BundleError: From<<T as cbor::decode::FromCbor>::Error>,
        F: FnMut(&Eid) -> Result<Option<bpsec::KeyMaterial>, bpsec::Error>,
    {
        let data = block.block_data(data)?;
        self.verify_bib_payload(bib_keys, eid, operation, &data, f)?;

        let (v, s, len) = cbor::decode::parse(&data)?;
        if len != data.len() {
            Err(BundleError::BlockAdditionalData(block.block_type))
        } else if !s {
            Err(bpsec::Error::NotCanonical(block.block_type).into())
        } else {
            Ok(v)
        }
    }

    fn parse_blocks<F>(
        &mut self,
        blocks: &mut cbor::decode::Array,
        mut offset: usize,
        data: &[u8],
        mut f: F,
    ) -> Result<(HashSet<u64>, bool), BundleError>
    where
        F: FnMut(&Eid) -> Result<Option<bpsec::KeyMaterial>, bpsec::Error>,
    {
        let mut shortest = true;
        let mut last_block_number = 0;
        let mut noncanonical_blocks = HashSet::new();
        let mut bcbs = Vec::new();
        let mut blocks_to_check = HashMap::new();
        let mut bibs_to_check = HashSet::new();

        while let Some((mut block, s, block_len)) =
            blocks.try_parse::<(block::BlockWithNumber, bool, usize)>()?
        {
            shortest = shortest && s;
            block.block.data_start += offset;

            // Check the block
            match block.block.block_type {
                BlockType::Payload
                | BlockType::PreviousNode
                | BlockType::BundleAge
                | BlockType::HopCount => {
                    // Confirm no duplicates
                    if blocks_to_check
                        .insert(block.block.block_type, block.number)
                        .is_some()
                    {
                        return Err(BundleError::DuplicateBlocks(block.block.block_type));
                    }
                }
                BlockType::BlockIntegrity => {
                    bibs_to_check.insert(block.number);
                }
                BlockType::BlockSecurity => {
                    if !block.block.flags.delete_block_on_failure {
                        return Err(bpsec::Error::BCBDeleteFlag.into());
                    }

                    bcbs.push((
                        block.number,
                        parse_payload::<bpsec::bcb::OperationSet>(&block.block, data)
                            .map(|(v, s)| {
                                if !s {
                                    noncanonical_blocks.insert(block.number);
                                    shortest = false;
                                }
                                v
                            })
                            .map_field_err("BPSec confidentiality extension block")?,
                    ));
                }
                _ => {}
            }

            // Add block
            if self.blocks.insert(block.number, block.block).is_some() {
                return Err(BundleError::DuplicateBlockNumber(block.number));
            }

            last_block_number = block.number;
            offset += block_len;
        }

        // Check the last block is the payload
        if blocks_to_check.remove(&BlockType::Payload).is_none() {
            return Err(BundleError::MissingPayload);
        };
        let Some(BlockType::Payload) = self.blocks.get(&last_block_number).map(|b| b.block_type)
        else {
            return Err(BundleError::PayloadNotFinal);
        };

        // Check bundle age is correct
        if !blocks_to_check.contains_key(&BlockType::BundleAge)
            && self.id.timestamp.creation_time.is_none()
        {
            return Err(BundleError::MissingBundleAge);
        }

        // Check BCB targets first
        let mut bcb_keys = HashMap::new();
        let mut bcb_targets = HashSet::new();
        let mut bibs = Vec::new();
        for (block_number, bcb) in &bcbs {
            for (target, op) in &bcb.operations {
                let Some(block) = self.blocks.get(target) else {
                    return Err(bpsec::Error::MissingSecurityTarget.into());
                };

                // Check BCB rules
                match block.block_type {
                    BlockType::BlockSecurity | BlockType::Primary => {
                        return Err(bpsec::Error::InvalidBCBTarget.into())
                    }
                    BlockType::Payload => {
                        if !self.blocks.get(block_number).unwrap().flags.must_replicate {
                            return Err(bpsec::Error::BCBMustReplicate.into());
                        }
                    }
                    BlockType::PreviousNode => {
                        self.previous_node = Some(
                            self.parse_bcb_payload(
                                block,
                                &mut bcb_keys,
                                &bcb.source,
                                op,
                                data,
                                &mut f,
                            )
                            .map_field_err("Previous Node Block")?,
                        );
                        blocks_to_check.remove(&block.block_type);
                    }
                    BlockType::BundleAge => {
                        self.age = Some(
                            self.parse_bcb_payload(
                                block,
                                &mut bcb_keys,
                                &bcb.source,
                                op,
                                data,
                                &mut f,
                            )
                            .map_field_err("Bundle Age Block")?,
                        );
                        blocks_to_check.remove(&block.block_type);
                    }
                    BlockType::HopCount => {
                        self.hop_count = Some(
                            self.parse_bcb_payload(
                                block,
                                &mut bcb_keys,
                                &bcb.source,
                                op,
                                data,
                                &mut f,
                            )
                            .map_field_err("Hop Count Block")?,
                        );
                        blocks_to_check.remove(&block.block_type);
                    }
                    BlockType::BlockIntegrity => {
                        let bib: bpsec::bib::OperationSet = self
                            .parse_bcb_payload(block, &mut bcb_keys, &bcb.source, op, data, &mut f)
                            .map_field_err("BPSec integrity extension block")?;

                        // TODO - Check targets match!!

                        bibs_to_check.remove(target);
                        bibs.push(bib);
                    }
                    _ => {
                        // Confirm we can decrypt if we have keys
                        match bcb_keys.get(&(&bcb.source, op.context_id())) {
                            Some(Some(key)) => {
                                op.decrypt(key, self, &block.block_data(data)?)?;
                            }
                            Some(None) => {}
                            None => {
                                let key = f(&bcb.source)?;
                                if let Some(key) = &key {
                                    op.decrypt(key, self, &block.block_data(data)?)?;
                                }
                                bcb_keys.insert((&bcb.source, op.context_id()), key);
                            }
                        }
                    }
                }

                if !bcb_targets.insert(target) {
                    return Err(bpsec::Error::DuplicateOpTarget.into());
                }
            }
        }

        // Gather remaining BIBs
        for block_number in bibs_to_check {
            bibs.push(
                parse_payload::<bpsec::bib::OperationSet>(
                    self.blocks.get(&block_number).unwrap(),
                    data,
                )
                .map(|(v, s)| {
                    if !s {
                        noncanonical_blocks.insert(block_number);
                        shortest = false;
                    }
                    v
                })
                .map_field_err("BPSec integrity extension block")?,
            );
        }

        // Check BIB targets next
        let mut bib_keys = HashMap::new();
        let mut bib_targets = HashSet::new();
        for bib in &bibs {
            for (target, op) in &bib.operations {
                let Some(block) = self.blocks.get(target) else {
                    return Err(bpsec::Error::MissingSecurityTarget.into());
                };

                // Check BIB rules
                match block.block_type {
                    BlockType::Primary => {
                        // Perform an integrity check if we have keys
                        self.verify_bib_payload(
                            &mut bib_keys,
                            &bib.source,
                            op,
                            &self.write_primary_block(),
                            &mut f,
                        )?
                    }
                    BlockType::PreviousNode => {
                        self.previous_node = Some(
                            self.parse_bib_payload(
                                block,
                                &mut bib_keys,
                                &bib.source,
                                op,
                                data,
                                &mut f,
                            )
                            .map_field_err("Previous Node Block")?,
                        );
                        blocks_to_check.remove(&block.block_type);
                    }
                    BlockType::BundleAge => {
                        self.age = Some(
                            self.parse_bib_payload(
                                block,
                                &mut bib_keys,
                                &bib.source,
                                op,
                                data,
                                &mut f,
                            )
                            .map_field_err("Bundle Age Block")?,
                        );
                        blocks_to_check.remove(&block.block_type);
                    }
                    BlockType::HopCount => {
                        self.hop_count = Some(
                            self.parse_bib_payload(
                                block,
                                &mut bib_keys,
                                &bib.source,
                                op,
                                data,
                                &mut f,
                            )
                            .map_field_err("Hop Count Block")?,
                        );
                        blocks_to_check.remove(&block.block_type);
                    }
                    BlockType::BlockSecurity | BlockType::BlockIntegrity => {
                        return Err(bpsec::Error::InvalidBIBTarget.into())
                    }
                    _ => {
                        // Perform an integrity check if we have keys
                        self.verify_bib_payload(
                            &mut bib_keys,
                            &bib.source,
                            op,
                            &block.block_data(data)?,
                            &mut f,
                        )?
                    }
                }

                if !bib_targets.insert(target) {
                    return Err(bpsec::Error::DuplicateOpTarget.into());
                }
            }
        }

        for block_number in blocks_to_check.values() {
            let block = self.blocks.get(block_number).unwrap();
            match block.block_type {
                BlockType::PreviousNode => {
                    self.previous_node = parse_payload(block, data)
                        .map(|(v, s)| {
                            if !s {
                                noncanonical_blocks.insert(*block_number);
                                shortest = false;
                            }
                            Some(v)
                        })
                        .map_field_err("Previous Node Block")?;
                }
                BlockType::BundleAge => {
                    self.age = parse_payload(block, data)
                        .map(|(v, s)| {
                            if !s {
                                noncanonical_blocks.insert(*block_number);
                                shortest = false;
                            }
                            Some(v)
                        })
                        .map_field_err("Bundle Age Block")?;
                }
                BlockType::HopCount => {
                    self.hop_count = parse_payload(block, data)
                        .map(|(v, s)| {
                            if !s {
                                noncanonical_blocks.insert(*block_number);
                                shortest = false;
                            }
                            Some(v)
                        })
                        .map_field_err("Hop Count Block")?;
                }
                _ => {}
            }
        }
        Ok((noncanonical_blocks, shortest))
    }

    fn write_primary_block(&self) -> Vec<u8> {
        crc::append_crc_value(
            self.crc_type,
            cbor::encode::emit_array(
                Some(if let CrcType::None = self.crc_type {
                    8
                } else {
                    9
                }),
                |a, _| {
                    // Version
                    a.emit(7);
                    // Flags
                    a.emit(&self.flags);
                    // CRC
                    a.emit(self.crc_type);
                    // EIDs
                    a.emit(&self.destination);
                    a.emit(&self.id.source);
                    a.emit(&self.report_to);
                    // Timestamp
                    a.emit(&self.id.timestamp);
                    // Lifetime
                    a.emit(self.lifetime);
                    // CRC
                    if let CrcType::None = self.crc_type {
                    } else {
                        a.skip_value();
                    }
                },
            ),
        )
    }

    pub fn emit_primary_block(&mut self, offset: usize) -> Vec<u8> {
        let block_data = self.write_primary_block();
        self.blocks.insert(
            0,
            Block {
                block_type: BlockType::Primary,
                flags: BlockFlags {
                    must_replicate: true,
                    report_on_failure: true,
                    delete_bundle_on_failure: true,
                    ..Default::default()
                },
                crc_type: self.crc_type,
                data_start: offset,
                payload_offset: 0,
                data_len: block_data.len(),
            },
        );
        block_data
    }

    fn canonicalise(
        &mut self,
        mut noncanonical_blocks: HashSet<u64>,
        source_data: &[u8],
    ) -> Result<Box<[u8]>, BundleError> {
        // Begin indefinite array
        let mut data = vec![(4 << 5) | 31u8];

        // Emit primary block
        let block_data = self.emit_primary_block(data.len());
        data.extend(block_data);

        // Stash payload block for last
        let mut payload_block = self.blocks.remove(&1).expect("No payload block!");

        // Emit extension blocks
        for (block_number, block) in &mut self.blocks {
            if let BlockType::Primary | BlockType::Payload = block.block_type {
                continue;
            }
            let block_data = if noncanonical_blocks.remove(block_number) {
                match &block.block_type {
                    BlockType::PreviousNode => block.emit(
                        *block_number,
                        &cbor::encode::emit(self.previous_node.as_ref().unwrap()),
                        data.len(),
                    ),
                    BlockType::BundleAge => block.emit(
                        *block_number,
                        &cbor::encode::emit(self.age.unwrap()),
                        data.len(),
                    ),
                    BlockType::HopCount => block.emit(
                        *block_number,
                        &cbor::encode::emit(self.hop_count.as_ref().unwrap()),
                        data.len(),
                    ),
                    BlockType::BlockIntegrity => block.emit(
                        *block_number,
                        &cbor::encode::emit(
                            &parse_payload::<bpsec::bib::OperationSet>(block, source_data)
                                .map(|(v, _)| v)
                                .unwrap(),
                        ),
                        data.len(),
                    ),
                    BlockType::BlockSecurity => block.emit(
                        *block_number,
                        &cbor::encode::emit(
                            &parse_payload::<bpsec::bcb::OperationSet>(block, source_data)
                                .map(|(v, _)| v)
                                .unwrap(),
                        ),
                        data.len(),
                    ),
                    _ => unreachable!(),
                }
            } else {
                block.emit(*block_number, &block.block_data(source_data)?, data.len())
            };
            data.extend(block_data);
        }

        // Emit payload block
        let block_data = payload_block.emit(
            1,
            &cbor::encode::emit(payload_block.block_data(source_data)?.as_ref()),
            data.len(),
        );
        data.extend(block_data);
        self.blocks.insert(1, payload_block);

        // End indefinite array
        data.push(0xFF);

        Ok(data.into())
    }
}

// For parsing a bundle plus 'minimal viability'
#[derive(Debug)]
pub enum ValidBundle {
    Valid(Bundle),
    Canonicalised(Bundle, Box<[u8]>),
    Invalid(Bundle),
}

impl ValidBundle {
    pub fn parse<F>(data: &[u8], f: F) -> Result<Self, BundleError>
    where
        F: FnMut(&Eid) -> Result<Option<bpsec::KeyMaterial>, bpsec::Error>,
    {
        let (mut bundle, valid, canonical, noncanonical_blocks) =
            cbor::decode::parse_array(data, |blocks, mut canonical, tags| {
                // Check for shortest/correct form
                canonical = canonical && !blocks.is_definite();
                if canonical {
                    // Appendix B of RFC9171
                    let mut seen_55799 = false;
                    for tag in &tags {
                        match *tag {
                            255799 if !seen_55799 => seen_55799 = true,
                            _ => {
                                canonical = false;
                                break;
                            }
                        }
                    }
                }

                // Parse Primary block
                let block_start = blocks.offset();
                let ((mut bundle, mut valid), block_len) = blocks
                    .parse_array(|block, s, tags| {
                        canonical = canonical && s && tags.is_empty() && block.is_definite();

                        parse_primary_block(data, block, block_start).map(|(bundle, valid, s)| {
                            canonical = canonical && s;
                            (bundle, valid)
                        })
                    })
                    .map_field_err("Primary Block")?;

                let mut noncanonical_blocks = HashSet::new();
                if valid {
                    // Add a block 0
                    bundle.blocks.insert(
                        0,
                        Block {
                            block_type: BlockType::Primary,
                            flags: BlockFlags {
                                must_replicate: true,
                                report_on_failure: true,
                                delete_bundle_on_failure: true,
                                ..Default::default()
                            },
                            crc_type: bundle.crc_type,
                            data_start: block_start,
                            payload_offset: 0,
                            data_len: block_len,
                        },
                    );

                    let r = bundle.parse_blocks(blocks, block_start + block_len, data, f);
                    if let Ok((ncb, s)) = r {
                        canonical = canonical && s;
                        noncanonical_blocks = ncb;
                    } else {
                        // Don't return an Err, just capture validity
                        #[cfg(test)]
                        dbg!(&r);

                        valid = false;
                    }
                }

                if !valid {
                    // Just skip over the blocks, avoiding any further parsing
                    blocks.skip_to_end(16)?;
                }
                Ok::<_, BundleError>((bundle, valid, canonical, noncanonical_blocks))
            })
            .map(|((bundle, valid, canonical, noncanonical_blocks), len)| {
                (
                    bundle,
                    valid && len == data.len(),
                    canonical,
                    noncanonical_blocks,
                )
            })?;
        if !valid {
            Ok(Self::Invalid(bundle))
        } else if !canonical {
            let data = bundle.canonicalise(noncanonical_blocks, data)?;
            Ok(Self::Canonicalised(bundle, data))
        } else {
            Ok(Self::Valid(bundle))
        }
    }
}
