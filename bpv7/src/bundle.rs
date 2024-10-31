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

    #[error("Invalid bundle flag combination")]
    InvalidFlags,

    #[error("Invalid bundle: {error}")]
    InvalidBundle {
        bundle: Box<Bundle>,
        error: Box<dyn std::error::Error + Send + Sync>,
    },

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
    ) -> Result<HashSet<u64>, BundleError>
    where
        F: FnMut(&Eid) -> Result<Option<bpsec::KeyMaterial>, bpsec::Error>,
    {
        let mut last_block_number = 0;
        let mut noncanonical_blocks = HashSet::new();
        let mut blocks_to_check = HashMap::new();
        let mut bcbs = Vec::new();
        let mut bibs_to_check = HashSet::new();

        while let Some((mut block, s, block_len)) =
            blocks.try_parse::<(block::BlockWithNumber, bool, usize)>()?
        {
            if !s {
                noncanonical_blocks.insert(block.number);
            }
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
                        block
                            .block
                            .parse_payload::<bpsec::bcb::OperationSet>(data)
                            .map(|(v, s)| {
                                if !s {
                                    noncanonical_blocks.insert(block.number);
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

                        // Check targets match!!
                        for t in bib.operations.keys() {
                            if !bcb.operations.contains_key(t) {
                                return Err(bpsec::Error::BCBMustShareTarget.into());
                            }
                        }

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
                self.blocks
                    .get(&block_number)
                    .unwrap()
                    .parse_payload::<bpsec::bib::OperationSet>(data)
                    .map(|(v, s)| {
                        if !s {
                            noncanonical_blocks.insert(block_number);
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
                            &primary_block::PrimaryBlock::emit(self),
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
                    self.previous_node = block
                        .parse_payload(data)
                        .map(|(v, s)| {
                            if !s {
                                noncanonical_blocks.insert(*block_number);
                            }
                            Some(v)
                        })
                        .map_field_err("Previous Node Block")?;
                }
                BlockType::BundleAge => {
                    self.age = block
                        .parse_payload(data)
                        .map(|(v, s)| {
                            if !s {
                                noncanonical_blocks.insert(*block_number);
                            }
                            Some(v)
                        })
                        .map_field_err("Bundle Age Block")?;
                }
                BlockType::HopCount => {
                    self.hop_count = block
                        .parse_payload(data)
                        .map(|(v, s)| {
                            if !s {
                                noncanonical_blocks.insert(*block_number);
                            }
                            Some(v)
                        })
                        .map_field_err("Hop Count Block")?;
                }
                _ => {}
            }
        }
        Ok(noncanonical_blocks)
    }

    pub fn emit_primary_block(&mut self, array: &mut cbor::encode::Array, offset: usize) -> usize {
        let len = array.emit_raw(&primary_block::PrimaryBlock::emit(self));
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
                data_len: len,
            },
        );
        len
    }

    fn canonicalise(
        &mut self,
        mut noncanonical_blocks: HashSet<u64>,
        source_data: &[u8],
    ) -> Vec<u8> {
        cbor::encode::emit_array(None, |a, mut offset| {
            // Emit primary block
            if noncanonical_blocks.remove(&0) {
                offset += self.emit_primary_block(a, offset);
            } else {
                offset += self
                    .blocks
                    .get_mut(&0)
                    .expect("Missing primary block!")
                    .copy(source_data, a, offset);
            }

            // Stash payload block for last
            let mut payload_block = self.blocks.remove(&1).expect("No payload block!");

            // Emit extension blocks
            for (block_number, block) in &mut self.blocks {
                if let BlockType::Primary | BlockType::Payload = block.block_type {
                    continue;
                }
                if noncanonical_blocks.remove(block_number) {
                    offset += match &block.block_type {
                        BlockType::PreviousNode => block.emit(
                            *block_number,
                            &cbor::encode::emit(self.previous_node.as_ref().unwrap()),
                            a,
                            offset,
                        ),
                        BlockType::BundleAge => block.emit(
                            *block_number,
                            &cbor::encode::emit(self.age.unwrap()),
                            a,
                            offset,
                        ),
                        BlockType::HopCount => block.emit(
                            *block_number,
                            &cbor::encode::emit(self.hop_count.as_ref().unwrap()),
                            a,
                            offset,
                        ),
                        BlockType::BlockIntegrity => block.emit(
                            *block_number,
                            &cbor::encode::emit(
                                &block
                                    .parse_payload::<bpsec::bib::OperationSet>(source_data)
                                    .unwrap()
                                    .0,
                            ),
                            a,
                            offset,
                        ),
                        BlockType::BlockSecurity => block.emit(
                            *block_number,
                            &cbor::encode::emit(
                                &block
                                    .parse_payload::<bpsec::bcb::OperationSet>(source_data)
                                    .unwrap()
                                    .0,
                            ),
                            a,
                            offset,
                        ),
                        _ => block.emit(
                            *block_number,
                            &block.block_data(source_data).unwrap(),
                            a,
                            offset,
                        ),
                    };
                } else {
                    offset += block.copy(source_data, a, offset);
                }
            }

            // Emit payload block
            payload_block.emit(
                1,
                &cbor::encode::emit(payload_block.block_data(source_data).unwrap().as_ref()),
                a,
                offset,
            );
            self.blocks.insert(1, payload_block);
        })
    }
}

// For parsing a bundle plus 'minimal viability'
#[derive(Debug)]
pub enum ValidBundle {
    Valid(Bundle),
    Rewritten(Bundle, Box<[u8]>),
    Invalid(Bundle, Box<dyn std::error::Error + Send + Sync>),
}

impl ValidBundle {
    pub fn parse<F>(data: &[u8], f: F) -> Result<Self, BundleError>
    where
        F: FnMut(&Eid) -> Result<Option<bpsec::KeyMaterial>, bpsec::Error>,
    {
        match cbor::decode::parse_array(data, |blocks, mut canonical, tags| {
            let mut noncanonical_blocks = HashSet::new();

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
            let (mut bundle, block_len) = blocks
                .parse::<(primary_block::PrimaryBlock, bool, usize)>()
                .map(|(v, s, len)| {
                    canonical = canonical && s;
                    (v.into_bundle(), len)
                })
                .map_field_err("Primary Block")?;

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
                    payload_offset: block_len,
                    data_len: block_len,
                },
            );

            if !canonical {
                noncanonical_blocks.insert(0);
            }

            match bundle.parse_blocks(blocks, block_start + block_len, data, f) {
                Ok(ncb) => {
                    noncanonical_blocks.extend(ncb);
                    Ok((bundle, noncanonical_blocks))
                }
                Err(e) => Err(BundleError::InvalidBundle {
                    bundle: bundle.into(),
                    error: e.into(),
                }),
            }
        }) {
            Ok(((mut bundle, noncanonical_blocks), len)) => {
                if len != data.len() {
                    Ok(Self::Invalid(bundle, BundleError::AdditionalData.into()))
                } else if !noncanonical_blocks.is_empty() {
                    let data = bundle.canonicalise(noncanonical_blocks, data);
                    Ok(Self::Rewritten(bundle, data.into()))
                } else {
                    Ok(Self::Valid(bundle))
                }
            }
            Err(BundleError::InvalidBundle { bundle, error: e }) => Ok(Self::Invalid(*bundle, e)),
            Err(e) => Err(e),
        }
    }
}
