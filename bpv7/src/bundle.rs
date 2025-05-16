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

pub enum Payload {
    Borrowed(std::ops::Range<usize>),
    Owned(zeroize::Zeroizing<Box<[u8]>>),
}

impl std::fmt::Debug for Payload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Borrowed(arg0) => write!(f, "Payload {} bytes", arg0.len()),
            Self::Owned(arg0) => write!(f, "Payload {} bytes", arg0.len()),
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
    pub lifetime: time::Duration,

    // Unpacked from extension blocks
    pub previous_node: Option<Eid>,
    pub age: Option<time::Duration>,
    pub hop_count: Option<HopInfo>,

    // The extension blocks
    pub blocks: std::collections::HashMap<u64, Block>,
}

impl Bundle {
    pub(crate) fn emit_primary_block(&mut self, array: &mut cbor::encode::Array) {
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
                bcb: None,
            },
        );
    }

    fn parse_payload<T>(
        &self,
        block_number: &u64,
        decrypted_data: Option<&(zeroize::Zeroizing<Box<[u8]>>, bool)>,
        source_data: &[u8],
    ) -> Result<(&Block, T, bool), Error>
    where
        T: cbor::decode::FromCbor<Error: From<cbor::decode::Error> + Into<Error>>,
    {
        if let Some((block_data, can_encrypt)) = decrypted_data {
            match cbor::decode::parse::<(T, bool, usize)>(block_data)
                .map(|(v, s, len)| (v, s && len == block_data.len()))
            {
                Ok((v, s)) => {
                    // If we can't re-encrypt, we can't rewrite
                    if !s && !can_encrypt {
                        Err(Error::NonCanonical(*block_number))
                    } else {
                        Ok((self.blocks.get(block_number).unwrap(), v, s))
                    }
                }
                Err(e) => Err(e.into()),
            }
        } else {
            let block = self.blocks.get(block_number).unwrap();
            cbor::decode::parse_value(block.payload(source_data), |v, _, _| match v {
                cbor::decode::Value::Bytes(data) => cbor::decode::parse::<(T, bool, usize)>(data)
                    .map(|(v, s, len)| (v, s && len == data.len())),
                cbor::decode::Value::ByteStream(data) => cbor::decode::parse::<(T, bool, usize)>(
                    &data.iter().fold(Vec::new(), |mut data, d| {
                        data.extend(*d);
                        data
                    }),
                )
                .map(|(v, s, len)| (v, s && len == data.len())),
                _ => unreachable!(),
            })
            .map(|((v, s), _)| (block, v, s))
            .map_err(Into::into)
        }
    }

    /* Refactoring this huge function into parts doesn't really help readability,
     * and seems to drive the borrow checker insane */
    #[allow(clippy::type_complexity)]
    fn parse_blocks(
        &mut self,
        canonical_bundle: bool,
        canonical_primary_block: bool,
        blocks: &mut cbor::decode::Array,
        mut offset: usize,
        source_data: &[u8],
        keys: &mut impl KeyCache,
    ) -> Result<(Option<Box<[u8]>>, bool), Error> {
        let mut last_block_number = 0;
        let mut noncanonical_blocks: HashMap<u64, bool> = HashMap::new();
        let mut blocks_to_check = HashMap::new();
        let mut blocks_to_remove = HashSet::new();
        let mut report_unsupported = false;
        let mut bcbs_to_check = Vec::new();
        let mut bibs_to_check = HashSet::new();

        // Parse the blocks and build a map
        while let Some((mut block, canonical, block_len)) =
            blocks.try_parse::<(block::BlockWithNumber, bool, usize)>()?
        {
            block.block.data_start += offset;

            if !canonical {
                noncanonical_blocks.insert(block.number, false);
            }

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
                    bcbs_to_check.push(block.number);
                }
                BlockType::Unrecognised(_) => {
                    if block.block.flags.delete_bundle_on_failure {
                        return Err(Error::Unsupported(block.number));
                    }

                    if block.block.flags.report_on_failure {
                        report_unsupported = true;
                    }

                    if block.block.flags.delete_block_on_failure {
                        noncanonical_blocks.remove(&block.number);
                        blocks_to_remove.insert(block.number);
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
        if blocks_to_check
            .remove(&BlockType::Payload)
            .ok_or(Error::MissingPayload)?
            != last_block_number
        {
            return Err(Error::PayloadNotFinal);
        }

        // Check for spurious extra data
        if blocks.offset() != source_data.len() {
            return Err(Error::AdditionalData);
        }

        // Rewrite primary block if required
        let primary_block =
            (!canonical_primary_block).then_some(primary_block::PrimaryBlock::emit(self));

        // Decrypt all BCB targets first
        let mut decrypted_data = HashMap::new();
        let mut protects_primary_block = HashSet::new();
        let mut bcb_targets = HashMap::new();
        let mut bcbs = HashMap::new();
        for bcb_block_number in bcbs_to_check {
            // Parse the BCB
            let (bcb_block, mut bcb, s) = self
                .parse_payload::<bpsec::bcb::OperationSet>(&bcb_block_number, None, source_data)
                .map_field_err("BPSec confidentiality extension block")?;

            if !s {
                noncanonical_blocks.insert(bcb_block_number, true);
            }

            if bcb_block.flags.delete_block_on_failure {
                return Err(bpsec::Error::BCBDeleteFlag.into());
            }

            if bcb.is_unsupported() {
                if bcb_block.flags.delete_bundle_on_failure {
                    return Err(Error::Unsupported(bcb_block_number));
                }

                if bcb_block.flags.delete_block_on_failure {
                    return Err(bpsec::Error::BCBDeleteFlag.into());
                }

                if bcb_block.flags.report_on_failure {
                    report_unsupported = true;
                }
            }

            // Decrypt targets
            let mut targets_to_drop = HashSet::new();
            for (target_number, op) in &bcb.operations {
                if bcb_targets
                    .insert(*target_number, bcb_block_number)
                    .is_some()
                {
                    return Err(bpsec::Error::DuplicateOpTarget.into());
                }
                let Some(target_block) = self.blocks.get(target_number) else {
                    return Err(bpsec::Error::MissingSecurityTarget.into());
                };

                match target_block.block_type {
                    BlockType::BlockSecurity | BlockType::Primary => {
                        return Err(bpsec::Error::InvalidBCBTarget.into());
                    }
                    BlockType::Payload => {
                        // Check flags
                        if !bcb_block.flags.must_replicate {
                            return Err(bpsec::Error::BCBMustReplicate.into());
                        }
                    }
                    _ => {}
                }

                // Confirm we can decrypt if we have keys
                let r = op.decrypt(
                    keys.get(&bcb.source, op.context_id())?,
                    bpsec::bcb::OperationArgs {
                        bpsec_source: &bcb.source,
                        target: target_block,
                        target_number: *target_number,
                        source: bcb_block,
                        source_number: bcb_block_number,
                        bundle: self,
                        primary_block: primary_block.as_deref(),
                        bundle_data: source_data,
                    },
                    None,
                )?;

                if !blocks_to_remove.contains(target_number) {
                    match (target_block.block_type, r.plaintext) {
                        (BlockType::PreviousNode | BlockType::HopCount, Some(block_data)) => {
                            // We will always replace these blocks when forwarded
                            decrypted_data.insert(*target_number, (block_data, true));

                            // We will rewrite the block unencrypted for now
                            noncanonical_blocks.insert(*target_number, true);

                            // And not re-encrypt
                            targets_to_drop.insert(*target_number);
                        }
                        (BlockType::BlockIntegrity, None) => {
                            // We can't decrypt, therefore we cannot check the BIB
                            bibs_to_check.remove(target_number);
                        }
                        (_, Some(block_data)) => {
                            decrypted_data.insert(*target_number, (block_data, r.can_encrypt));

                            if r.protects_primary_block {
                                protects_primary_block.insert(bcb_block_number);
                            }
                        }
                        _ => {}
                    }
                }
            }

            // Remove any operations we need to rewrite
            if !targets_to_drop.is_empty() {
                bcb.operations.retain(|k, _| !targets_to_drop.contains(k));

                // Ensure we rewrite the BCB
                noncanonical_blocks.insert(bcb_block_number, true);
            }

            bcbs.insert(bcb_block_number, bcb);
        }

        // Mark all blocks that are BCB targets
        for (target, bcb) in bcb_targets {
            self.blocks.get_mut(&target).unwrap().bcb = Some(bcb);
        }

        // Now parse all the non-BIBs we need to check
        for (block_type, block_number) in blocks_to_check {
            if !match block_type {
                BlockType::PreviousNode => {
                    let (_, v, s) = self
                        .parse_payload(
                            &block_number,
                            decrypted_data.get(&block_number),
                            source_data,
                        )
                        .map_field_err("Previous Node Block")?;
                    self.previous_node = Some(v);
                    s
                }
                BlockType::BundleAge => {
                    let (_, v, s) = self
                        .parse_payload::<u64>(
                            &block_number,
                            decrypted_data.get(&block_number),
                            source_data,
                        )
                        .map(|(b, a, s)| (b, time::Duration::milliseconds(a as i64), s))
                        .map_field_err("Bundle Age Block")?;
                    self.age = Some(v);
                    s
                }
                BlockType::HopCount => {
                    let (_, v, s) = self
                        .parse_payload(
                            &block_number,
                            decrypted_data.get(&block_number),
                            source_data,
                        )
                        .map_field_err("Hop Count Block")?;
                    self.hop_count = Some(v);
                    s
                }
                _ => true,
            } {
                noncanonical_blocks.insert(block_number, true);
            }
        }

        // Check bundle age exists if needed
        if self.age.is_none() && self.id.timestamp.creation_time.is_none() {
            return Err(Error::MissingBundleAge);
        }

        // Now parse all BIBs
        let mut bibs = HashMap::new();
        let mut bib_targets = HashSet::new();
        for bib_block_number in bibs_to_check {
            let (bib_block, mut bib, canonical) = self
                .parse_payload::<bpsec::bib::OperationSet>(
                    &bib_block_number,
                    decrypted_data.get(&bib_block_number),
                    source_data,
                )
                .map_field_err("BPSec integrity extension block")?;

            if bib.is_unsupported() {
                if bib_block.flags.delete_bundle_on_failure {
                    return Err(Error::Unsupported(bib_block_number));
                }

                if bib_block.flags.report_on_failure {
                    report_unsupported = true;
                }

                if bib_block.flags.delete_block_on_failure {
                    noncanonical_blocks.remove(&bib_block_number);
                    blocks_to_remove.insert(bib_block_number);
                    continue;
                }
            }

            let mut targets_to_drop = HashSet::new();
            let bcb = bib_block.bcb.and_then(|b| bcbs.get(&b));

            // Check targets
            for (target_number, op) in &bib.operations {
                if !bib_targets.insert(*target_number) {
                    return Err(bpsec::Error::DuplicateOpTarget.into());
                }
                let Some(target_block) = self.blocks.get(target_number) else {
                    return Err(bpsec::Error::MissingSecurityTarget.into());
                };

                // Verify BIB target
                if let BlockType::BlockSecurity | BlockType::BlockIntegrity =
                    target_block.block_type
                {
                    return Err(bpsec::Error::InvalidBIBTarget.into());
                }

                if let Some(bcb) = bcb {
                    // Check we share a target with our BCB
                    if !bcb.operations.contains_key(target_number) {
                        return Err(bpsec::Error::BCBMustShareTarget.into());
                    }
                }

                let (payload_data, can_encrypt) = decrypted_data
                    .get(target_number)
                    .map_or((None, true), |(v, c)| (Some(v.as_ref()), *c));

                let r = op.verify(
                    keys.get(&bib.source, op.context_id())?,
                    bpsec::bib::OperationArgs {
                        bpsec_source: &bib.source,
                        target: target_block,
                        target_number: *target_number,
                        source: bib_block,
                        source_number: bib_block_number,
                        bundle: self,
                        primary_block: primary_block.as_deref(),
                        bundle_data: source_data,
                    },
                    payload_data,
                )?;

                if !blocks_to_remove.contains(target_number) {
                    if let BlockType::PreviousNode | BlockType::HopCount = target_block.block_type {
                        // Do not re-sign, we will rewrite when we forward
                        targets_to_drop.insert(*target_number);
                    } else {
                        if let Some(true) = noncanonical_blocks.get(target_number) {
                            // If we can't re-encrypt or re-sign, we can't rewrite
                            if !can_encrypt || !r.can_sign {
                                return Err(Error::NonCanonical(*target_number));
                            }
                        }

                        if r.protects_primary_block {
                            protects_primary_block.insert(bib_block_number);
                        }
                    }
                }
            }

            // Remove targets scheduled for removal
            let old_len = bib.operations.len();
            bib.operations
                .retain(|k, _| !blocks_to_remove.contains(k) && !targets_to_drop.contains(k));
            if bib.operations.is_empty() {
                noncanonical_blocks.remove(&bib_block_number);
                protects_primary_block.remove(&bib_block_number);
                blocks_to_remove.insert(bib_block_number);
                continue;
            } else if !canonical || bib.operations.len() != old_len {
                noncanonical_blocks.insert(bib_block_number, true);
                bibs.insert(bib_block_number, (bib_block, bib));
            }
        }

        // Reduce BCB targets scheduled for removal
        bcbs.retain(|bcb_block_number, bcb| {
            let old_len = bcb.operations.len();
            bcb.operations.retain(|k, _| !blocks_to_remove.contains(k));
            if bcb.operations.is_empty() {
                noncanonical_blocks.remove(bcb_block_number);
                protects_primary_block.remove(bcb_block_number);
                blocks_to_remove.insert(*bcb_block_number);
                false
            } else if bcb.operations.len() != old_len {
                noncanonical_blocks.insert(*bcb_block_number, true);
                true
            } else {
                false
            }
        });

        // Check we have at least some primary block protection
        if let CrcType::None = self.crc_type {
            if protects_primary_block.is_empty() {
                return Err(Error::MissingIntegrityCheck);
            }
        }

        // If we have nothing to rewrite, get out now
        if canonical_bundle
            && canonical_primary_block
            && noncanonical_blocks.is_empty()
            && blocks_to_remove.is_empty()
        {
            return Ok((None, report_unsupported));
        }

        // Now start rewriting blocks
        let mut new_payloads: HashMap<u64, Box<[u8]>> = HashMap::new();
        noncanonical_blocks.retain(|block_number, is_payload_noncanonical| {
            if *is_payload_noncanonical {
                match self.blocks.get(block_number).unwrap().block_type {
                    BlockType::PreviousNode => {
                        new_payloads.insert(
                            *block_number,
                            cbor::encode::emit(self.previous_node.as_ref().unwrap()).into(),
                        );
                        false
                    }
                    BlockType::BundleAge => {
                        new_payloads.insert(
                            *block_number,
                            cbor::encode::emit(self.age.unwrap().whole_milliseconds() as u64)
                                .into(),
                        );
                        false
                    }
                    BlockType::HopCount => {
                        new_payloads.insert(
                            *block_number,
                            cbor::encode::emit(self.hop_count.as_ref().unwrap()).into(),
                        );
                        false
                    }
                    BlockType::BlockIntegrity | BlockType::BlockSecurity => {
                        /* ignore for now  */
                        true
                    }
                    _ => unreachable!(),
                }
            } else {
                true
            }
        });

        // Update BIBs
        for (bib_block_number, (bib_block, mut bib)) in bibs {
            for (target_number, op) in bib.operations.iter_mut() {
                if let Some(payload_data) = new_payloads.get(target_number) {
                    let target_block = self.blocks.get(target_number).unwrap();
                    op.sign(
                        keys.get(&bib.source, op.context_id())?,
                        bpsec::bib::OperationArgs {
                            bpsec_source: &bib.source,
                            target: target_block,
                            target_number: *target_number,
                            source: bib_block,
                            source_number: bib_block_number,
                            bundle: self,
                            primary_block: primary_block.as_deref(),
                            bundle_data: source_data,
                        },
                        Some(payload_data),
                    )?;
                }
            }

            noncanonical_blocks.remove(&bib_block_number);
            new_payloads.insert(bib_block_number, cbor::encode::emit(bib).into());
        }

        // Encrypt blocks and update BCBs
        for (bcb_block_number, mut bcb) in bcbs {
            let bcb_block = self.blocks.get(&bcb_block_number).unwrap();
            for (target_number, op) in bcb.operations.iter_mut() {
                if let Some(payload_data) = new_payloads.get(target_number) {
                    let target_block = self.blocks.get(target_number).unwrap();
                    let new_data = op.encrypt(
                        keys.get(&bcb.source, op.context_id())?,
                        bpsec::bcb::OperationArgs {
                            bpsec_source: &bcb.source,
                            target: target_block,
                            target_number: *target_number,
                            source: bcb_block,
                            source_number: bcb_block_number,
                            bundle: self,
                            primary_block: primary_block.as_deref(),
                            bundle_data: source_data,
                        },
                        Some(payload_data),
                    )?;
                    new_payloads.insert(*target_number, new_data);
                }
            }

            noncanonical_blocks.remove(&bcb_block_number);
            new_payloads.insert(bcb_block_number, cbor::encode::emit(bcb).into());
        }

        let new_data = cbor::encode::emit_array(None, |a| {
            // Emit primary
            if let Some(p) = primary_block {
                a.emit_raw(p);
            } else {
                self.blocks.get_mut(&0).unwrap().copy(source_data, a);
            }

            // Stash payload block for last
            let mut payload_block = self.blocks.remove(&1).unwrap();

            // Emit blocks
            self.blocks.retain(|block_number, block| {
                if *block_number == 0 {
                    return true;
                }
                if blocks_to_remove.contains(block_number) {
                    return false;
                }

                if let Some(data) = new_payloads.remove(block_number) {
                    block.emit(*block_number, &data, a);
                } else if noncanonical_blocks.remove(block_number).is_some() {
                    block.rewrite(*block_number, a, source_data);
                } else {
                    // Copy canonical blocks verbatim
                    block.write(source_data, a);
                }
                true
            });

            // Emit payload block
            if noncanonical_blocks.remove(&1).is_some() {
                payload_block.rewrite(1, a, source_data);
            } else {
                payload_block.write(source_data, a);
            }
            self.blocks.insert(1, payload_block);
        });
        Ok((Some(new_data.into()), report_unsupported))
    }

    pub fn payload(
        &self,
        data: &[u8],
        mut f: impl FnMut(&Eid, bpsec::Context) -> Result<Option<bpsec::KeyMaterial>, bpsec::Error>,
    ) -> Result<Payload, Error> {
        let Some(payload_block) = self.blocks.get(&1) else {
            return Err(Error::Altered);
        };

        // Check for BCB
        let Some(bcb_block_number) = payload_block.bcb else {
            return Ok(Payload::Borrowed(payload_block.payload_range()));
        };

        let (bcb_block, bcb, _) = self
            .parse_payload::<bpsec::bcb::OperationSet>(&bcb_block_number, None, data)
            .map_err(|_| Error::Altered)?;

        let Some(op) = bcb.operations.get(&1) else {
            // If the operation doesn't exist, someone has fiddled with the data
            return Err(Error::Altered);
        };

        let Some(key) = f(&bcb.source, op.context_id())? else {
            return Err(bpsec::Error::NoKey(bcb.source).into());
        };

        // Confirm we can decrypt if we have keys
        let Some(data) = op
            .decrypt(
                Some(&key),
                bpsec::bcb::OperationArgs {
                    bpsec_source: &bcb.source,
                    target: payload_block,
                    target_number: 1,
                    source: bcb_block,
                    source_number: bcb_block_number,
                    bundle: self,
                    primary_block: None,
                    bundle_data: data,
                },
                None,
            )?
            .plaintext
        else {
            return Err(bpsec::Error::DecryptionFailed.into());
        };
        Ok(Payload::Owned(data))
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
    pub fn parse(
        data: &[u8],
        f: impl FnMut(&Eid, bpsec::Context) -> Result<Option<bpsec::KeyMaterial>, bpsec::Error>,
    ) -> Result<Self, Error> {
        let mut keys = KeyCacheImpl::new(f);
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
            let (primary_block, canonical_primary_block, block_len) = blocks
                .parse::<(primary_block::PrimaryBlock, bool, usize)>()
                .map_field_err("Primary Block")?;

            let (mut bundle, e) = primary_block.into_bundle();
            if let Some(e) = e {
                return Ok(Self::Invalid(
                    bundle,
                    StatusReportReasonCode::BlockUnintelligible,
                    e,
                ));
            }

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

            // And now parse the blocks
            match bundle.parse_blocks(
                canonical,
                canonical_primary_block,
                blocks,
                block_start + block_len,
                data,
                &mut keys,
            ) {
                Ok((None, report_unsupported)) => Ok(Self::Valid(bundle, report_unsupported)),
                Ok((Some(new_data), report_unsupported)) => {
                    Ok(Self::Rewritten(bundle, new_data, report_unsupported))
                }
                Err(Error::Unsupported(n)) => Ok(Self::Invalid(
                    bundle,
                    StatusReportReasonCode::BlockUnsupported,
                    Error::Unsupported(n).into(),
                )),
                Err(e) => Ok(Self::Invalid(
                    bundle,
                    StatusReportReasonCode::BlockUnintelligible,
                    e.into(),
                )),
            }
        })
        .map(|v| v.0)
    }
}
