use super::*;
use std::collections::{HashMap, HashSet};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum BundleError {
    #[error("Bundle has additional data after end of CBOR array")]
    AdditionalData,

    #[error("Unsupported bundle protocol version {0}")]
    InvalidVersion(u64),

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

    #[error("Bundle source has no clock, and there is no Bundle Age extension block")]
    MissingBundleAge,

    #[error("Unsupported block type or block content sub-type")]
    Unsupported,

    #[error("Invalid bundle flag combination")]
    InvalidFlags,

    #[error("Invalid bundle: {error}")]
    InvalidBundle {
        bundle: Box<Bundle>,
        reason: StatusReportReasonCode,
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

struct KeyCache<F>
where
    F: FnMut(&Eid) -> Result<Option<bpsec::KeyMaterial>, bpsec::Error>,
{
    keys: HashMap<Eid, HashMap<bpsec::Context, Option<bpsec::KeyMaterial>>>,
    f: F,
}

impl<F> KeyCache<F>
where
    F: FnMut(&Eid) -> Result<Option<bpsec::KeyMaterial>, bpsec::Error>,
{
    pub fn new(f: F) -> Self {
        Self {
            keys: HashMap::new(),
            f,
        }
    }

    pub fn get(
        &mut self,
        source: &Eid,
        context: bpsec::Context,
    ) -> Result<Option<bpsec::KeyMaterial>, bpsec::Error> {
        if let Some(inner) = self.keys.get_mut(source) {
            if let Some(material) = inner.get(&context) {
                return Ok(material.clone());
            }
            let material = (self.f)(source)?;
            inner.insert(context, material.clone());
            Ok(material)
        } else {
            let material = (self.f)(source)?;
            self.keys
                .insert(source.clone(), HashMap::from([(context, material.clone())]));
            Ok(material)
        }
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
    fn bcb_decrypt_block<F>(
        &self,
        block: &Block,
        keys: &mut KeyCache<F>,
        eid: &Eid,
        operation: &bpsec::bcb::Operation,
        data: &[u8],
    ) -> Result<Option<Box<[u8]>>, bpsec::Error>
    where
        F: FnMut(&Eid) -> Result<Option<bpsec::KeyMaterial>, bpsec::Error>,
    {
        match keys.get(eid, operation.context_id())? {
            Some(key) => operation
                .decrypt(&key, self, &block.block_data(data)?)
                .map(Some),
            None => Ok(None),
        }
    }

    fn bib_verify_data<F>(
        &self,
        keys: &mut KeyCache<F>,
        eid: &Eid,
        operation: &bpsec::bib::Operation,
        data: &[u8],
    ) -> Result<(), bpsec::Error>
    where
        F: FnMut(&Eid) -> Result<Option<bpsec::KeyMaterial>, bpsec::Error>,
    {
        match keys.get(eid, operation.context_id())? {
            Some(key) => operation.verify(&key, self, data),
            None => Ok(()),
        }
    }

    fn parse_blocks<F>(
        &mut self,
        blocks: &mut cbor::decode::Array,
        mut offset: usize,
        data: &[u8],
        f: F,
    ) -> Result<(HashSet<u64>, bool), BundleError>
    where
        F: FnMut(&Eid) -> Result<Option<bpsec::KeyMaterial>, bpsec::Error>,
    {
        let mut last_block_number = 0;
        let mut noncanonical_blocks = HashSet::new();
        let mut blocks_to_check = HashMap::new();
        let mut blocks_to_remove = HashSet::new();
        let mut report_unsupported = false;
        let mut bcbs_to_check = Vec::new();
        let mut bibs_to_check = HashSet::new();
        let mut bcb_targets = HashSet::new();

        // Parse the blocks and build a map
        while let Some((mut block, s, block_len)) =
            blocks.try_parse::<(block::BlockWithNumber, bool, usize)>()?
        {
            if !s {
                noncanonical_blocks.insert(block.number);
            }
            block.block.data_start += offset;

            // Check the block
            match block.block.block_type {
                BlockType::Primary => unreachable!(),
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
                    if block.block.flags.delete_block_on_failure {
                        return Err(bpsec::Error::BCBDeleteFlag.into());
                    }

                    let bcb = block
                        .block
                        .parse_payload::<bpsec::bcb::OperationSet>(data)
                        .map(|(v, s)| {
                            if !s {
                                noncanonical_blocks.insert(block.number);
                            }
                            v
                        })
                        .map_field_err("BPSec confidentiality extension block")?;

                    if bcb.is_unsupported() {
                        if block.block.flags.delete_bundle_on_failure {
                            return Err(BundleError::Unsupported);
                        }

                        if block.block.flags.report_on_failure {
                            report_unsupported = true;
                        }
                    }
                    bcbs_to_check.push((block.number, bcb));
                }
                BlockType::Unrecognised(_) => {
                    if block.block.flags.delete_bundle_on_failure {
                        return Err(BundleError::Unsupported);
                    }

                    if block.block.flags.delete_block_on_failure {
                        blocks_to_remove.insert(block.number);
                    }

                    if block.block.flags.report_on_failure {
                        report_unsupported = true;
                    }
                }
            }

            // Add block
            if self.blocks.insert(block.number, block.block).is_some() {
                return Err(BundleError::DuplicateBlockNumber(block.number));
            }

            last_block_number = block.number;
            offset += block_len;
        }

        // Check the last block is the payload
        match blocks_to_check.remove(&BlockType::Payload) {
            None => return Err(BundleError::MissingPayload),
            Some(block_number) => {
                if block_number != last_block_number {
                    return Err(BundleError::PayloadNotFinal);
                }
            }
        }

        // Do the first BCB pass, checking BIBs and general sanity
        let keys = &mut KeyCache::new(f);
        let mut bcbs = Vec::new();
        let mut bib_targets = HashSet::new();
        for (block_number, bcb) in &bcbs_to_check {
            for (bcb_target, bcb_op) in &bcb.operations {
                if !bcb_targets.insert(bcb_target) {
                    return Err(bpsec::Error::DuplicateOpTarget.into());
                }
                let Some(bcb_target_block) = self.blocks.get(bcb_target) else {
                    return Err(bpsec::Error::MissingSecurityTarget.into());
                };

                let mut add_target = !bcb_op.is_unsupported();
                match bcb_target_block.block_type {
                    BlockType::BlockSecurity | BlockType::Primary => {
                        return Err(bpsec::Error::InvalidBCBTarget.into())
                    }
                    BlockType::Payload => {
                        // Just validate
                        if !self.blocks.get(block_number).unwrap().flags.must_replicate {
                            return Err(bpsec::Error::BCBMustReplicate.into());
                        }
                    }
                    BlockType::BlockIntegrity => {
                        if !bcb_op.is_unsupported() {
                            let Some(bcb_data) = self.bcb_decrypt_block(
                                bcb_target_block,
                                keys,
                                &bcb.source,
                                bcb_op,
                                data,
                            )?
                            else {
                                return Err(bpsec::Error::NoKeys(bcb.source.clone()).into());
                            };

                            let (bib, s) =
                                cbor::decode::parse::<(bpsec::bib::OperationSet, bool, usize)>(
                                    &bcb_data,
                                )
                                .map(|(v, s, len)| (v, s && len == bcb_data.len()))
                                .map_field_err("BPSec integrity extension block")?;
                            if !s {
                                return Err(
                                    bpsec::Error::NotCanonical(BlockType::BlockIntegrity).into()
                                );
                            }

                            if bib.is_unsupported() {
                                if bcb_target_block.flags.delete_bundle_on_failure {
                                    return Err(BundleError::Unsupported);
                                }

                                if bcb_target_block.flags.delete_block_on_failure {
                                    return Err(bpsec::Error::InvalidTargetFlags.into());
                                }

                                if bcb_target_block.flags.report_on_failure {
                                    report_unsupported = true;
                                }
                            }
                            // Validate targets, as they are encrypted by this BCB
                            for (bib_target, bib_op) in bib.operations {
                                // Check targets match
                                if !bcb.operations.contains_key(&bib_target) {
                                    return Err(bpsec::Error::BCBMustShareTarget.into());
                                }
                                if !bib_targets.insert(bib_target) {
                                    return Err(bpsec::Error::DuplicateOpTarget.into());
                                }

                                let Some(bib_target_block) = self.blocks.get(&bib_target) else {
                                    return Err(bpsec::Error::MissingSecurityTarget.into());
                                };

                                // Check BIB target
                                if let BlockType::BlockSecurity | BlockType::BlockIntegrity =
                                    bib_target_block.block_type
                                {
                                    return Err(bpsec::Error::InvalidBIBTarget.into());
                                }

                                // Decrypt the BIB target
                                if let Some(data) = self.bcb_decrypt_block(
                                    bib_target_block,
                                    keys,
                                    &bcb.source,
                                    bcb_op,
                                    data,
                                )? {
                                    if !bib_op.is_unsupported() {
                                        // Do BIB verification
                                        self.bib_verify_data(keys, &bib.source, &bib_op, &data)?;
                                    }

                                    if !match bib_target_block.block_type {
                                        BlockType::PreviousNode => {
                                            let (eid, s, len) = cbor::decode::parse(&data)
                                                .map_field_err("Previous Node Block")?;
                                            self.previous_node = Some(eid);
                                            s && len == data.len()
                                        }
                                        BlockType::BundleAge => {
                                            let (age, s, len) = cbor::decode::parse(&data)
                                                .map_field_err("Bundle Age Block")?;
                                            self.age = Some(age);
                                            s && len == data.len()
                                        }
                                        BlockType::HopCount => {
                                            let (hop_count, s, len) = cbor::decode::parse(&data)
                                                .map_field_err("Hop Count Block")?;
                                            self.hop_count = Some(hop_count);
                                            s && len == data.len()
                                        }
                                        _ => true,
                                    } {
                                        return Err(bpsec::Error::NotCanonical(
                                            bib_target_block.block_type,
                                        )
                                        .into());
                                    }
                                    blocks_to_check.remove(&bib_target_block.block_type);
                                }
                            }
                        }

                        // Don't need to check this BIB again
                        bibs_to_check.remove(bcb_target);

                        // Don't need to reprocess this BCB target
                        add_target = false;
                    }
                    _ => {}
                }

                if add_target {
                    bcbs.push((*bcb_target, bcb_target_block, &bcb.source, bcb_op));
                }
            }
        }
        drop(bcb_targets);

        // Check non-BIB BCB targets next
        for (target_block_number, target_block, source, op) in bcbs {
            // Skip blocks we have already processed as BIB targets
            if bib_targets.contains(&target_block_number) {
                continue;
            }

            // Confirm we can decrypt if we have keys
            if let Some(data) = self.bcb_decrypt_block(target_block, keys, source, op, data)? {
                if !match target_block.block_type {
                    BlockType::PreviousNode => {
                        let (eid, s, len) =
                            cbor::decode::parse(&data).map_field_err("Previous Node Block")?;
                        self.previous_node = Some(eid);
                        s && len == data.len()
                    }
                    BlockType::BundleAge => {
                        let (age, s, len) =
                            cbor::decode::parse(&data).map_field_err("Bundle Age Block")?;
                        self.age = Some(age);
                        s && len == data.len()
                    }
                    BlockType::HopCount => {
                        let (hop_count, s, len) =
                            cbor::decode::parse(&data).map_field_err("Hop Count Block")?;
                        self.hop_count = Some(hop_count);
                        s && len == data.len()
                    }
                    _ => true,
                } {
                    return Err(bpsec::Error::NotCanonical(target_block.block_type).into());
                }
                blocks_to_check.remove(&target_block.block_type);
            }
        }
        drop(bcbs_to_check);

        // Check remaining BIB targets next
        for block_number in bibs_to_check {
            let bib_block = self.blocks.get(&block_number).unwrap();

            let bib = bib_block
                .parse_payload::<bpsec::bib::OperationSet>(data)
                .map(|(v, s)| {
                    if !s {
                        noncanonical_blocks.insert(block_number);
                    }
                    v
                })
                .map_field_err("BPSec integrity extension block")?;

            if bib.is_unsupported() {
                if bib_block.flags.delete_bundle_on_failure {
                    return Err(BundleError::Unsupported);
                }

                if bib_block.flags.delete_block_on_failure {
                    // TODO: This requires a rewrite of the BIB
                    //blocks_to_remove.insert(block_number);
                }

                if bib_block.flags.report_on_failure {
                    report_unsupported = true;
                }
            }

            for (bib_target, bib_op) in bib.operations {
                if !bib_targets.insert(bib_target) {
                    return Err(bpsec::Error::DuplicateOpTarget.into());
                }
                let Some(bib_target_block) = self.blocks.get(&bib_target) else {
                    return Err(bpsec::Error::MissingSecurityTarget.into());
                };
                if let BlockType::BlockSecurity | BlockType::BlockIntegrity =
                    bib_target_block.block_type
                {
                    return Err(bpsec::Error::InvalidBIBTarget.into());
                }

                // Check BIB target
                let data = if let BlockType::Primary = bib_target_block.block_type {
                    primary_block::PrimaryBlock::emit(self).into()
                } else {
                    bib_target_block.block_data(data)?
                };

                if !bib_op.is_unsupported() {
                    self.bib_verify_data(keys, &bib.source, &bib_op, &data)?;
                }

                if !match bib_target_block.block_type {
                    BlockType::PreviousNode => {
                        let (eid, s, len) =
                            cbor::decode::parse(&data).map_field_err("Previous Node Block")?;
                        self.previous_node = Some(eid);
                        s && len == data.len()
                    }
                    BlockType::BundleAge => {
                        let (age, s, len) =
                            cbor::decode::parse(&data).map_field_err("Bundle Age Block")?;
                        self.age = Some(age);
                        s && len == data.len()
                    }
                    BlockType::HopCount => {
                        let (hop_count, s, len) =
                            cbor::decode::parse(&data).map_field_err("Hop Count Block")?;
                        self.hop_count = Some(hop_count);
                        s && len == data.len()
                    }
                    _ => true,
                } {
                    return Err(bpsec::Error::NotCanonical(bib_target_block.block_type).into());
                }
                blocks_to_check.remove(&bib_target_block.block_type);
            }
        }
        drop(bib_targets);

        for block_number in blocks_to_check.values() {
            let block = self.blocks.get(block_number).unwrap();
            let data = block.block_data(data)?;
            if !match block.block_type {
                BlockType::PreviousNode => {
                    let (eid, s, len) =
                        cbor::decode::parse(&data).map_field_err("Previous Node Block")?;
                    self.previous_node = Some(eid);
                    s && len == data.len()
                }
                BlockType::BundleAge => {
                    let (age, s, len) =
                        cbor::decode::parse(&data).map_field_err("Bundle Age Block")?;
                    self.age = Some(age);
                    s && len == data.len()
                }
                BlockType::HopCount => {
                    let (hop_count, s, len) =
                        cbor::decode::parse(&data).map_field_err("Hop Count Block")?;
                    self.hop_count = Some(hop_count);
                    s && len == data.len()
                }
                _ => true,
            } {
                noncanonical_blocks.insert(*block_number);
            }
        }

        // Check bundle age exists if needed
        if self.age.is_none() && self.id.timestamp.creation_time.is_none() {
            return Err(BundleError::MissingBundleAge);
        }
        Ok((noncanonical_blocks, report_unsupported))
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
    Valid(Bundle, bool),
    Rewritten(Bundle, Box<[u8]>, bool),
    Invalid(
        Bundle,
        StatusReportReasonCode,
        Box<dyn std::error::Error + Send + Sync>,
    ),
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
                Ok((ncb, report_unsupported)) => {
                    noncanonical_blocks.extend(ncb);
                    Ok((bundle, noncanonical_blocks, report_unsupported))
                }
                Err(BundleError::Unsupported) => Err(BundleError::InvalidBundle {
                    bundle: bundle.into(),
                    reason: StatusReportReasonCode::BlockUnsupported,
                    error: BundleError::Unsupported.into(),
                }),
                Err(e) => Err(BundleError::InvalidBundle {
                    bundle: bundle.into(),
                    reason: StatusReportReasonCode::BlockUnintelligible,
                    error: e.into(),
                }),
            }
        }) {
            Ok(((mut bundle, noncanonical_blocks, report_unsupported), len)) => {
                if len != data.len() {
                    Ok(Self::Invalid(
                        bundle,
                        StatusReportReasonCode::BlockUnintelligible,
                        BundleError::AdditionalData.into(),
                    ))
                } else if !noncanonical_blocks.is_empty() {
                    let data = bundle.canonicalise(noncanonical_blocks, data);
                    Ok(Self::Rewritten(bundle, data.into(), report_unsupported))
                } else {
                    Ok(Self::Valid(bundle, report_unsupported))
                }
            }
            Err(BundleError::InvalidBundle {
                bundle,
                reason,
                error: e,
            }) => Ok(Self::Invalid(*bundle, reason, e)),
            Err(e) => Err(e),
        }
    }
}
