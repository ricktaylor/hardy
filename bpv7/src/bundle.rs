use super::*;
use base64::prelude::*;
use error::CaptureFieldErr;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

trait KeyCache {
    fn get<'a>(
        &'a mut self,
        source: &eid::Eid,
        context: bpsec::Context,
    ) -> Result<Option<&'a bpsec::KeyMaterial>, bpsec::Error>;
}

struct KeyCacheImpl<F>
where
    F: FnMut(&eid::Eid, bpsec::Context) -> Result<Option<bpsec::KeyMaterial>, bpsec::Error>,
{
    keys: HashMap<eid::Eid, HashMap<bpsec::Context, Option<bpsec::KeyMaterial>>>,
    f: F,
}

impl<F> KeyCacheImpl<F>
where
    F: FnMut(&eid::Eid, bpsec::Context) -> Result<Option<bpsec::KeyMaterial>, bpsec::Error>,
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
    F: FnMut(&eid::Eid, bpsec::Context) -> Result<Option<bpsec::KeyMaterial>, bpsec::Error>,
{
    fn get<'a>(
        &'a mut self,
        source: &eid::Eid,
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

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct FragmentInfo {
    pub offset: u64,
    pub total_len: u64,
}

pub mod id {
    use thiserror::Error;

    #[derive(Error, Debug)]
    pub enum Error {
        #[error("Bad bundle id key")]
        BadKey,

        #[error("Bad base64 encoding")]
        BadBase64(#[from] base64::DecodeError),

        #[error("Failed to decode {field}: {source}")]
        InvalidField {
            field: &'static str,
            source: Box<dyn std::error::Error + Send + Sync>,
        },

        #[error(transparent)]
        InvalidCBOR(#[from] hardy_cbor::decode::Error),
    }
}

trait CaptureFieldIdErr<T> {
    fn map_field_id_err(self, field: &'static str) -> Result<T, id::Error>;
}

impl<T, E: Into<Box<dyn std::error::Error + Send + Sync>>> CaptureFieldIdErr<T>
    for std::result::Result<T, E>
{
    fn map_field_id_err(self, field: &'static str) -> Result<T, id::Error> {
        self.map_err(|e| id::Error::InvalidField {
            field,
            source: e.into(),
        })
    }
}

#[derive(Default, Debug, Clone, Hash, PartialEq, Eq)]
pub struct Id {
    pub source: eid::Eid,
    pub timestamp: creation_timestamp::CreationTimestamp,
    pub fragment_info: Option<FragmentInfo>,
}

impl Id {
    pub fn from_key(k: &str) -> Result<Self, id::Error> {
        hardy_cbor::decode::parse_array(&BASE64_STANDARD_NO_PAD.decode(k)?, |array, _, _| {
            let s = Self {
                source: array.parse().map_field_id_err("source EID")?,
                timestamp: array.parse().map_field_id_err("creation timestamp")?,
                fragment_info: if let Some(4) = array.count() {
                    Some(FragmentInfo {
                        offset: array.parse().map_field_id_err("fragment offset")?,
                        total_len: array
                            .parse()
                            .map_field_id_err("total application data unit Length")?,
                    })
                } else {
                    None
                },
            };
            if array.end()?.is_none() {
                Err(id::Error::BadKey)
            } else {
                Ok(s)
            }
        })
        .map(|v| v.0)
    }

    pub fn to_key(&self) -> String {
        BASE64_STANDARD_NO_PAD.encode(if let Some(fragment_info) = &self.fragment_info {
            hardy_cbor::encode::emit_array(Some(4), |array| {
                array.emit(&self.source);
                array.emit(&self.timestamp);
                array.emit(fragment_info.offset);
                array.emit(fragment_info.total_len);
            })
        } else {
            hardy_cbor::encode::emit_array(Some(2), |array| {
                array.emit(&self.source);
                array.emit(&self.timestamp);
            })
        })
    }
}

#[derive(Default, Debug, Clone)]
pub struct Flags {
    pub is_fragment: bool,
    pub is_admin_record: bool,
    pub do_not_fragment: bool,
    pub app_ack_requested: bool,
    pub report_status_time: bool,
    pub receipt_report_requested: bool,
    pub forward_report_requested: bool,
    pub delivery_report_requested: bool,
    pub delete_report_requested: bool,
    pub unrecognised: u64,
}

impl From<u64> for Flags {
    fn from(value: u64) -> Self {
        let mut flags = Self {
            unrecognised: value & !((2 ^ 20) - 1),
            ..Default::default()
        };

        for b in 0..=20 {
            if value & (1 << b) != 0 {
                match b {
                    0 => flags.is_fragment = true,
                    1 => flags.is_admin_record = true,
                    2 => flags.do_not_fragment = true,
                    5 => flags.app_ack_requested = true,
                    6 => flags.report_status_time = true,
                    14 => flags.receipt_report_requested = true,
                    16 => flags.forward_report_requested = true,
                    17 => flags.delivery_report_requested = true,
                    18 => flags.delete_report_requested = true,
                    b => {
                        flags.unrecognised |= 1 << b;
                    }
                }
            }
        }
        flags
    }
}

impl From<&Flags> for u64 {
    fn from(value: &Flags) -> Self {
        let mut flags = value.unrecognised;
        if value.is_fragment {
            flags |= 1 << 0;
        }
        if value.is_admin_record {
            flags |= 1 << 1;
        }
        if value.do_not_fragment {
            flags |= 1 << 2;
        }
        if value.app_ack_requested {
            flags |= 1 << 5;
        }
        if value.report_status_time {
            flags |= 1 << 6;
        }
        if value.receipt_report_requested {
            flags |= 1 << 14;
        }
        if value.forward_report_requested {
            flags |= 1 << 16;
        }
        if value.delivery_report_requested {
            flags |= 1 << 17;
        }
        if value.delete_report_requested {
            flags |= 1 << 18;
        }
        flags
    }
}

impl hardy_cbor::encode::ToCbor for &Flags {
    fn to_cbor(self, encoder: &mut hardy_cbor::encode::Encoder) {
        encoder.emit(u64::from(self))
    }
}

impl hardy_cbor::decode::FromCbor for Flags {
    type Error = hardy_cbor::decode::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        hardy_cbor::decode::try_parse::<(u64, bool, usize)>(data)
            .map(|o| o.map(|(value, shortest, len)| (value.into(), shortest, len)))
    }
}

#[derive(Default, Debug, Clone)]
pub struct Bundle {
    // From Primary Block
    pub id: Id,
    pub flags: Flags,
    pub crc_type: crc::CrcType,
    pub destination: eid::Eid,
    pub report_to: eid::Eid,
    pub lifetime: std::time::Duration,

    // Unpacked from extension blocks
    pub previous_node: Option<eid::Eid>,
    pub age: Option<std::time::Duration>,
    pub hop_count: Option<hop_info::HopInfo>,

    // The extension blocks
    pub blocks: std::collections::HashMap<u64, block::Block>,
}

impl Bundle {
    pub(crate) fn emit_primary_block(&mut self, array: &mut hardy_cbor::encode::Array) {
        let data_start = array.offset();
        let data = primary_block::PrimaryBlock::emit(self);
        let payload_len = data.len();
        array.emit_raw(data);

        self.blocks.insert(
            0,
            block::Block {
                block_type: block::Type::Primary,
                flags: block::Flags {
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
    ) -> Result<(&block::Block, T, bool), Error>
    where
        T: hardy_cbor::decode::FromCbor<Error: From<hardy_cbor::decode::Error> + Into<Error>>,
    {
        if let Some((block_data, can_encrypt)) = decrypted_data {
            match hardy_cbor::decode::parse::<(T, bool, usize)>(block_data)
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
            hardy_cbor::decode::parse_value(block.payload(source_data), |v, _, _| match v {
                hardy_cbor::decode::Value::Bytes(data) => {
                    hardy_cbor::decode::parse::<(T, bool, usize)>(data)
                        .map(|(v, s, len)| (v, s && len == data.len()))
                }
                hardy_cbor::decode::Value::ByteStream(data) => {
                    hardy_cbor::decode::parse::<(T, bool, usize)>(&data.iter().fold(
                        Vec::new(),
                        |mut data, d| {
                            data.extend(*d);
                            data
                        },
                    ))
                    .map(|(v, s, len)| (v, s && len == data.len()))
                }
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
        blocks: &mut hardy_cbor::decode::Array,
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
                block::Type::Primary => unreachable!(),
                block::Type::Payload
                | block::Type::PreviousNode
                | block::Type::BundleAge
                | block::Type::HopCount => {
                    // Confirm no duplicates
                    if blocks_to_check
                        .insert(block.block.block_type, block.number)
                        .is_some()
                    {
                        return Err(Error::DuplicateBlocks(block.block.block_type));
                    }
                }
                block::Type::BlockIntegrity => {
                    bibs_to_check.insert(block.number);
                }
                block::Type::BlockSecurity => {
                    bcbs_to_check.push(block.number);
                }
                block::Type::Unrecognised(_) => {
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
            .remove(&block::Type::Payload)
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
        let primary_block_data = if canonical_primary_block {
            Cow::Borrowed(
                self.blocks
                    .get(&0)
                    .expect("Missing primary block!")
                    .payload(source_data),
            )
        } else {
            Cow::Owned(primary_block::PrimaryBlock::emit(self))
        };

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
                    block::Type::BlockSecurity | block::Type::Primary => {
                        return Err(bpsec::Error::InvalidBCBTarget.into());
                    }
                    block::Type::Payload => {
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
                        target_payload: target_block.payload(source_data),
                        source: bcb_block,
                        source_number: bcb_block_number,
                        primary_block: &primary_block_data,
                    },
                    None,
                )?;

                if !blocks_to_remove.contains(target_number) {
                    match (target_block.block_type, r.plaintext) {
                        (block::Type::PreviousNode | block::Type::HopCount, Some(block_data)) => {
                            // We will always replace these blocks when forwarded
                            decrypted_data.insert(*target_number, (block_data, true));

                            // We will rewrite the block unencrypted for now
                            noncanonical_blocks.insert(*target_number, true);

                            // And not re-encrypt
                            targets_to_drop.insert(*target_number);
                        }
                        (block::Type::BlockIntegrity, None) => {
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
                block::Type::PreviousNode => {
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
                block::Type::BundleAge => {
                    let (_, v, s) = self
                        .parse_payload::<u64>(
                            &block_number,
                            decrypted_data.get(&block_number),
                            source_data,
                        )
                        .map_field_err("Bundle Age Block")?;
                    self.age = Some(std::time::Duration::from_millis(v));
                    s
                }
                block::Type::HopCount => {
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
                if let block::Type::BlockSecurity | block::Type::BlockIntegrity =
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
                        target_payload: target_block.payload(source_data),
                        source: bib_block,
                        source_number: bib_block_number,
                        primary_block: &primary_block_data,
                    },
                    payload_data,
                )?;

                if !blocks_to_remove.contains(target_number) {
                    if let block::Type::PreviousNode | block::Type::HopCount =
                        target_block.block_type
                    {
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
        if let crc::CrcType::None = self.crc_type
            && protects_primary_block.is_empty()
        {
            return Err(Error::MissingIntegrityCheck);
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
                    block::Type::PreviousNode => {
                        new_payloads.insert(
                            *block_number,
                            hardy_cbor::encode::emit(self.previous_node.as_ref().unwrap()).into(),
                        );
                        false
                    }
                    block::Type::BundleAge => {
                        new_payloads.insert(
                            *block_number,
                            hardy_cbor::encode::emit(self.age.unwrap().as_millis() as u64).into(),
                        );
                        false
                    }
                    block::Type::HopCount => {
                        new_payloads.insert(
                            *block_number,
                            hardy_cbor::encode::emit(self.hop_count.as_ref().unwrap()).into(),
                        );
                        false
                    }
                    block::Type::BlockIntegrity | block::Type::BlockSecurity => {
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
                            target_payload: new_payloads
                                .get(target_number)
                                .map_or(target_block.payload(source_data), |d| d),
                            source: bib_block,
                            source_number: bib_block_number,
                            primary_block: &primary_block_data,
                        },
                        Some(payload_data),
                    )?;
                }
            }

            noncanonical_blocks.remove(&bib_block_number);
            new_payloads.insert(bib_block_number, hardy_cbor::encode::emit(bib).into());
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
                            target_payload: new_payloads
                                .get(target_number)
                                .map_or(target_block.payload(source_data), |d| d),
                            source: bcb_block,
                            source_number: bcb_block_number,
                            primary_block: &primary_block_data,
                        },
                        Some(payload_data),
                    )?;
                    new_payloads.insert(*target_number, new_data);
                }
            }

            noncanonical_blocks.remove(&bcb_block_number);
            new_payloads.insert(bcb_block_number, hardy_cbor::encode::emit(bcb).into());
        }

        let new_data = hardy_cbor::encode::emit_array(None, |a| {
            // Emit primary
            let block = self.blocks.get_mut(&0).expect("Missing primary block!");
            block.data_start = a.offset();
            block.data_len = primary_block_data.len();
            block.payload_len = block.data_len;
            a.emit_raw_slice(&primary_block_data);

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
        mut f: impl FnMut(&eid::Eid, bpsec::Context) -> Result<Option<bpsec::KeyMaterial>, bpsec::Error>,
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
                    target_payload: payload_block.payload(data),
                    source: bcb_block,
                    source_number: bcb_block_number,
                    primary_block: self
                        .blocks
                        .get(&0)
                        .expect("Missing primary block!")
                        .payload(data),
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
        status_report::ReasonCode,
        Box<dyn std::error::Error + Send + Sync>,
    ),
}

impl ValidBundle {
    pub fn parse(
        data: &[u8],
        f: impl FnMut(&eid::Eid, bpsec::Context) -> Result<Option<bpsec::KeyMaterial>, bpsec::Error>,
    ) -> Result<Self, Error> {
        let mut keys = KeyCacheImpl::new(f);
        hardy_cbor::decode::parse_array(data, |blocks, mut canonical, tags| {
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
                    status_report::ReasonCode::BlockUnintelligible,
                    e,
                ));
            }

            // Add a block 0
            bundle.blocks.insert(
                0,
                block::Block {
                    block_type: block::Type::Primary,
                    flags: block::Flags {
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
                    status_report::ReasonCode::BlockUnsupported,
                    Error::Unsupported(n).into(),
                )),
                Err(e) => Ok(Self::Invalid(
                    bundle,
                    status_report::ReasonCode::BlockUnintelligible,
                    e.into(),
                )),
            }
        })
        .map(|v| v.0)
    }
}
