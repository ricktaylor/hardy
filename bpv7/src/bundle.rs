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

    #[error("Primary block is not protected by a BPSec BIB or a CRC")]
    MissingIntegrityCheck,

    #[error("Bundle payload block must be block number 1")]
    InvalidPayloadBlockNumber,

    #[error("Final block of bundle is not a payload block")]
    PayloadNotFinal,

    #[error("Bundle has more than one block with block number {0}")]
    DuplicateBlockNumber(u64),

    #[error("{1} block cannot be block number {0}")]
    InvalidBlockNumber(u64, BlockType),

    #[error("Bundle has multiple {0} blocks")]
    DuplicateBlocks(BlockType),

    #[error("Bundle source has no clock, and there is no Bundle Age extension block")]
    MissingBundleAge,

    #[error("Block {0} has an unsupported block type or block content sub-type")]
    Unsupported(u64),

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
    F: FnMut(&Eid, bpsec::Context) -> Result<Option<bpsec::KeyMaterial>, bpsec::Error>,
{
    keys: HashMap<Eid, HashMap<bpsec::Context, Option<bpsec::KeyMaterial>>>,
    f: F,
}

impl<F> KeyCache<F>
where
    F: FnMut(&Eid, bpsec::Context) -> Result<Option<bpsec::KeyMaterial>, bpsec::Error>,
{
    pub fn new(f: F) -> Self {
        Self {
            keys: HashMap::new(),
            f,
        }
    }

    pub fn get<'a>(
        &'a mut self,
        source: &Eid,
        context: bpsec::Context,
    ) -> Result<Option<&'a bpsec::KeyMaterial>, bpsec::Error> {
        if !self.keys.contains_key(source) {
            self.keys.insert(source.clone(), HashMap::new());
        }
        let inner = self.keys.get_mut(source).unwrap();
        if let std::collections::hash_map::Entry::Vacant(e) = inner.entry(context) {
            e.insert((self.f)(source, context)?);
        }
        Ok(inner.get(&context).unwrap().as_ref())
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
    #[allow(clippy::type_complexity)]
    fn bcb_decrypt_block<F>(
        &self,
        keys: &mut KeyCache<F>,
        operation: &bpsec::bcb::Operation,
        args: bpsec::OperationArgs,
    ) -> Result<Option<(Box<[u8]>, bool)>, bpsec::Error>
    where
        F: FnMut(&Eid, bpsec::Context) -> Result<Option<bpsec::KeyMaterial>, bpsec::Error>,
    {
        if let Some(key) = keys.get(args.bpsec_source, operation.context_id())? {
            operation.decrypt(key, args)
        } else {
            Ok(None)
        }
    }

    fn bib_verify_data<F>(
        &self,
        keys: &mut KeyCache<F>,
        operation: &bpsec::bib::Operation,
        args: bpsec::OperationArgs,
        payload_data: Option<&[u8]>,
    ) -> Result<bool, bpsec::Error>
    where
        F: FnMut(&Eid, bpsec::Context) -> Result<Option<bpsec::KeyMaterial>, bpsec::Error>,
    {
        if let Some(key) = keys.get(args.bpsec_source, operation.context_id())? {
            operation.verify(key, args, payload_data)
        } else {
            Ok(false)
        }
    }

    #[allow(clippy::type_complexity)]
    fn parse_blocks<F>(
        &mut self,
        canonical_primary_block: bool,
        blocks: &mut cbor::decode::Array,
        mut offset: usize,
        source_data: &[u8],
        f: F,
    ) -> Result<(HashMap<u64, bool>, HashSet<u64>, bool), BundleError>
    where
        F: FnMut(&Eid, bpsec::Context) -> Result<Option<bpsec::KeyMaterial>, bpsec::Error>,
    {
        let mut last_block_number = 0;
        let mut noncanonical_blocks = HashMap::new();
        let mut blocks_to_check = HashMap::new();
        let mut blocks_to_remove = HashSet::new();
        let mut report_unsupported = false;
        let mut bcbs_to_check = Vec::new();
        let mut bibs_to_check = HashSet::new();
        let mut bcb_targets = HashSet::new();
        let mut protects_primary_block = HashSet::new();

        // Parse the blocks and build a map
        while let Some((mut block, s, block_len)) =
            blocks.try_parse::<(block::BlockWithNumber, bool, usize)>()?
        {
            if !s {
                noncanonical_blocks.insert(block.number, false);
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
                        .parse_payload::<bpsec::bcb::OperationSet>(source_data)
                        .map(|(v, s)| {
                            if !s {
                                noncanonical_blocks.insert(block.number, false);
                            }
                            v
                        })
                        .map_field_err("BPSec confidentiality extension block")?;

                    if bcb.is_unsupported() {
                        if block.block.flags.delete_bundle_on_failure {
                            return Err(BundleError::Unsupported(block.number));
                        }

                        if block.block.flags.report_on_failure {
                            report_unsupported = true;
                        }
                    }
                    bcbs_to_check.push((block.number, bcb));
                }
                BlockType::Unrecognised(_) => {
                    if block.block.flags.delete_bundle_on_failure {
                        return Err(BundleError::Unsupported(block.number));
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
        if let Some(payload_block_number) = blocks_to_check.remove(&BlockType::Payload) {
            if payload_block_number != last_block_number {
                return Err(BundleError::PayloadNotFinal);
            }
        } else {
            return Err(BundleError::MissingPayload);
        }

        // Do the first BCB pass, checking BIBs and general sanity
        let keys = &mut KeyCache::new(f);
        let mut bcbs = Vec::new();
        let mut bib_targets = HashSet::new();
        let mut bcb_target_counts = HashMap::new();
        for (bcb_block_number, bcb) in &bcbs_to_check {
            let bcb_block = self.blocks.get(bcb_block_number).unwrap();
            let mut bcb_targets_remaining = bcb.operations.len();
            for (bcb_target_number, bcb_op) in &bcb.operations {
                if !bcb_targets.insert(bcb_target_number) {
                    return Err(bpsec::Error::DuplicateOpTarget.into());
                }
                let Some(bcb_target_block) = self.blocks.get(bcb_target_number) else {
                    return Err(bpsec::Error::MissingSecurityTarget.into());
                };

                // Don't fully canonicalise BCB targets
                if let Some(is_bpsec_target) = noncanonical_blocks.get_mut(bcb_target_number) {
                    *is_bpsec_target = true;
                }

                let mut add_target = !bcb_op.is_unsupported();
                match bcb_target_block.block_type {
                    BlockType::BlockSecurity | BlockType::Primary => {
                        return Err(bpsec::Error::InvalidBCBTarget.into())
                    }
                    BlockType::Payload => {
                        // Just validate
                        if !bcb_block.flags.must_replicate {
                            return Err(bpsec::Error::BCBMustReplicate.into());
                        }
                    }
                    BlockType::BlockIntegrity => {
                        if let Some((bib, p)) = self.bcb_decrypt_block(
                            keys,
                            bcb_op,
                            bpsec::OperationArgs {
                                bpsec_source: &bcb.source,
                                target: bcb_target_block,
                                target_number: bcb_target_number,
                                source: bcb_block,
                                source_number: bcb_block_number,
                                bundle: self,
                                canonical_primary_block,
                                bundle_data: source_data,
                            },
                        )? {
                            if p {
                                protects_primary_block.insert(*bcb_block_number);
                            }
                            let bib = cbor::decode::parse::<bpsec::bib::OperationSet>(&bib)
                                .map_field_err("BPSec integrity extension block")?;

                            if bib.is_unsupported() {
                                if bcb_target_block.flags.delete_bundle_on_failure {
                                    return Err(BundleError::Unsupported(*bcb_target_number));
                                }

                                if bcb_target_block.flags.delete_block_on_failure {
                                    blocks_to_remove.insert(*bcb_target_number);
                                }

                                if bcb_target_block.flags.report_on_failure {
                                    report_unsupported = true;
                                }
                            }

                            // Validate targets now, as they are encrypted by this BCB
                            let mut bib_targets_remaining = bib.operations.len();
                            for (bib_target_number, bib_op) in bib.operations {
                                if !bib_targets.insert(bib_target_number) {
                                    return Err(bpsec::Error::DuplicateOpTarget.into());
                                }

                                let Some(bib_target_block) = self.blocks.get(&bib_target_number)
                                else {
                                    return Err(bpsec::Error::MissingSecurityTarget.into());
                                };

                                // Check BIB target
                                if let BlockType::BlockSecurity | BlockType::BlockIntegrity =
                                    bib_target_block.block_type
                                {
                                    return Err(bpsec::Error::InvalidBIBTarget.into());
                                }

                                // Find correct bcb_op for target
                                let Some(bcb_op) = bcb.operations.get(&bib_target_number) else {
                                    return Err(bpsec::Error::BCBMustShareTarget.into());
                                };

                                // Decrypt the BIB target
                                if let Some((block_data, p)) = self.bcb_decrypt_block(
                                    keys,
                                    bcb_op,
                                    bpsec::OperationArgs {
                                        bpsec_source: &bcb.source,
                                        target: bib_target_block,
                                        target_number: &bib_target_number,
                                        source: bcb_block,
                                        source_number: bcb_block_number,
                                        bundle: self,
                                        canonical_primary_block,
                                        bundle_data: source_data,
                                    },
                                )? {
                                    if p {
                                        protects_primary_block.insert(*bcb_block_number);
                                    }

                                    // Do BIB verification
                                    if self.bib_verify_data(
                                        keys,
                                        &bib_op,
                                        bpsec::OperationArgs {
                                            bpsec_source: &bib.source,
                                            target: bib_target_block,
                                            target_number: &bib_target_number,
                                            source: bcb_target_block,
                                            source_number: bcb_target_number,
                                            bundle: self,
                                            canonical_primary_block,
                                            bundle_data: source_data,
                                        },
                                        Some(&block_data),
                                    )? {
                                        protects_primary_block.insert(*bcb_target_number);
                                    }

                                    // And parse
                                    match bib_target_block.block_type {
                                        BlockType::PreviousNode => {
                                            self.previous_node = Some(
                                                cbor::decode::parse(&block_data)
                                                    .map_field_err("Previous Node Block")?,
                                            );
                                        }
                                        BlockType::BundleAge => {
                                            self.age = Some(
                                                cbor::decode::parse(&block_data)
                                                    .map_field_err("Bundle Age Block")?,
                                            );
                                        }
                                        BlockType::HopCount => {
                                            self.hop_count = Some(
                                                cbor::decode::parse(&block_data)
                                                    .map_field_err("Hop Count Block")?,
                                            );
                                        }
                                        _ => {}
                                    }
                                    blocks_to_check.remove(&bib_target_block.block_type);
                                }

                                // Check if the target block is supported
                                if blocks_to_remove.contains(&bib_target_number) {
                                    bib_targets_remaining -= 1;
                                }
                            }

                            if bib_targets_remaining == 0 {
                                // All targets are unsupported
                                blocks_to_remove.insert(*bcb_target_number);
                            }
                        }

                        // Don't need to check this BIB again
                        bibs_to_check.remove(bcb_target_number);

                        // Don't need to reprocess this BCB target BIB
                        add_target = false;
                    }
                    _ => {}
                }

                if blocks_to_remove.contains(bcb_target_number) {
                    bcb_targets_remaining -= 1;
                } else if add_target {
                    bcbs.push((
                        bcb_target_number,
                        bcb_target_block,
                        &bcb.source,
                        bcb_op,
                        bcb_block,
                        bcb_block_number,
                    ));
                }
            }

            if bcb_targets_remaining == 0 {
                blocks_to_remove.insert(*bcb_block_number);
            } else {
                bcb_target_counts.insert(*bcb_block_number, bcb_targets_remaining);
            }
        }
        drop(bcb_targets);

        // Check non-BIB valid BCB targets next
        for (target_number, target_block, source, op, bcb_block, bcb_block_number) in bcbs {
            // Skip blocks we have already processed as BIB targets
            if bib_targets.contains(target_number) {
                continue;
            }

            // Confirm we can decrypt if we have keys
            if let Some((block_data, p)) = self.bcb_decrypt_block(
                keys,
                op,
                bpsec::OperationArgs {
                    bpsec_source: source,
                    target: target_block,
                    target_number,
                    source: bcb_block,
                    source_number: bcb_block_number,
                    bundle: self,
                    canonical_primary_block,
                    bundle_data: source_data,
                },
            )? {
                if p {
                    protects_primary_block.insert(*bcb_block_number);
                }

                match target_block.block_type {
                    BlockType::PreviousNode => {
                        self.previous_node = Some(
                            cbor::decode::parse(&block_data)
                                .map_field_err("Previous Node Block")?,
                        );
                    }
                    BlockType::BundleAge => {
                        self.age = Some(
                            cbor::decode::parse(&block_data).map_field_err("Bundle Age Block")?,
                        );
                    }
                    BlockType::HopCount => {
                        self.hop_count = Some(
                            cbor::decode::parse(&block_data).map_field_err("Hop Count Block")?,
                        );
                    }
                    _ => {}
                }
                blocks_to_check.remove(&target_block.block_type);
            }

            if blocks_to_remove.contains(target_number) {
                if let Some(remaining) = bcb_target_counts.get_mut(bcb_block_number) {
                    *remaining -= 1;
                    if *remaining == 0 {
                        blocks_to_remove.insert(*bcb_block_number);
                    }
                }
            }
        }
        drop(bcbs_to_check);

        // Check remaining BIB targets next
        for bib_block_number in bibs_to_check {
            let bib_block = self.blocks.get(&bib_block_number).unwrap();

            let bib = bib_block
                .parse_payload::<bpsec::bib::OperationSet>(source_data)
                .map(|(v, s)| {
                    if !s {
                        noncanonical_blocks.insert(bib_block_number, false);
                    }
                    v
                })
                .map_field_err("BPSec integrity extension block")?;

            if bib.is_unsupported() {
                if bib_block.flags.delete_bundle_on_failure {
                    return Err(BundleError::Unsupported(bib_block_number));
                }

                if bib_block.flags.delete_block_on_failure {
                    blocks_to_remove.insert(bib_block_number);
                }

                if bib_block.flags.report_on_failure {
                    report_unsupported = true;
                }
            }

            let mut bib_targets_remaining = bib.operations.len();
            for (bib_target_number, op) in bib.operations {
                if !bib_targets.insert(bib_target_number) {
                    return Err(bpsec::Error::DuplicateOpTarget.into());
                }
                let Some(bib_target_block) = self.blocks.get(&bib_target_number) else {
                    return Err(bpsec::Error::MissingSecurityTarget.into());
                };

                // Verify BIB target
                if let BlockType::BlockSecurity | BlockType::BlockIntegrity =
                    bib_target_block.block_type
                {
                    return Err(bpsec::Error::InvalidBIBTarget.into());
                }

                if self.bib_verify_data(
                    keys,
                    &op,
                    bpsec::OperationArgs {
                        bpsec_source: &bib.source,
                        target: bib_target_block,
                        target_number: &bib_target_number,
                        source: bib_block,
                        source_number: &bib_block_number,
                        bundle: self,
                        canonical_primary_block,
                        bundle_data: source_data,
                    },
                    None,
                )? {
                    protects_primary_block.insert(bib_block_number);
                }

                if blocks_to_remove.contains(&bib_target_number) {
                    bib_targets_remaining -= 1;
                }

                // Don't fully canonicalise BIB targets
                if let Some(is_bpsec_target) = noncanonical_blocks.get_mut(&bib_target_number) {
                    *is_bpsec_target = true;
                }
            }

            if bib_targets_remaining == 0 {
                blocks_to_remove.insert(bib_block_number);
            }
        }
        drop(bib_targets);

        // Check everything that isn't BCB covered
        for block_number in blocks_to_check.values() {
            let block = self.blocks.get(block_number).unwrap();
            if !match block.block_type {
                BlockType::PreviousNode => block
                    .parse_payload(source_data)
                    .map(|(v, s)| {
                        self.previous_node = Some(v);
                        s
                    })
                    .map_field_err("Previous Node Block")?,
                BlockType::BundleAge => block
                    .parse_payload(source_data)
                    .map(|(v, s)| {
                        self.age = Some(v);
                        s
                    })
                    .map_field_err("Bundle Age Block")?,
                BlockType::HopCount => block
                    .parse_payload(source_data)
                    .map(|(v, s)| {
                        self.hop_count = Some(v);
                        s
                    })
                    .map_field_err("Hop Count Block")?,
                _ => true,
            } && !noncanonical_blocks.contains_key(block_number)
            {
                noncanonical_blocks.insert(*block_number, false);
            }
        }

        // Check bundle age exists if needed
        if self.age.is_none() && self.id.timestamp.creation_time.is_none() {
            return Err(BundleError::MissingBundleAge);
        }

        if let CrcType::None = self.crc_type {
            if protects_primary_block.is_empty() {
                return Err(BundleError::MissingIntegrityCheck);
            }

            // We are going to need to add a CRC to the primary block!
            if protects_primary_block.is_subset(&blocks_to_remove) {
                self.crc_type = CrcType::CRC32_CASTAGNOLI;
                noncanonical_blocks.insert(0, false);
            }
        }

        // Sanity filter
        Ok((noncanonical_blocks, blocks_to_remove, report_unsupported))
    }

    pub fn emit_primary_block(&mut self, array: &mut cbor::encode::Array) {
        let data_start = array.offset();
        let data = primary_block::PrimaryBlock::emit(self);
        let data_len = data.len();
        array.emit_raw(data);

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
                data_start,
                payload_offset: 0,
                data_len,
            },
        );
    }

    fn canonicalise(
        &mut self,
        mut noncanonical_blocks: HashMap<u64, bool>,
        blocks_to_remove: HashSet<u64>,
        source_data: &[u8],
    ) -> Vec<u8> {
        cbor::encode::emit_array(None, |a| {
            // Emit primary block
            if noncanonical_blocks.remove(&0).is_some() {
                self.emit_primary_block(a);
            } else {
                self.blocks
                    .get_mut(&0)
                    .expect("Missing primary block!")
                    .copy(source_data, a);
            }

            // Stash payload block for last
            let mut payload_block = self.blocks.remove(&1).expect("No payload block!");

            // Emit extension blocks
            for (block_number, block) in &mut self.blocks {
                if let BlockType::Primary = block.block_type {
                    continue;
                }
                if !blocks_to_remove.contains(block_number) {
                    if let Some(is_bpsec_target) = noncanonical_blocks.remove(block_number) {
                        match block.block_type {
                            BlockType::PreviousNode if !is_bpsec_target => block.emit(
                                *block_number,
                                &cbor::encode::emit(self.previous_node.as_ref().unwrap()),
                                a,
                            ),
                            BlockType::BundleAge if !is_bpsec_target => {
                                block.emit(*block_number, &cbor::encode::emit(self.age.unwrap()), a)
                            }
                            BlockType::HopCount if !is_bpsec_target => block.emit(
                                *block_number,
                                &cbor::encode::emit(self.hop_count.as_ref().unwrap()),
                                a,
                            ),
                            BlockType::BlockIntegrity if !is_bpsec_target => block.emit(
                                *block_number,
                                &bpsec::bib::OperationSet::rewrite(
                                    block,
                                    &blocks_to_remove,
                                    source_data,
                                )
                                .expect("Invalid BIB Block"),
                                a,
                            ),
                            BlockType::BlockSecurity => block.emit(
                                *block_number,
                                &bpsec::bcb::OperationSet::rewrite(
                                    block,
                                    &blocks_to_remove,
                                    source_data,
                                )
                                .expect("Invalid BCB Block"),
                                a,
                            ),
                            _ => block.rewrite(*block_number, a, source_data).unwrap(),
                        }
                    } else {
                        block.copy(source_data, a);
                    }
                }
            }

            // Emit payload block
            if noncanonical_blocks.remove(&1).is_some() {
                payload_block.rewrite(1, a, source_data).unwrap();
            } else {
                payload_block.copy(source_data, a);
            }
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
        F: FnMut(&Eid, bpsec::Context) -> Result<Option<bpsec::KeyMaterial>, bpsec::Error>,
    {
        match cbor::decode::parse_array(data, |blocks, mut canonical, tags| {
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
            let mut canonical_primary_block = true;
            let block_start = blocks.offset();
            let (mut bundle, block_len) = blocks
                .parse::<(primary_block::PrimaryBlock, bool, usize)>()
                .map(|(v, s, len)| {
                    canonical_primary_block = s;
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
                    payload_offset: 0,
                    data_len: block_len,
                },
            );

            match bundle.parse_blocks(
                canonical_primary_block,
                blocks,
                block_start + block_len,
                data,
                f,
            ) {
                Ok((mut noncanonical_blocks, blocks_to_remove, report_unsupported)) => {
                    if !canonical_primary_block || !canonical {
                        noncanonical_blocks.insert(0, false);
                    }
                    Ok((
                        bundle,
                        noncanonical_blocks,
                        blocks_to_remove,
                        report_unsupported,
                    ))
                }
                Err(BundleError::Unsupported(n)) => Err(BundleError::InvalidBundle {
                    bundle: bundle.into(),
                    reason: StatusReportReasonCode::BlockUnsupported,
                    error: BundleError::Unsupported(n).into(),
                }),
                Err(e) => Err(BundleError::InvalidBundle {
                    bundle: bundle.into(),
                    reason: StatusReportReasonCode::BlockUnintelligible,
                    error: e.into(),
                }),
            }
        }) {
            Ok(((mut bundle, noncanonical_blocks, block_to_remove, report_unsupported), len)) => {
                if len != data.len() {
                    Ok(Self::Invalid(
                        bundle,
                        StatusReportReasonCode::BlockUnintelligible,
                        BundleError::AdditionalData.into(),
                    ))
                } else if !noncanonical_blocks.is_empty() || !block_to_remove.is_empty() {
                    let data = bundle.canonicalise(noncanonical_blocks, block_to_remove, data);
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
