use super::*;
use error::CaptureFieldErr;
use std::collections::{HashMap, HashSet};

trait KeyCache {
    fn get<'a>(
        &'a mut self,
        source: &Eid,
        context: bpsec::Context,
    ) -> Result<Option<&'a bpsec::KeyMaterial>, bpsec::Error>;
}

struct KeyCacheImpl<F>
where
    F: FnMut(&Eid, bpsec::Context) -> Result<Option<bpsec::KeyMaterial>, bpsec::Error>,
{
    keys: HashMap<Eid, HashMap<bpsec::Context, Option<bpsec::KeyMaterial>>>,
    f: F,
}

impl<F> KeyCacheImpl<F>
where
    F: FnMut(&Eid, bpsec::Context) -> Result<Option<bpsec::KeyMaterial>, bpsec::Error>,
{
    pub fn new(f: F) -> Self {
        Self {
            keys: HashMap::new(),
            f,
        }
    }
}

impl<F> KeyCache for KeyCacheImpl<F>
where
    F: FnMut(&Eid, bpsec::Context) -> Result<Option<bpsec::KeyMaterial>, bpsec::Error>,
{
    fn get<'a>(
        &'a mut self,
        source: &Eid,
        context: bpsec::Context,
    ) -> Result<Option<&'a bpsec::KeyMaterial>, bpsec::Error> {
        let inner = self.keys.entry(source.clone()).or_default();
        let v = inner.entry(context).or_insert((self.f)(source, context)?);
        Ok(v.as_ref())
    }
}

#[derive(Default)]
struct NoncanonicalInfo {
    bcb_can_rewrite: bool,
    bib_source: Option<u64>,
    bib_can_rewrite: bool,
    is_payload_noncanonical: bool,
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
    fn bcb_decrypt_block(
        &self,
        keys: &mut impl KeyCache,
        operation: &bpsec::bcb::Operation,
        args: bpsec::OperationArgs,
        protects_primary_block: &mut HashSet<u64>,
        noncanonical_blocks: &mut HashMap<u64, NoncanonicalInfo>,
    ) -> Result<(Option<Box<[u8]>>, bool), bpsec::Error> {
        let source_number = args.source_number;
        let target_number = args.target_number;
        let r = operation.decrypt(keys.get(args.bpsec_source, operation.context_id())?, args)?;
        if r.protects_primary_block {
            protects_primary_block.insert(source_number);
        }
        if r.can_encrypt {
            // Remember that the non-canonical block is a re-encryptable BCB target
            if let Some(info) = noncanonical_blocks.get_mut(&target_number) {
                info.bcb_can_rewrite = true;
            }
        }
        Ok((r.plaintext, r.can_encrypt))
    }

    fn bib_verify_data(
        &self,
        keys: &mut impl KeyCache,
        operation: &bpsec::bib::Operation,
        args: bpsec::OperationArgs,
        payload_data: Option<&[u8]>,
        protects_primary_block: &mut HashSet<u64>,
        noncanonical_blocks: &mut HashMap<u64, NoncanonicalInfo>,
    ) -> Result<bool, bpsec::Error> {
        let source_number = args.source_number;
        let target_number = args.target_number;
        let r = operation.verify(
            keys.get(args.bpsec_source, operation.context_id())?,
            args,
            payload_data,
        )?;
        if r.protects_primary_block {
            protects_primary_block.insert(source_number);
        }

        // Remember that the non-canonical block is a BIB target
        if let Some(info) = noncanonical_blocks.get_mut(&target_number) {
            info.bib_source = Some(source_number);
            info.bib_can_rewrite = r.can_sign;
        }

        Ok(r.can_sign)
    }

