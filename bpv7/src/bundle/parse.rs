/*!
This module contains the internal logic for parsing a BPv7 bundle from a byte slice.
It handles the entire parsing process, including validating the primary block, iterating
through extension blocks, handling BPSec (BIB and BCB) operations, and dealing with
canonicalization issues.
*/

use super::*;
use error::CaptureFieldErr;
use thiserror::Error;

/// A state machine for parsing the blocks of a bundle.
///
/// This struct holds the state required to parse all blocks, handle inter-block
/// dependencies (like BPSec), and manage data that might be decrypted or rewritten
/// for canonicalization.
#[derive(Debug, Default)]
struct BlockParse<'a> {
    /// The raw byte data of the entire bundle.
    source_data: &'a [u8],
    /// The collection of blocks parsed so far, keyed by block number.
    blocks: HashMap<u64, block::Block>,
    /// Data that has been decrypted from a BCB.
    decrypted_data: HashMap<u64, zeroize::Zeroizing<Box<[u8]>>>,
    /// Blocks that were not in canonical CBOR form and have been rewritten.
    noncanonical_blocks: HashMap<u64, Option<Box<[u8]>>>,
    /// A map of known block types to their block numbers, used for validation.
    blocks_to_check: HashMap<block::Type, u64>,
    /// A set of BIBs that need to be checked after all blocks are parsed.
    bibs_to_check: HashSet<u64>,
    /// A set of blocks that are marked for removal (e.g., unsupported blocks).
    blocks_to_remove: HashSet<u64>,
    /// A map of BCB block numbers to their parsed operation sets.
    bcbs: HashMap<u64, bpsec::bcb::OperationSet>,
    /// A set of security blocks (BIB or BCB) that protect the primary block.
    protects_primary_block: HashSet<u64>,
}

impl<'a> bpsec::BlockSet<'a> for BlockParse<'a> {
    fn block(&'a self, block_number: u64) -> Option<&'a block::Block> {
        self.blocks.get(&block_number)
    }

    fn block_payload(&self, block_number: u64) -> Option<&[u8]> {
        if let Some(b) = self.decrypted_data.get(&block_number) {
            Some(b.as_ref())
        } else if let Some(Some(b)) = self.noncanonical_blocks.get(&block_number) {
            Some(b.as_ref())
        } else {
            Some(&self.source_data[self.block(block_number)?.payload()])
        }
    }
}

impl<'a> BlockParse<'a> {
    /// Creates a new `BlockParse` state for a given bundle data slice.
    fn new(source_data: &'a [u8]) -> Self {
        Self {
            source_data,
            ..Default::default()
        }
    }