    /* Refactoring this huge function into parts doesn't really help readability,
     * and seems to drive the borrow checker insane */
    #[allow(clippy::type_complexity)]
    fn parse_blocks(
        &mut self,
        canonical_primary_block: bool,
        blocks: &mut cbor::decode::Array,
        mut offset: usize,
        source_data: &[u8],
        keys: &mut impl KeyCache,
    ) -> Result<(HashMap<u64, NoncanonicalInfo>, HashSet<u64>, bool), Error> {
        let mut last_block_number = 0;
        let mut noncanonical_blocks: HashMap<u64, NoncanonicalInfo> = HashMap::new();
        let mut blocks_to_check = HashMap::new();
        let mut blocks_to_remove = HashSet::new();
        let mut report_unsupported = false;
        let mut bcbs_to_check = Vec::new();
        let mut bibs_to_check = HashSet::new();
        let mut bcb_targets = HashMap::new();
        let mut protects_primary_block = HashSet::new();

        // Parse the blocks and build a map
        while let Some((mut block, s, block_len)) =
            blocks.try_parse::<(block::BlockWithNumber, bool, usize)>()?
        {
            if !s {
                noncanonical_blocks.entry(block.number).or_default();
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
                        return Err(Error::DuplicateBlocks(block.block.block_type));
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
                                noncanonical_blocks
                                    .entry(block.number)
                                    .or_default()
                                    .is_payload_noncanonical = true;
                            }
                            v
                        })
                        .map_field_err("BPSec confidentiality extension block")?;

                    if bcb.is_unsupported() {
                        if block.block.flags.delete_bundle_on_failure {
                            return Err(Error::Unsupported(block.number));
                        }

                        if block.block.flags.report_on_failure {
                            report_unsupported = true;
                        }
                    }
                    bcbs_to_check.push((block.number, bcb));
                }
                BlockType::Unrecognised(_) => {
                    if block.block.flags.delete_bundle_on_failure {
                        return Err(Error::Unsupported(block.number));
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
                return Err(Error::DuplicateBlockNumber(block.number));
            }

            last_block_number = block.number;
            offset += block_len;
        }

        // Check the last block is the payload
        if let Some(payload_block_number) = blocks_to_check.remove(&BlockType::Payload) {
            if payload_block_number != last_block_number {
                return Err(Error::PayloadNotFinal);
            }
        } else {
            return Err(Error::MissingPayload);
        }

        // Do the first BCB pass, checking BIBs and general sanity
        let mut bcbs = Vec::new();
        let mut bib_targets = HashSet::new();
        let mut bcb_target_counts = HashMap::new();
        for (bcb_block_number, bcb) in &bcbs_to_check {
            let bcb_block = self.blocks.get(bcb_block_number).unwrap();
            let mut bcb_targets_remaining = bcb.operations.len();
            for (bcb_target_number, bcb_op) in &bcb.operations {
                if bcb_targets
                    .insert(*bcb_target_number, *bcb_block_number)
                    .is_some()
                {
                    return Err(bpsec::Error::DuplicateOpTarget.into());
                }
                let Some(bcb_target_block) = self.blocks.get(bcb_target_number) else {
                    return Err(bpsec::Error::MissingSecurityTarget.into());
                };

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
                        if let (Some(bib), can_encrypt) = self.bcb_decrypt_block(
                            keys,
                            bcb_op,
                            bpsec::OperationArgs {
                                bpsec_source: &bcb.source,
                                target: bcb_target_block,
                                target_number: *bcb_target_number,
                                source: bcb_block,
                                source_number: *bcb_block_number,
                                bundle: self,
                                canonical_primary_block,
                                bundle_data: source_data,
                            },
                            &mut protects_primary_block,
                            &mut noncanonical_blocks,
                        )? {
                            let (bib, s) =
                                cbor::decode::parse::<(bpsec::bib::OperationSet, bool)>(&bib)
                                    .map_field_err("BPSec integrity extension block")?;
                            if !s {
                                // If we can't re-encrypt, we can't rewrite
                                if !can_encrypt {
                                    return Err(Error::NonCanonical(*bcb_target_number));
                                }

                                noncanonical_blocks.insert(
                                    *bcb_target_number,
                                    NoncanonicalInfo {
                                        bcb_can_rewrite: true,
                                        is_payload_noncanonical: true,
                                        ..Default::default()
                                    },
                                );
                            };

                            if bib.is_unsupported() {
                                if bcb_target_block.flags.delete_bundle_on_failure {
                                    return Err(Error::Unsupported(*bcb_target_number));
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
                                if let (Some(block_data), can_encrypt) = self.bcb_decrypt_block(
                                    keys,
                                    bcb_op,
                                    bpsec::OperationArgs {
                                        bpsec_source: &bcb.source,
                                        target: bib_target_block,
                                        target_number: bib_target_number,
                                        source: bcb_block,
                                        source_number: *bcb_block_number,
                                        bundle: self,
                                        canonical_primary_block,
                                        bundle_data: source_data,
                                    },
                                    &mut protects_primary_block,
                                    &mut noncanonical_blocks,
                                )? {
                                    // Do BIB verification
                                    let can_resign = self.bib_verify_data(
                                        keys,
                                        &bib_op,
                                        bpsec::OperationArgs {
                                            bpsec_source: &bib.source,
                                            target: bib_target_block,
                                            target_number: bib_target_number,
                                            source: bcb_target_block,
                                            source_number: *bcb_target_number,
                                            bundle: self,
                                            canonical_primary_block,
                                            bundle_data: source_data,
                                        },
                                        Some(&block_data),
                                        &mut protects_primary_block,
                                        &mut noncanonical_blocks,
                                    )?;

                                    // And parse if needed
                                    if blocks_to_check
                                        .remove(&bib_target_block.block_type)
                                        .is_some()
                                        && !match bib_target_block.block_type {
                                            BlockType::PreviousNode => {
                                                cbor::decode::parse::<(Eid, bool)>(&block_data)
                                                    .map(|(v, s)| {
                                                        self.previous_node = Some(v);
                                                        s
                                                    })
                                                    .map_field_err("Previous Node Block")?
                                            }
                                            BlockType::BundleAge => {
                                                cbor::decode::parse::<(u64, bool)>(&block_data)
                                                    .map(|(v, s)| {
                                                        self.age = Some(v);
                                                        s
                                                    })
                                                    .map_field_err("Bundle Age Block")?
                                            }
                                            BlockType::HopCount => {
                                                cbor::decode::parse::<(HopInfo, bool)>(&block_data)
                                                    .map(|(v, s)| {
                                                        self.hop_count = Some(v);
                                                        s
                                                    })
                                                    .map_field_err("Hop Count Block")?
                                            }
                                            _ => true,
                                        }
                                    {
                                        // If we can't re-encrypt or re-sign, we can't rewrite
                                        if !can_encrypt || !can_resign {
                                            return Err(Error::NonCanonical(*bcb_target_number));
                                        }

                                        noncanonical_blocks.insert(
                                            *bcb_target_number,
                                            NoncanonicalInfo {
                                                bcb_can_rewrite: true,
                                                bib_source: Some(*bcb_target_number),
                                                bib_can_rewrite: true,
                                                is_payload_noncanonical: true,
                                            },
                                        );
                                    }
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
                        *bcb_target_number,
                        bcb_target_block,
                        &bcb.source,
                        bcb_op,
                        bcb_block,
                        *bcb_block_number,
                    ));
                }
            }

            if bcb_targets_remaining == 0 {
                blocks_to_remove.insert(*bcb_block_number);
            } else {
                bcb_target_counts.insert(*bcb_block_number, bcb_targets_remaining);
            }
        }

        // Check non-BIB valid BCB targets next
        for (target_number, target_block, source, op, bcb_block, bcb_block_number) in bcbs {
            // Skip blocks we have already processed as BIB targets
            if bib_targets.contains(&target_number) {
                continue;
            }

            // Confirm we can decrypt if we have keys
            if let (Some(block_data), can_encrypt) = self.bcb_decrypt_block(
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
                &mut protects_primary_block,
                &mut noncanonical_blocks,
            )? {
                if blocks_to_check.remove(&target_block.block_type).is_some()
                    && !match target_block.block_type {
                        BlockType::PreviousNode => cbor::decode::parse::<(Eid, bool)>(&block_data)
                            .map(|(v, s)| {
                                self.previous_node = Some(v);
                                s
                            })
                            .map_field_err("Previous Node Block")?,
                        BlockType::BundleAge => cbor::decode::parse::<(u64, bool)>(&block_data)
                            .map(|(v, s)| {
                                self.age = Some(v);
                                s
                            })
                            .map_field_err("Bundle Age Block")?,
                        BlockType::HopCount => cbor::decode::parse::<(HopInfo, bool)>(&block_data)
                            .map(|(v, s)| {
                                self.hop_count = Some(v);
                                s
                            })
                            .map_field_err("Hop Count Block")?,
                        _ => true,
                    }
                {
                    // If we can't re-encrypt, we can't rewrite
                    if !can_encrypt {
                        return Err(Error::NonCanonical(target_number));
                    }

                    noncanonical_blocks.insert(
                        target_number,
                        NoncanonicalInfo {
                            bcb_can_rewrite: true,
                            is_payload_noncanonical: true,
                            ..Default::default()
                        },
                    );
                }
            }

            if blocks_to_remove.contains(&target_number) {
                if let Some(remaining) = bcb_target_counts.get_mut(&bcb_block_number) {
                    *remaining -= 1;
                    if *remaining == 0 {
                        blocks_to_remove.insert(bcb_block_number);
                    }
                }
            }
        }

        // Record the BCB that targets this block
        for (bcb_target, bcb) in bcb_targets {
            self.blocks.get_mut(&bcb_target).unwrap().bcb = Some(bcb);
        }

        // Check remaining BIB targets next
        for bib_block_number in bibs_to_check {
            let bib_block = self.blocks.get(&bib_block_number).unwrap();

            let bib = bib_block
                .parse_payload::<bpsec::bib::OperationSet>(source_data)
                .map(|(v, s)| {
                    if !s {
                        noncanonical_blocks
                            .entry(bib_block_number)
                            .or_default()
                            .is_payload_noncanonical = true;
                    }
                    v
                })
                .map_field_err("BPSec integrity extension block")?;

            if bib.is_unsupported() {
                if bib_block.flags.delete_bundle_on_failure {
                    return Err(Error::Unsupported(bib_block_number));
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

                let can_resign = self.bib_verify_data(
                    keys,
                    &op,
                    bpsec::OperationArgs {
                        bpsec_source: &bib.source,
                        target: bib_target_block,
                        target_number: bib_target_number,
                        source: bib_block,
                        source_number: bib_block_number,
                        bundle: self,
                        canonical_primary_block,
                        bundle_data: source_data,
                    },
                    None,
                    &mut protects_primary_block,
                    &mut noncanonical_blocks,
                )?;

                if blocks_to_check
                    .remove(&bib_target_block.block_type)
                    .is_some()
                    && !match bib_target_block.block_type {
                        BlockType::PreviousNode => bib_target_block
                            .parse_payload(source_data)
                            .map(|(v, s)| {
                                self.previous_node = Some(v);
                                s
                            })
                            .map_field_err("Previous Node Block")?,
                        BlockType::BundleAge => bib_target_block
                            .parse_payload(source_data)
                            .map(|(v, s)| {
                                self.age = Some(v);
                                s
                            })
                            .map_field_err("Bundle Age Block")?,
                        BlockType::HopCount => bib_target_block
                            .parse_payload(source_data)
                            .map(|(v, s)| {
                                self.hop_count = Some(v);
                                s
                            })
                            .map_field_err("Hop Count Block")?,
                        _ => true,
                    }
                {
                    // If we can't re-sign, we can't rewrite
                    if !can_resign {
                        return Err(Error::NonCanonical(bib_target_number));
                    }

                    noncanonical_blocks.insert(
                        bib_target_number,
                        NoncanonicalInfo {
                            bib_source: Some(bib_block_number),
                            bib_can_rewrite: true,
                            is_payload_noncanonical: true,
                            ..Default::default()
                        },
                    );
                }

                if blocks_to_remove.contains(&bib_target_number) {
                    bib_targets_remaining -= 1;
                }
            }

            if bib_targets_remaining == 0 {
                blocks_to_remove.insert(bib_block_number);
            }
        }

        // Check everything that isn't already checked
        for (_, block_number) in blocks_to_check {
            let block = self.blocks.get(&block_number).unwrap();
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
            } {
                noncanonical_blocks
                    .entry(block_number)
                    .or_default()
                    .is_payload_noncanonical = true;
            }
        }

        // Check bundle age exists if needed
        if self.age.is_none() && self.id.timestamp.creation_time.is_none() {
            return Err(Error::MissingBundleAge);
        }

        if let CrcType::None = self.crc_type {
            if protects_primary_block.is_empty()
                || protects_primary_block.is_subset(&blocks_to_remove)
            {
                return Err(Error::MissingIntegrityCheck);
            }
        }

        Ok((noncanonical_blocks, blocks_to_remove, report_unsupported))
    }

    pub fn emit_primary_block(&mut self, array: &mut cbor::encode::Array, bcb: Option<u64>) {
        let data_start = array.offset();
        let data = primary_block::PrimaryBlock::emit(self);
        let payload_len = data.len();
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
                data_len: payload_len,
                payload_offset: 0,
                payload_len,
                bcb,
            },
        );
    }

    fn canonicalise(
        &mut self,
        mut noncanonical_blocks: HashMap<u64, NoncanonicalInfo>,
        blocks_to_remove: HashSet<u64>,
        source_data: &[u8],
    ) -> Vec<u8> {
        cbor::encode::emit_array(None, |a| {
            // Emit primary block
            if noncanonical_blocks.remove(&0).is_some() {
                self.emit_primary_block(a, self.blocks.get(&0).unwrap().bcb);
            } else {
                self.blocks.get_mut(&0).unwrap().copy(source_data, a);
            }

            // Stash payload block for last
            let mut payload_block = self.blocks.remove(&1).unwrap();

            // Emit extension blocks
            for (block_number, block) in &mut self.blocks {
                if let BlockType::Primary = block.block_type {
                    continue;
                }

                // Skip blocks to be removed
                if !blocks_to_remove.contains(block_number) {
                    if let Some(info) = noncanonical_blocks.remove(block_number) {
                        // TODO: We can be much smarter here with re-signing/re-encrypting the data
                        let can_rewrite = block.bcb.is_none() && info.bib_source.is_none();

                        match block.block_type {
                            BlockType::PreviousNode
                                if info.is_payload_noncanonical && can_rewrite =>
                            {
                                block.emit(
                                    *block_number,
                                    &cbor::encode::emit(self.previous_node.as_ref().unwrap()),
                                    a,
                                )
                            }
                            BlockType::BundleAge if info.is_payload_noncanonical && can_rewrite => {
                                block.emit(*block_number, &cbor::encode::emit(self.age.unwrap()), a)
                            }
                            BlockType::HopCount if can_rewrite => block.emit(
                                *block_number,
                                &cbor::encode::emit(self.hop_count.as_ref().unwrap()),
                                a,
                            ),
                            BlockType::BlockIntegrity
                                if info.is_payload_noncanonical && can_rewrite =>
                            {
                                block.emit(
                                    *block_number,
                                    &bpsec::bib::OperationSet::rewrite(
                                        block,
                                        &blocks_to_remove,
                                        source_data,
                                    )
                                    .unwrap(),
                                    a,
                                )
                            }
                            BlockType::BlockSecurity if info.is_payload_noncanonical => block.emit(
                                *block_number,
                                &bpsec::bcb::OperationSet::rewrite(
                                    block,
                                    &blocks_to_remove,
                                    source_data,
                                )
                                .unwrap(),
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
                payload_block.write(source_data, a);
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
    pub fn parse<F>(data: &[u8], f: F) -> Result<Self, Error>
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
                    data_len: block_len,
                    payload_offset: 0,
                    payload_len: block_len,
                    bcb: None,
                },
            );

            let mut keys = KeyCacheImpl::new(f);

            match bundle.parse_blocks(
                canonical_primary_block,
                blocks,
                block_start + block_len,
                data,
                &mut keys,
            ) {
                Ok((mut noncanonical_blocks, blocks_to_remove, report_unsupported)) => {
                    if !canonical_primary_block || !canonical {
                        noncanonical_blocks.entry(0).or_default();
                    }
                    Ok((
                        bundle,
                        noncanonical_blocks,
                        blocks_to_remove,
                        report_unsupported,
                    ))
                }
                Err(Error::Unsupported(n)) => Err(Error::InvalidBundle {
                    bundle: bundle.into(),
                    reason: StatusReportReasonCode::BlockUnsupported,
                    error: Error::Unsupported(n).into(),
                }),
                Err(e) => Err(Error::InvalidBundle {
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
                        Error::AdditionalData.into(),
                    ))
                } else if !noncanonical_blocks.is_empty() || !block_to_remove.is_empty() {
                    let data = bundle.canonicalise(noncanonical_blocks, block_to_remove, data);
                    Ok(Self::Rewritten(bundle, data.into(), report_unsupported))
                } else {
                    Ok(Self::Valid(bundle, report_unsupported))
                }
            }
            Err(Error::InvalidBundle {
                bundle,
                reason,
                error: e,
            }) => Ok(Self::Invalid(*bundle, reason, e)),
            Err(e) => Err(e),
        }
    }
}