    /// Parses the payload of a specific block into a given type `T`.
    fn parse_payload<T>(&'a self, block_number: u64) -> Result<(T, bool), Error>
    where
        T: hardy_cbor::decode::FromCbor<Error: From<hardy_cbor::decode::Error> + Into<Error>>,
    {
        let payload = <Self as bpsec::BlockSet>::block_payload(self, block_number)
            .expect("Missing block payload!");

        hardy_cbor::decode::parse::<(T, bool, usize)>(payload)
            .map(|(v, s, len)| (v, s && len == payload.len()))
            .map_err(Into::into)
    }

    /// Parses all extension blocks in the bundle.
    /// This is the first pass over the blocks, where they are identified and basic
    /// validation is performed.
    fn parse_blocks(
        &mut self,
        bundle: &Bundle,
        block_array: &mut hardy_cbor::decode::Array,
    ) -> Result<bool, Error> {
        let mut last_block_number = 0;
        let mut report_unsupported = false;
        let mut offset = block_array.offset();

        while let Some((mut block, canonical)) =
            block_array.try_parse::<(block::BlockWithNumber, bool)>()?
        {
            // Adjust block extent to be relative to source_data
            block.block.extent = block.block.extent.start + offset..block.block.extent.end + offset;
            offset = block_array.offset();

            // Check the block
            if (bundle.flags.is_admin_record || bundle.id.source == eid::Eid::Null)
                && block.block.flags.report_on_failure
            {
                return Err(Error::InvalidFlags);
            }

            let mut remove = false;
            match block.block.block_type {
                block::Type::Primary => unreachable!(),
                block::Type::Payload
                | block::Type::PreviousNode
                | block::Type::BundleAge
                | block::Type::HopCount => {
                    // Confirm no duplicates
                    if self
                        .blocks_to_check
                        .insert(block.block.block_type, block.number)
                        .is_some()
                    {
                        return Err(Error::DuplicateBlocks(block.block.block_type));
                    }
                }
                block::Type::BlockIntegrity => {
                    // We defer BIB checking till after BCB unpacking
                    self.bibs_to_check.insert(block.number);
                }
                block::Type::BlockSecurity => {
                    if block.block.flags.delete_block_on_failure {
                        return Err(bpsec::Error::BCBDeleteFlag.into());
                    }

                    // Get the block data (not in the maps yet)
                    let block_data = if let Some(payload) = &block.payload {
                        payload.as_ref()
                    } else {
                        &self.source_data[block.block.payload()]
                    };

                    // Parse the BCB
                    let (bcb, canonical) =
                        hardy_cbor::decode::parse::<(bpsec::bcb::OperationSet, bool, usize)>(
                            block_data,
                        )
                        .map(|(v, s, len)| (v, s && len == block_data.len()))
                        .map_field_err("BPSec confidentiality extension block")?;

                    if bcb.is_unsupported() {
                        if block.block.flags.delete_bundle_on_failure {
                            return Err(Error::Unsupported(block.number));
                        }

                        if block.block.flags.delete_block_on_failure {
                            return Err(bpsec::Error::BCBDeleteFlag.into());
                        }

                        if block.block.flags.report_on_failure {
                            report_unsupported = true;
                        }
                    }

                    if !canonical {
                        // Rewrite the BCB canonically
                        block.payload = Some(hardy_cbor::encode::emit(&bcb).0.into());
                    }

                    self.bcbs.insert(block.number, bcb);
                }
                block::Type::Unrecognised(_) => {
                    if block.block.flags.delete_bundle_on_failure {
                        return Err(Error::Unsupported(block.number));
                    }

                    if block.block.flags.report_on_failure {
                        report_unsupported = true;
                    }

                    if block.block.flags.delete_block_on_failure {
                        remove = true;
                    }
                }
            }

            if self.blocks.insert(block.number, block.block).is_some() {
                return Err(Error::DuplicateBlockNumber(block.number));
            }

            if remove {
                self.blocks_to_remove.insert(block.number);
            } else if block.payload.is_some() || !canonical {
                self.noncanonical_blocks.insert(block.number, block.payload);
            }

            last_block_number = block.number;
        }

        // Check the last block is the payload
        if self
            .blocks_to_check
            .remove(&block::Type::Payload)
            .ok_or(Error::MissingPayload)?
            != last_block_number
        {
            return Err(Error::PayloadNotFinal);
        }

        // Check for spurious extra data
        if block_array.offset() != self.source_data.len() {
            return Err(Error::AdditionalData);
        }

        Ok(report_unsupported)
    }

    /// Parses and processes all Block Confidentiality Blocks (BCBs).
    /// This involves decrypting the payloads of their target blocks.
    fn parse_bcbs(&mut self, key_f: &impl bpsec::key::KeyStore) -> Result<(), Error> {
        let mut decrypted_data = HashMap::new();
        let mut bcb_targets = HashMap::new();
        for (bcb_block_number, bcb) in &self.bcbs {
            let bcb_block = self
                .blocks
                .get(bcb_block_number)
                .expect("Missing BCB block!");

            // Check targets
            for (target_number, op) in &bcb.operations {
                if bcb_targets
                    .insert(*target_number, bcb_block_number)
                    .is_some()
                {
                    return Err(bpsec::Error::DuplicateOpTarget.into());
                }

                let target_block = &self
                    .blocks
                    .get(target_number)
                    .ok_or(bpsec::Error::MissingSecurityTarget)?;

                if match target_block.block_type {
                    block::Type::BlockSecurity | block::Type::Primary => {
                        return Err(bpsec::Error::InvalidBCBTarget.into());
                    }
                    block::Type::Payload => {
                        // Check flags
                        if !bcb_block.flags.must_replicate {
                            return Err(bpsec::Error::BCBMustReplicate.into());
                        }
                        if op.protects_primary_block() {
                            self.protects_primary_block.insert(*bcb_block_number);
                        }
                        false
                    }
                    block::Type::PreviousNode
                    | block::Type::BundleAge
                    | block::Type::HopCount
                    | block::Type::BlockIntegrity => {
                        if self.blocks_to_remove.contains(target_number) {
                            false
                        } else {
                            if op.protects_primary_block() {
                                self.protects_primary_block.insert(*bcb_block_number);
                            }
                            true
                        }
                    }
                    _ => {
                        if !self.blocks_to_remove.contains(target_number)
                            && op.protects_primary_block()
                        {
                            self.protects_primary_block.insert(*bcb_block_number);
                        }
                        false
                    }
                } {
                    // Try to decrypt if we have keys
                    if let Some(plaintext) = op.decrypt_any(
                        key_f,
                        bpsec::bcb::OperationArgs {
                            bpsec_source: &bcb.source,
                            target: *target_number,
                            source: *bcb_block_number,
                            blocks: self,
                        },
                    )? {
                        decrypted_data.insert(*target_number, plaintext);
                    } else if target_block.block_type == block::Type::BlockIntegrity {
                        // We can't decrypt the BIB, therefore we cannot check the BIB
                        self.bibs_to_check.remove(target_number);
                    } else {
                        // We can't decrypt the block, therefore we cannot check it
                        self.blocks_to_check.remove(&target_block.block_type);
                    }
                }
            }
        }

        // Mark all blocks that are BCB targets (we have to delay this because of borrow rules)
        for (target_block, bcb_block_number) in bcb_targets {
            self.blocks
                .get_mut(&target_block)
                .expect("Missing BCB target!")
                .bcb = Some(*bcb_block_number);
        }

        Ok(())
    }

    /// Parses the content of known extension blocks (like HopCount, BundleAge, etc.).
    fn check_blocks(&mut self, bundle: &mut Bundle) -> Result<(), Error> {
        for (block_type, block_number) in core::mem::take(&mut self.blocks_to_check) {
            if let Some(payload) = match block_type {
                block::Type::PreviousNode => {
                    let (v, s) = self
                        .parse_payload(block_number)
                        .map_field_err("Previous Node Block")?;
                    let r = (!s).then(|| hardy_cbor::encode::emit(&v).0.into());
                    bundle.previous_node = Some(v);
                    r
                }
                block::Type::BundleAge => {
                    let (v, s) = self
                        .parse_payload(block_number)
                        .map_field_err("Bundle Age Block")?;
                    bundle.age = Some(core::time::Duration::from_millis(v));
                    (!s).then(|| hardy_cbor::encode::emit(&v).0.into())
                }
                block::Type::HopCount => {
                    let (v, s) = self
                        .parse_payload(block_number)
                        .map_field_err("Hop Count Block")?;
                    let r = (!s).then(|| hardy_cbor::encode::emit(&v).0.into());
                    bundle.hop_count = Some(v);
                    r
                }
                _ => unreachable!(),
            } {
                self.noncanonical_blocks.insert(block_number, Some(payload));
            }
        }

        Ok(())
    }

    /// Parses and validates all Block Integrity Blocks (BIBs).
    fn parse_bibs(&mut self) -> Result<bool, Error> {
        let mut report_unsupported = false;
        let mut bib_targets = HashMap::new();
        for bib_block_number in core::mem::take(&mut self.bibs_to_check) {
            let bib_block = self.blocks.get(&bib_block_number).expect("Missing BIB!");

            let (mut bib, canonical) = self
                .parse_payload::<bpsec::bib::OperationSet>(bib_block_number)
                .map_field_err("BPSec integrity extension block")?;

            if bib.is_unsupported() {
                if bib_block.flags.delete_bundle_on_failure {
                    return Err(Error::Unsupported(bib_block_number));
                }

                if bib_block.flags.report_on_failure {
                    report_unsupported = true;
                }

                if bib_block.flags.delete_block_on_failure {
                    self.noncanonical_blocks.remove(&bib_block_number);
                    self.blocks_to_remove.insert(bib_block_number);
                    continue;
                }
            }

            let bcb = bib_block.bcb.and_then(|b| self.bcbs.get(&b));

            // Check targets
            for (target_number, op) in &bib.operations {
                if bib_targets
                    .insert(*target_number, bib_block_number)
                    .is_some()
                {
                    return Err(bpsec::Error::DuplicateOpTarget.into());
                }

                let target_block = &self
                    .blocks
                    .get(target_number)
                    .ok_or(bpsec::Error::MissingSecurityTarget)?;

                // Verify BIB target
                if matches!(
                    target_block.block_type,
                    block::Type::BlockSecurity | block::Type::BlockIntegrity
                ) {
                    return Err(bpsec::Error::InvalidBIBTarget.into());
                }

                if let Some(bcb) = bcb {
                    // Check we share a target with our BCB
                    if !bcb.operations.contains_key(target_number) {
                        return Err(bpsec::Error::BCBMustShareTarget.into());
                    }
                }

                if target_number == &0
                    || (!self.blocks_to_remove.contains(target_number)
                        && op.protects_primary_block())
                {
                    self.protects_primary_block.insert(bib_block_number);
                }
            }

            // Remove targets scheduled for removal
            let old_len = bib.operations.len();
            bib.operations
                .retain(|k, _| !self.blocks_to_remove.contains(k));
            if bib.operations.is_empty() {
                self.noncanonical_blocks.remove(&bib_block_number);
                self.protects_primary_block.remove(&bib_block_number);
                self.blocks_to_remove.insert(bib_block_number);
            } else if !canonical || bib.operations.len() != old_len {
                self.noncanonical_blocks.insert(
                    bib_block_number,
                    Some(hardy_cbor::encode::emit(&bib).0.into()),
                );
            }
        }

        // Mark all blocks that are BIB targets (we have to delay this because of borrow rules)
        for (target_block, bib_block_number) in bib_targets {
            self.blocks
                .get_mut(&target_block)
                .expect("Missing BIB target!")
                .bib = Some(bib_block_number);
        }

        Ok(report_unsupported)
    }

    /// Reduces the set of BCBs by removing targets that are scheduled for removal.
    fn reduce_bcbs(&mut self) {
        // Remove BCB targets scheduled for removal
        for (bcb_block_number, mut bcb) in core::mem::take(&mut self.bcbs) {
            let old_len = bcb.operations.len();
            bcb.operations
                .retain(|k, _| !self.blocks_to_remove.contains(k));
            if bcb.operations.is_empty() {
                self.noncanonical_blocks.remove(&bcb_block_number);
                self.protects_primary_block.remove(&bcb_block_number);
                self.blocks_to_remove.insert(bcb_block_number);
            } else if bcb.operations.len() != old_len {
                self.noncanonical_blocks.insert(
                    bcb_block_number,
                    Some(hardy_cbor::encode::emit(&bcb).0.into()),
                );
            }
        }
    }

    /// Emits a single block into a CBOR array, handling canonical and non-canonical data.
    fn emit_block(
        &mut self,
        block: &mut block::Block,
        block_number: u64,
        array: &mut hardy_cbor::encode::Array,
    ) -> Result<(), Error> {
        match self.noncanonical_blocks.remove(&block_number) {
            Some(Some(payload)) => block.emit(block_number, &payload, array),
            Some(None) => block.emit(block_number, &self.source_data[block.payload()], array),
            None => {
                block.r#move(self.source_data, array);
                Ok(())
            }
        }
    }

    /// Rewrites the entire bundle if any blocks were non-canonical or removed.
    /// Returns `None` if no rewrite was necessary.
    fn rewrite(mut self, bundle: &mut Bundle) -> Option<(Box<[u8]>, bool)> {
        // If we have nothing to rewrite, get out now
        if self.noncanonical_blocks.is_empty() && self.blocks_to_remove.is_empty() {
            bundle.blocks = self.blocks;
            return None;
        }

        let non_canonical = !self.noncanonical_blocks.is_empty();

        // Drop any blocks marked for removal
        self.blocks
            .retain(|block_number, _| !self.blocks_to_remove.contains(block_number));

        // Write out the new bundle
        Some((
            hardy_cbor::encode::emit_array(None, |block_array| {
                // Primary block first
                let mut primary_block = self.blocks.remove(&0).expect("Missing primary block!");

                primary_block.extent =
                    if let Some(Some(payload)) = self.noncanonical_blocks.remove(&0) {
                        block_array.emit(&hardy_cbor::encode::RawOwned::new(payload))
                    } else {
                        block_array.emit(&hardy_cbor::encode::Raw(
                            &self.source_data[primary_block.extent],
                        ))
                    };
                primary_block.data = primary_block.extent.clone();
                bundle.blocks.insert(0, primary_block);

                // Stash payload block
                let mut payload_block = self.blocks.remove(&1).expect("Missing payload block!");

                // Emit all blocks
                for (block_number, mut block) in core::mem::take(&mut self.blocks) {
                    self.emit_block(&mut block, block_number, block_array)
                        .expect("Failed to emit block");
                    bundle.blocks.insert(block_number, block);
                }

                // And final payload block
                self.emit_block(&mut payload_block, 1, block_array)
                    .expect("Failed to emit payload block");
                bundle.blocks.insert(1, payload_block);
            })
            .into(),
            non_canonical,
        ))
    }
}

/// The main parsing function for the bundle's extension blocks.
#[allow(clippy::type_complexity)]
fn parse_blocks(
    bundle: &mut Bundle,
    canonical_bundle: bool,
    block_array: &mut hardy_cbor::decode::Array,
    source_data: &[u8],
    key_f: &impl bpsec::key::KeyStore,
) -> Result<(Option<(Box<[u8]>, bool)>, bool), Error> {
    let mut parser = BlockParse::new(source_data);

    // Steal the primary block, we put it back later
    parser
        .blocks
        .insert(0, bundle.blocks.remove(&0).expect("No primary block?!"));

    // Rewrite primary block if the bundle or primary block aren't canonical
    if !canonical_bundle {
        parser
            .noncanonical_blocks
            .insert(0, Some(primary_block::PrimaryBlock::emit(bundle)?.into()));
    }

    // Parse the blocks
    let mut report_unsupported = parser.parse_blocks(bundle, block_array)?;

    // Decrypt all relevant BCB targets first
    parser.parse_bcbs(key_f)?;

    // Now parse all the non-BIBs we need to check
    parser.check_blocks(bundle)?;

    // Check bundle age exists if needed
    if bundle.age.is_none() && !bundle.id.timestamp.is_clocked() {
        return Err(Error::MissingBundleAge);
    }

    // Now parse all BIBs
    report_unsupported = parser.parse_bibs()? && report_unsupported;

    // We are done with all decrypted content
    parser.decrypted_data.clear();

    // Reduce BCB targets scheduled for removal
    parser.reduce_bcbs();

    // Check we have at least some primary block protection
    if let crc::CrcType::None = bundle.crc_type
        && parser.protects_primary_block.is_empty()
    {
        return Err(Error::MissingIntegrityCheck);
    }

    // Now rewrite blocks (if required)
    Ok((parser.rewrite(bundle), report_unsupported))
}

/// An intermediate error type used during parsing to distinguish between
/// recoverable and non-recoverable errors.
#[derive(Error, Debug)]
enum ValidError {
    #[error("An invalid bundle")]
    Invalid(Bundle, status_report::ReasonCode, Error),

    #[error(transparent)]
    InvalidCBOR(#[from] hardy_cbor::decode::Error),

    #[error(transparent)]
    Wrapped(#[from] Error),
}

impl ValidBundle {
    /// Parses a byte slice into a `ValidBundle`.
    /// This is the main entry point for bundle parsing.
    // Bouncing via ValidError allows us to avoid the array completeness check when a semantic error occurs
    // so we don't shadow the semantic error by exiting the loop early and therefore reporting 'additional items'
    pub fn parse(data: &[u8], key_f: &impl bpsec::key::KeyStore) -> Result<Self, Error> {
        match hardy_cbor::decode::parse_array(data, |a, shortest, tags| {
            Self::parse_inner(data, key_f, a, shortest, tags)
        }) {
            Ok((Self::Valid(bundle, _) | Self::Rewritten(bundle, _, _, _), len))
                if len != data.len() =>
            {
                Ok(Self::Invalid(
                    bundle,
                    status_report::ReasonCode::BlockUnintelligible,
                    Error::AdditionalData,
                ))
            }
            Ok((b, _)) => Ok(b),
            Err(ValidError::Invalid(bundle, reason, e)) => Ok(Self::Invalid(bundle, reason, e)),
            Err(ValidError::InvalidCBOR(e)) => Err(e.into()),
            Err(ValidError::Wrapped(e)) => Err(e),
        }
    }

    /// The inner parsing logic, called by `parse`.
    /// This function is responsible for parsing the primary block and then handing off
    /// to `parse_blocks` for the extension blocks.
    fn parse_inner(
        data: &[u8],
        key_f: &impl bpsec::key::KeyStore,
        block_array: &mut hardy_cbor::decode::Array,
        mut canonical: bool,
        tags: &[u64],
    ) -> Result<Self, ValidError> {
        // Check for shortest/correct form
        canonical = canonical && !block_array.is_definite();
        if canonical {
            // TODO: POLICY CHECK
            // Appendix B of RFC9171
            let mut seen_55799 = false;
            for tag in tags {
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
        let block_start = block_array.offset();
        let primary_block = block_array
            .parse::<(primary_block::PrimaryBlock, bool)>()
            .map(|(v, s)| {
                canonical = canonical && s;
                v
            })
            .map_field_err("Primary Block")?;

        let (mut bundle, e) = primary_block.into_bundle(block_start..block_array.offset());
        if let Some(e) = e {
            block_array.skip_to_end(16)?;
            return Ok(Self::Invalid(
                bundle,
                status_report::ReasonCode::BlockUnintelligible,
                Error::InvalidField {
                    field: "Primary Block",
                    source: e.into(),
                },
            ));
        }

        // And now parse the blocks
        match parse_blocks(&mut bundle, canonical, block_array, data, key_f) {
            Ok((None, report_unsupported)) => Ok(Self::Valid(bundle, report_unsupported)),
            Ok((Some((new_data, non_canonical)), report_unsupported)) => Ok(Self::Rewritten(
                bundle,
                new_data,
                report_unsupported,
                non_canonical,
            )),
            Err(Error::Unsupported(n)) => Err(ValidError::Invalid(
                bundle,
                status_report::ReasonCode::BlockUnsupported,
                Error::Unsupported(n),
            )),
            Err(e) => Err(ValidError::Invalid(
                bundle,
                status_report::ReasonCode::BlockUnintelligible,
                e,
            )),
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use hex_literal::hex;

    #[test]
    fn tests() {
        // From Stephan Havermans testing
        assert!(matches!(
            ValidBundle::parse(
                &hex!(
                    "9f89071844018202820301820100820100821b000000b5998c982b011a000493e042c9f6850602182700458202820200850704010042183485010101004454455354ff"
                ),
                &bpsec::key::EmptyStore,
            ),
            Ok(ValidBundle::Invalid(_, _, Error::InvalidFlags))
        ));
    }
}
