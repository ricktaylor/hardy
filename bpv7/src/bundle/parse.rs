/*!
This module contains the internal logic for parsing a BPv7 bundle from a byte slice.
It handles the entire parsing process, including validating the primary block, iterating
through extension blocks, handling BPSec (BIB and BCB) operations, and dealing with
canonicalization issues.
*/

use super::*;
use error::CaptureFieldErr;
use smallvec::SmallVec;
use thiserror::Error;

/// Controls parsing behavior for different use cases.
#[derive(Debug)]
enum ParseMode {
    /// Preserve original encoding - no rewriting (ParsedBundle)
    Preserve,
    /// Canonicalize CBOR, validate structure, keep all blocks (CheckedBundle)
    Canonicalize,
    /// Full processing: rewrite, remove blocks, BPSec crypto (RewrittenBundle)
    Full,
}

/// A state machine for parsing the blocks of a bundle.
///
/// This struct holds the state required to parse all blocks, handle inter-block
/// dependencies (like BPSec), and manage data that might be decrypted or rewritten
/// for canonicalization.
#[derive(Debug)]
struct BlockParse<'a> {
    /// The raw byte data of the entire bundle.
    source_data: &'a [u8],
    /// The collection of blocks parsed so far, keyed by block number.
    blocks: HashMap<u64, block::Block>,
    /// Data that has been decrypted from a BCB.
    decrypted_data: HashMap<u64, zeroize::Zeroizing<Box<[u8]>>>,
    /// Blocks that were not in canonical CBOR form and have been rewritten.
    noncanonical_blocks: HashMap<u64, Option<Box<[u8]>>>,
    /// Track unique block types for duplicate detection (PreviousNode, BundleAge, HopCount, Payload).
    unique_blocks: HashSet<block::Type>,
    /// Blocks that need to be checked/parsed (BIBs and extension blocks).
    blocks_to_check: HashSet<u64>,
    /// A set of blocks that are marked for removal (e.g., unsupported blocks).
    blocks_to_remove: HashSet<u64>,
    /// A map of BCB block numbers to their parsed operation sets.
    bcbs: HashMap<u64, bpsec::bcb::OperationSet>,
    /// A map of BIB block numbers to their targets, for duplicate target detection.
    bib_targets: HashMap<u64, u64>,
    /// Parsing mode controlling rewriting and block removal behavior.
    mode: ParseMode,
}

impl<'a> bpsec::BlockSet<'a> for BlockParse<'a> {
    fn block(
        &'a self,
        block_number: u64,
    ) -> Option<(&'a block::Block, Option<block::Payload<'a>>)> {
        let block = self.blocks.get(&block_number)?;
        Some((
            block,
            if let Some(b) = self.decrypted_data.get(&block_number) {
                Some(b.as_ref())
            } else if let Some(Some(b)) = self.noncanonical_blocks.get(&block_number) {
                Some(b.as_ref())
            } else {
                block.payload(self.source_data)
            }
            .map(block::Payload::Borrowed),
        ))
    }
}

impl<'a> BlockParse<'a> {
    /// Creates a new `BlockParse` state for a given bundle data slice.
    ///
    /// Pre-allocates collections based on typical bundle sizes to avoid reallocations.
    /// Most bundles have 5-10 blocks, with 1-2 security blocks.
    fn new(source_data: &'a [u8], mode: ParseMode) -> Self {
        Self {
            source_data,
            mode,
            blocks: HashMap::with_capacity(8),
            decrypted_data: HashMap::with_capacity(4),
            noncanonical_blocks: HashMap::with_capacity(4),
            unique_blocks: HashSet::with_capacity(4), // PreviousNode, BundleAge, HopCount, Payload
            blocks_to_check: HashSet::with_capacity(8),
            blocks_to_remove: HashSet::with_capacity(4),
            bcbs: HashMap::with_capacity(2),
            bib_targets: HashMap::with_capacity(4),
        }
    }

    /// Parses the payload of a specific block into a given type `T`.
    fn parse_payload<T>(&'a self, block_number: u64) -> Result<(T, bool), Error>
    where
        T: hardy_cbor::decode::FromCbor<Error: From<hardy_cbor::decode::Error> + Into<Error>>,
    {
        let payload = if let Some(b) = self.decrypted_data.get(&block_number) {
            b.as_ref()
        } else if let Some(Some(b)) = self.noncanonical_blocks.get(&block_number) {
            b.as_ref()
        } else {
            self.blocks
                .get(&block_number)
                .and_then(|block| block.payload(self.source_data))
                .ok_or(Error::MissingPayload)?
        };

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
            if (bundle.flags.is_admin_record || bundle.id.source.is_null())
                && block.block.flags.report_on_failure
            {
                return Err(Error::InvalidFlags);
            }

            let mut unknown_block = false;
            match block.block.block_type {
                block::Type::Primary => unreachable!(),
                block::Type::Payload => {
                    // Confirm no duplicates
                    if !self.unique_blocks.insert(block.block.block_type) {
                        return Err(Error::DuplicateBlocks(block.block.block_type));
                    }
                }
                block::Type::PreviousNode | block::Type::BundleAge | block::Type::HopCount => {
                    // Confirm no duplicates
                    if !self.unique_blocks.insert(block.block.block_type) {
                        return Err(Error::DuplicateBlocks(block.block.block_type));
                    }
                    self.blocks_to_check.insert(block.number);
                }
                block::Type::BlockIntegrity => {
                    // Add BIBs to blocks_to_check for later parsing
                    self.blocks_to_check.insert(block.number);
                }
                block::Type::BlockSecurity => {
                    if block.block.flags.delete_block_on_failure {
                        return Err(bpsec::Error::BCBDeleteFlag.into());
                    }

                    // Get the block data (not in the maps yet)
                    let block_data = if let Some(payload) = &block.payload {
                        payload.as_ref()
                    } else {
                        &self.source_data[block.block.payload_range()]
                    };

                    // Parse the BCB
                    let (bcb, canonical) =
                        hardy_cbor::decode::parse::<(bpsec::bcb::OperationSet, bool, usize)>(
                            block_data,
                        )
                        .map(|(v, s, len)| (v, s && len == block_data.len()))
                        .map_field_err::<Error>("BPSec confidentiality extension block")?;

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
                        unknown_block = true;
                    }
                }
            }

            if self.blocks.insert(block.number, block.block).is_some() {
                return Err(Error::DuplicateBlockNumber(block.number));
            }

            if unknown_block && matches!(self.mode, ParseMode::Full) {
                self.blocks_to_remove.insert(block.number);
            } else if block.payload.is_some() || !canonical {
                self.noncanonical_blocks.insert(block.number, block.payload);
            }

            last_block_number = block.number;
        }

        // Check the last block is the payload
        if !self.unique_blocks.contains(&block::Type::Payload) {
            return Err(Error::MissingPayload);
        }
        // Payload block number is always 1
        if last_block_number != 1 {
            return Err(Error::PayloadNotFinal);
        }

        // Check for spurious extra data
        if block_array.offset() != self.source_data.len() {
            return Err(Error::AdditionalData);
        }

        Ok(report_unsupported)
    }

    /// Validates BCB targets and marks Block.bcb fields.
    /// This is done before any decryption so that Block.bcb values are available
    /// when the key provider is consulted.
    fn mark_bcb_targets(&mut self) -> Result<(), Error> {
        // Pre-allocate based on total number of BCB operations
        let total_targets: usize = self.bcbs.values().map(|bcb| bcb.operations.len()).sum();
        let mut bcb_targets = HashMap::with_capacity(total_targets);
        for (bcb_block_number, bcb) in &self.bcbs {
            let bcb_block = self
                .blocks
                .get(bcb_block_number)
                .expect("Missing BCB block!");

            // Check targets
            for target_number in bcb.operations.keys() {
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

    /// Decrypts BCB targets from blocks_to_check that match the filter.
    /// Called after mark_bcb_targets() so Block.bcb values are available.
    /// After successful decryption, immediately checks/parses the block.
    /// Blocks that cannot be decrypted (no valid key) are removed from the set.
    /// Returns (report_unsupported, has_undecrypted_bibs) where:
    /// - report_unsupported: true if any unsupported BIBs were found that need reporting
    /// - has_undecrypted_bibs: true if any BIBs couldn't be decrypted (for Unknown marking)
    fn decrypt_bcbs<K, F>(
        &mut self,
        key_source: &K,
        filter: F,
        bundle: &mut Bundle,
    ) -> Result<(bool, bool), Error>
    where
        K: bpsec::key::KeySource + ?Sized,
        F: Fn(block::Type) -> bool,
    {
        let mut report_unsupported = false;
        let mut has_undecrypted_bibs = false;
        let mut to_remove: SmallVec<[u64; 8]> = SmallVec::new();
        let mut to_check: SmallVec<[(u64, block::Type); 16]> = SmallVec::new();

        // Decrypt and immediately check each block
        for &target_number in &self.blocks_to_check {
            let target_block = self.blocks.get(&target_number).expect("Missing block!");
            let target_type = target_block.block_type;

            // Skip if block type doesn't match filter
            if !filter(target_type) {
                continue;
            }

            // Skip if not encrypted
            let Some(bcb_block_number) = target_block.bcb else {
                continue;
            };

            let bcb = self.bcbs.get(&bcb_block_number).expect("Missing BCB!");
            let op = bcb
                .operations
                .get(&target_number)
                .expect("Missing operation!");

            match op.decrypt(
                key_source,
                bpsec::bcb::OperationArgs {
                    bpsec_source: &bcb.source,
                    target: target_number,
                    source: bcb_block_number,
                    blocks: self,
                },
            ) {
                Ok(plaintext) => {
                    self.decrypted_data.insert(target_number, plaintext);
                    // Immediately check the decrypted block
                    to_check.push((target_number, target_type));
                }
                Err(bpsec::Error::NoKey) => {
                    // Can't decrypt, mark for removal so we don't try to parse it
                    to_remove.push(target_number);
                    // Track if this was a BIB we couldn't decrypt
                    if target_type == block::Type::BlockIntegrity {
                        has_undecrypted_bibs = true;
                    }
                }
                Err(e) => return Err(e.into()),
            }
        }

        // Remove blocks we couldn't decrypt
        for block_number in to_remove {
            self.blocks_to_check.remove(&block_number);
        }

        for (target_number, target_type) in to_check {
            if self.check_block(key_source, target_number, target_type, bundle)? {
                report_unsupported = true;
            }
        }

        Ok((report_unsupported, has_undecrypted_bibs))
    }

    /// Parses and validates all unencrypted blocks from blocks_to_check.
    /// Processed blocks are removed from the set.
    /// Returns true if any unsupported BIBs were found that need reporting.
    fn check_unencrypted_blocks<K>(
        &mut self,
        key_source: &K,
        bundle: &mut Bundle,
    ) -> Result<bool, Error>
    where
        K: bpsec::key::KeySource + ?Sized,
    {
        let mut report_unsupported = false;

        // Collect unencrypted blocks with their types
        let to_check: SmallVec<[(u64, block::Type); 16]> = self
            .blocks_to_check
            .iter()
            .filter_map(|&block_number| {
                self.blocks.get(&block_number).and_then(|b| {
                    if b.bcb.is_none() {
                        Some((block_number, b.block_type))
                    } else {
                        None
                    }
                })
            })
            .collect();

        for (block_number, block_type) in to_check {
            if self.check_block(key_source, block_number, block_type, bundle)? {
                report_unsupported = true;
            }
        }

        Ok(report_unsupported)
    }

    /// Parses and validates a single block.
    /// Returns true if an unsupported block was found that needs reporting.
    fn check_block<K>(
        &mut self,
        key_source: &K,
        block_number: u64,
        block_type: block::Type,
        bundle: &mut Bundle,
    ) -> Result<bool, Error>
    where
        K: bpsec::key::KeySource + ?Sized,
    {
        self.blocks_to_check.remove(&block_number);

        match block_type {
            block::Type::PreviousNode => {
                let (v, s) = self
                    .parse_payload(block_number)
                    .map_field_err::<Error>("Previous Node Block")?;
                if !s {
                    self.noncanonical_blocks
                        .insert(block_number, Some(hardy_cbor::encode::emit(&v).0.into()));
                }
                bundle.previous_node = Some(v);
                Ok(false)
            }
            block::Type::BundleAge => {
                let (v, s) = self
                    .parse_payload(block_number)
                    .map_field_err::<Error>("Bundle Age Block")?;
                if !s {
                    self.noncanonical_blocks
                        .insert(block_number, Some(hardy_cbor::encode::emit(&v).0.into()));
                }
                bundle.age = Some(core::time::Duration::from_millis(v));
                Ok(false)
            }
            block::Type::HopCount => {
                let (v, s) = self
                    .parse_payload(block_number)
                    .map_field_err::<Error>("Hop Count Block")?;
                if !s {
                    self.noncanonical_blocks
                        .insert(block_number, Some(hardy_cbor::encode::emit(&v).0.into()));
                }
                bundle.hop_count = Some(v);
                Ok(false)
            }
            block::Type::BlockIntegrity => self.check_bib(key_source, block_number),
            _ => unreachable!("Unexpected block type in check_block: {:?}", block_type),
        }
    }

    /// Parses and validates a single Block Integrity Block (BIB).
    /// If key_source provides a key for verification, verifies each target.
    /// Returns true if the BIB is unsupported and needs reporting.
    fn check_bib<K>(&mut self, key_source: &K, bib_block_number: u64) -> Result<bool, Error>
    where
        K: bpsec::key::KeySource + ?Sized,
    {
        let bib_block = self.blocks.get(&bib_block_number).expect("Missing BIB!");

        // Copy these values to release the borrow on self.blocks
        let bib_block_bcb = bib_block.bcb;

        let (mut bib, mut canonical) = self
            .parse_payload::<bpsec::bib::OperationSet>(bib_block_number)
            .map_field_err::<Error>("BPSec integrity extension block")?;

        let mut report_unsupported = false;

        if bib.is_unsupported() {
            if bib_block.flags.delete_bundle_on_failure {
                return Err(Error::Unsupported(bib_block_number));
            }

            if bib_block.flags.report_on_failure {
                report_unsupported = true;
            }

            if bib_block.flags.delete_block_on_failure && matches!(self.mode, ParseMode::Full) {
                self.noncanonical_blocks.remove(&bib_block_number);
                self.blocks_to_remove.insert(bib_block_number);
                return Ok(report_unsupported);
            }
        }

        // Check and mark targets
        for target_number in bib.operations.keys() {
            // Check for duplicate BIB targets
            if self
                .bib_targets
                .insert(*target_number, bib_block_number)
                .is_some()
            {
                return Err(bpsec::Error::DuplicateOpTarget.into());
            }

            let target_block = self
                .blocks
                .get_mut(target_number)
                .ok_or(bpsec::Error::MissingSecurityTarget)?;

            // Check BIB target is valid
            if matches!(
                target_block.block_type,
                block::Type::BlockSecurity | block::Type::BlockIntegrity
            ) {
                return Err(bpsec::Error::InvalidBIBTarget.into());
            }

            // If BIB target is the target of the BCB, then the BIB MUST also be a BCB target
            if target_block.bcb.is_some() && bib_block_bcb.is_none() {
                return Err(bpsec::Error::BIBMustBeEncrypted.into());
            }

            // Mark target immediately so decrypt callbacks see fresh BIB info
            target_block.bib = block::BibCoverage::Some(bib_block_number);
        }

        // RFC 9172 Section 3.8: "A BCB MUST NOT target a BIB unless it shares a
        // security target with that BIB."
        //
        // If this BIB is encrypted by a BCB, verify the BCB shares at least one
        // target with this BIB. However, this check only applies to contexts that
        // support sharing (e.g., future COSE-based contexts). BCB-AES-GCM cannot
        // share due to IV uniqueness requirements, so separate BCBs are expected.
        if let Some(bcb_block_num) = bib_block_bcb
            && let Some(bcb) = self.bcbs.get(&bcb_block_num)
            && bcb.can_share()
        {
            // The BCB should share at least one target with the BIB
            let shares_target = bib
                .operations
                .keys()
                .any(|bib_target| bcb.operations.contains_key(bib_target));
            if !shares_target {
                return Err(bpsec::Error::InvalidBCBTarget.into());
            }
        }

        // Verify each target block if key_source provides a key
        // NoKey means skip verification (policy decision), other errors are failures
        // Skip targets that are still encrypted (will be verified after decryption)
        if !bib.is_unsupported() {
            for (target_number, op) in &bib.operations {
                // Skip verification if target is still encrypted and not yet decrypted
                if let Some(target_block) = self.blocks.get(target_number)
                    && target_block.bcb.is_some()
                    && !self.decrypted_data.contains_key(target_number)
                {
                    continue;
                }

                match op.verify(
                    key_source,
                    bpsec::bib::OperationArgs {
                        bpsec_source: &bib.source,
                        target: *target_number,
                        source: bib_block_number,
                        blocks: self,
                    },
                ) {
                    Ok(()) => {}                    // Verified successfully
                    Err(bpsec::Error::NoKey) => {}  // No key provided, skip verification
                    Err(e) => return Err(e.into()), // Verification failed
                }
            }
        }

        if matches!(self.mode, ParseMode::Full) {
            // Remove targets scheduled for removal
            let old_len = bib.operations.len();
            bib.operations
                .retain(|k, _| !self.blocks_to_remove.contains(k));
            if bib.operations.is_empty() {
                self.noncanonical_blocks.remove(&bib_block_number);
                self.blocks_to_remove.insert(bib_block_number);
                return Ok(report_unsupported);
            }

            if bib.operations.len() != old_len {
                canonical = false;
            }
        }

        if !canonical {
            self.noncanonical_blocks.insert(
                bib_block_number,
                Some(hardy_cbor::encode::emit(&bib).0.into()),
            );
        }

        Ok(report_unsupported)
    }

    /// Marks all blocks with `bib == None` as `Maybe`.
    /// Called when there are encrypted BIBs that couldn't be decrypted,
    /// meaning we don't know which blocks they target.
    fn mark_bib_coverage_unknown(&mut self) {
        for block in self.blocks.values_mut() {
            // Only mark blocks that could be valid BIB targets
            // BIBs cannot target other security blocks (BIB or BCB)
            if !matches!(
                block.block_type,
                block::Type::BlockIntegrity | block::Type::BlockSecurity
            ) && matches!(block.bib, block::BibCoverage::None)
            {
                block.bib = block::BibCoverage::Maybe;
            }
        }
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
            Some(None) => block.emit(
                block_number,
                &self.source_data[block.payload_range()],
                array,
            ),
            None => {
                block.copy_whole(self.source_data, array);
                Ok(())
            }
        }
    }

    /// Rewrites the entire bundle if any blocks were non-canonical or removed.
    /// Returns `None` if no rewrite was necessary.
    #[allow(clippy::type_complexity)]
    fn finish(mut self, bundle: &mut Bundle) -> Result<(Option<Box<[u8]>>, bool), Error> {
        // Preserve mode: never rewrite
        if matches!(self.mode, ParseMode::Preserve) {
            bundle.blocks = self.blocks;
            return Ok((None, !self.noncanonical_blocks.is_empty()));
        }

        // Canonicalize/Full: check if we need to rewrite
        if self.noncanonical_blocks.is_empty() && self.blocks_to_remove.is_empty() {
            bundle.blocks = self.blocks;
            return Ok((None, false));
        }

        let non_canonical = !self.noncanonical_blocks.is_empty();

        // Drop any blocks marked for removal
        self.blocks
            .retain(|block_number, _| !self.blocks_to_remove.contains(block_number));

        // Write out the new bundle
        Ok((
            Some(
                hardy_cbor::encode::try_emit_array(None, |block_array| {
                    // Primary block first
                    let mut primary_block = self.blocks.remove(&0).expect("Missing primary block!");

                    primary_block.extent =
                        if let Some(Some(payload)) = self.noncanonical_blocks.remove(&0) {
                            block_array.emit(&hardy_cbor::encode::Raw(&payload))
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
                        self.emit_block(&mut block, block_number, block_array)?;
                        bundle.blocks.insert(block_number, block);
                    }

                    // And final payload block
                    self.emit_block(&mut payload_block, 1, block_array)?;
                    bundle.blocks.insert(1, payload_block);

                    Ok::<_, Error>(())
                })?
                .into(),
            ),
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
    key_source: &dyn bpsec::key::KeySource,
    mode: ParseMode,
) -> Result<(Option<Box<[u8]>>, bool, bool), Error> {
    let mut parser = BlockParse::new(source_data, mode);

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

    // Phase 1: Parse all blocks
    let mut report_unsupported = parser.parse_blocks(bundle, block_array)?;

    // Phase 2: Validate and mark BCB targets (no decryption yet)
    // This ensures Block.bcb values are set before key provider is consulted
    parser.mark_bcb_targets()?;

    // Phase 3: Check all unencrypted blocks (BIBs and extension blocks)
    if parser.check_unencrypted_blocks(key_source, bundle)? {
        report_unsupported = true;
    }

    // Phase 4: Decrypt and check BIBs first (so key provider sees BIB coverage for other blocks)
    let (unsupported, has_undecrypted_bibs) =
        parser.decrypt_bcbs(key_source, |t| t == block::Type::BlockIntegrity, bundle)?;
    if unsupported {
        report_unsupported = true;
    }

    // If any BIBs couldn't be decrypted, mark blocks with unknown BIB coverage
    if has_undecrypted_bibs {
        parser.mark_bib_coverage_unknown();
    }

    // Phase 5: Decrypt and check remaining blocks (key provider now sees BIB coverage)
    // BIBs already removed from blocks_to_check, so just process everything remaining
    let (unsupported, _) = parser.decrypt_bcbs(key_source, |_| true, bundle)?;
    if unsupported {
        report_unsupported = true;
    }

    // Check bundle age exists if needed
    if bundle.age.is_none() && !bundle.id.timestamp.is_clocked() {
        return Err(Error::MissingBundleAge);
    }

    // We are done with all decrypted content
    parser.decrypted_data.clear();

    // Check we have at least some primary block protection
    if let crc::CrcType::None = bundle.crc_type
        && matches!(
            parser.blocks.get(&0).expect("Missing primary block!").bib,
            block::BibCoverage::None
        )
    {
        return Err(Error::MissingIntegrityCheck);
    }

    if matches!(parser.mode, ParseMode::Full) {
        // Reduce BCB targets scheduled for removal
        parser.reduce_bcbs();
    }

    // Now rewrite blocks (if required)
    let (b, non_canonical) = parser.finish(bundle)?;
    Ok((b, non_canonical, report_unsupported))
}

/// Parses the primary block and converts it to a Bundle.
/// Returns Ok((bundle, canonical)) on success.
/// Returns Err((Some(bundle), error)) for semantic errors (bundle available).
/// Returns Err((None, error)) for CBOR parse errors (no bundle available).
#[allow(clippy::type_complexity, clippy::result_large_err)]
fn parse_primary_block(
    block_array: &mut hardy_cbor::decode::Array,
    canonical: bool,
    tags: &[u64],
) -> Result<(Bundle, bool), (Option<Bundle>, Error)> {
    let mut canonical = canonical && !block_array.is_definite() && tags.is_empty();

    let block_start = block_array.offset();
    let primary_block = block_array
        .parse::<(primary_block::PrimaryBlock, bool)>()
        .map(|(v, s)| {
            canonical = canonical && s;
            v
        })
        .map_field_err::<Error>("Primary Block")
        .map_err(|e| (None, e))?;

    let (bundle, e) = primary_block.into_bundle(block_start..block_array.offset());
    if let Some(e) = e {
        _ = block_array.skip_to_end(16);
        return Err((Some(bundle), e));
    }

    Ok((bundle, canonical))
}

/// Common parsing logic with a key provider closure.
/// Returns Ok((bundle, new_data, non_canonical, report_unsupported)) on success.
/// Returns Err((Some(bundle), error)) for bundle errors (bundle available).
/// Returns Err((None, error)) for CBOR parse errors (no bundle available).
#[allow(clippy::type_complexity, clippy::result_large_err)]
fn parse_bundle_with_provider<F>(
    data: &[u8],
    key_provider: F,
    block_array: &mut hardy_cbor::decode::Array,
    canonical: bool,
    tags: &[u64],
    mode: ParseMode,
) -> Result<(Bundle, Option<Box<[u8]>>, bool, bool), (Option<Bundle>, Error)>
where
    F: FnOnce(&Bundle, &[u8]) -> Box<dyn bpsec::key::KeySource>,
{
    let (mut bundle, canonical) = parse_primary_block(block_array, canonical, tags)?;
    let key_source = key_provider(&bundle, data);
    match parse_blocks(
        &mut bundle,
        canonical,
        block_array,
        data,
        &*key_source,
        mode,
    ) {
        Ok((new_data, non_canonical, report_unsupported)) => {
            Ok((bundle, new_data, non_canonical, report_unsupported))
        }
        Err(e) => Err((Some(bundle), e)),
    }
}

/// Common parsing logic with keys provided directly.
/// Returns Ok((bundle, new_data, non_canonical, report_unsupported)) on success.
/// Returns Err((Some(bundle), error)) for bundle errors (bundle available).
/// Returns Err((None, error)) for CBOR parse errors (no bundle available).
#[allow(clippy::type_complexity, clippy::result_large_err)]
fn parse_bundle_with_keys(
    data: &[u8],
    key_source: &dyn bpsec::key::KeySource,
    block_array: &mut hardy_cbor::decode::Array,
    canonical: bool,
    tags: &[u64],
    mode: ParseMode,
) -> Result<(Bundle, Option<Box<[u8]>>, bool, bool), (Option<Bundle>, Error)> {
    let (mut bundle, canonical) = parse_primary_block(block_array, canonical, tags)?;
    match parse_blocks(&mut bundle, canonical, block_array, data, key_source, mode) {
        Ok((new_data, non_canonical, report_unsupported)) => {
            Ok((bundle, new_data, non_canonical, report_unsupported))
        }
        Err(e) => Err((Some(bundle), e)),
    }
}

/// An intermediate error type used during parsing to distinguish between
/// recoverable and non-recoverable errors.
#[derive(Error, Debug)]
enum RewriteError {
    #[error("An invalid bundle")]
    Invalid(Box<RewrittenBundle>),

    #[error(transparent)]
    InvalidCBOR(#[from] hardy_cbor::decode::Error),

    #[error(transparent)]
    Wrapped(#[from] Error),
}

impl RewrittenBundle {
    /// Parses a byte slice into a `RewrittenBundle` using a key provider closure.
    ///
    /// The closure receives the bundle and raw data, allowing key selection based on
    /// bundle context (e.g., destination EID).
    // Bouncing via RewriteError allows us to avoid the array completeness check when a semantic error occurs
    // so we don't shadow the semantic error by exiting the loop early and therefore reporting 'additional items'
    pub fn parse<F>(data: &[u8], key_provider: F) -> Result<Self, Error>
    where
        F: FnOnce(&Bundle, &[u8]) -> Box<dyn bpsec::key::KeySource>,
    {
        match hardy_cbor::decode::parse_array(data, |a, canonical, tags| {
            match parse_bundle_with_provider(
                data,
                key_provider,
                a,
                canonical,
                tags,
                ParseMode::Full,
            ) {
                Ok((bundle, None, _, report_unsupported)) => Ok(Self::Valid {
                    bundle,
                    report_unsupported,
                }),
                Ok((bundle, Some(new_data), non_canonical, report_unsupported)) => {
                    Ok(Self::Rewritten {
                        bundle,
                        new_data,
                        report_unsupported,
                        non_canonical,
                    })
                }
                Err((Some(bundle), error)) => {
                    let reason = if matches!(&error, Error::Unsupported(_)) {
                        status_report::ReasonCode::BlockUnsupported
                    } else {
                        status_report::ReasonCode::BlockUnintelligible
                    };
                    Err(RewriteError::Invalid(Box::new(Self::Invalid {
                        bundle,
                        reason,
                        error,
                    })))
                }
                Err((None, error)) => Err(error.into()),
            }
        }) {
            Ok((Self::Valid { bundle, .. } | Self::Rewritten { bundle, .. }, len))
                if len != data.len() =>
            {
                Ok(Self::Invalid {
                    bundle,
                    reason: status_report::ReasonCode::BlockUnintelligible,
                    error: Error::AdditionalData,
                })
            }
            Ok((b, _)) => Ok(b),
            Err(RewriteError::Invalid(bundle)) => Ok(*bundle),
            Err(RewriteError::InvalidCBOR(e)) => Err(e.into()),
            Err(RewriteError::Wrapped(e)) => Err(e),
        }
    }

    /// Parses a byte slice into a `RewrittenBundle` using keys provided directly.
    ///
    /// This is a simpler API for cases where keys don't depend on bundle context.
    pub fn parse_with_keys(data: &[u8], keys: &dyn bpsec::key::KeySource) -> Result<Self, Error> {
        match hardy_cbor::decode::parse_array(data, |a, canonical, tags| {
            match parse_bundle_with_keys(data, keys, a, canonical, tags, ParseMode::Full) {
                Ok((bundle, None, _, report_unsupported)) => Ok(Self::Valid {
                    bundle,
                    report_unsupported,
                }),
                Ok((bundle, Some(new_data), non_canonical, report_unsupported)) => {
                    Ok(Self::Rewritten {
                        bundle,
                        new_data,
                        report_unsupported,
                        non_canonical,
                    })
                }
                Err((Some(bundle), error)) => {
                    let reason = if matches!(&error, Error::Unsupported(_)) {
                        status_report::ReasonCode::BlockUnsupported
                    } else {
                        status_report::ReasonCode::BlockUnintelligible
                    };
                    Err(RewriteError::Invalid(Box::new(Self::Invalid {
                        bundle,
                        reason,
                        error,
                    })))
                }
                Err((None, error)) => Err(error.into()),
            }
        }) {
            Ok((Self::Valid { bundle, .. } | Self::Rewritten { bundle, .. }, len))
                if len != data.len() =>
            {
                Ok(Self::Invalid {
                    bundle,
                    reason: status_report::ReasonCode::BlockUnintelligible,
                    error: Error::AdditionalData,
                })
            }
            Ok((b, _)) => Ok(b),
            Err(RewriteError::Invalid(bundle)) => Ok(*bundle),
            Err(RewriteError::InvalidCBOR(e)) => Err(e.into()),
            Err(RewriteError::Wrapped(e)) => Err(e),
        }
    }
}

impl ParsedBundle {
    /// Parses a byte slice into a `ParsedBundle` using a key provider closure.
    ///
    /// The closure receives the bundle and raw data, allowing key selection based on
    /// bundle context (e.g., destination EID).
    pub fn parse<F>(data: &[u8], key_provider: F) -> Result<Self, Error>
    where
        F: FnOnce(&Bundle, &[u8]) -> Box<dyn bpsec::key::KeySource>,
    {
        let (b, len) = hardy_cbor::decode::parse_array(data, |a, canonical, tags| {
            parse_bundle_with_provider(data, key_provider, a, canonical, tags, ParseMode::Preserve)
                .map(|(bundle, _, non_canonical, report_unsupported)| Self {
                    bundle,
                    report_unsupported,
                    non_canonical,
                })
                .map_err(|(_, e)| e)
        })?;

        if len != data.len() {
            Err(Error::AdditionalData)
        } else {
            Ok(b)
        }
    }

    /// Parses a byte slice into a `ParsedBundle` using keys provided directly.
    ///
    /// This is a simpler API for cases where keys don't depend on bundle context.
    pub fn parse_with_keys(data: &[u8], keys: &dyn bpsec::key::KeySource) -> Result<Self, Error> {
        let (b, len) = hardy_cbor::decode::parse_array(data, |a, canonical, tags| {
            parse_bundle_with_keys(data, keys, a, canonical, tags, ParseMode::Preserve)
                .map(|(bundle, _, non_canonical, report_unsupported)| Self {
                    bundle,
                    report_unsupported,
                    non_canonical,
                })
                .map_err(|(_, e)| e)
        })?;

        if len != data.len() {
            Err(Error::AdditionalData)
        } else {
            Ok(b)
        }
    }
}

impl CheckedBundle {
    /// Parses a byte slice into a `CheckedBundle` using a key provider closure.
    ///
    /// This variant canonicalizes the bundle but does not remove any blocks,
    /// making it suitable for validating locally-originated bundles from Services.
    pub fn parse<F>(data: &[u8], key_provider: F) -> Result<Self, Error>
    where
        F: FnOnce(&Bundle, &[u8]) -> Box<dyn bpsec::key::KeySource>,
    {
        let (b, len) = hardy_cbor::decode::parse_array(data, |a, canonical, tags| {
            parse_bundle_with_provider(
                data,
                key_provider,
                a,
                canonical,
                tags,
                ParseMode::Canonicalize,
            )
            .map(|(bundle, new_data, _, _)| Self { bundle, new_data })
            .map_err(|(_, e)| e)
        })?;

        if len != data.len() {
            Err(Error::AdditionalData)
        } else {
            Ok(b)
        }
    }

    /// Parses a byte slice into a `CheckedBundle` using keys provided directly.
    pub fn parse_with_keys(data: &[u8], keys: &dyn bpsec::key::KeySource) -> Result<Self, Error> {
        let (b, len) = hardy_cbor::decode::parse_array(data, |a, canonical, tags| {
            parse_bundle_with_keys(data, keys, a, canonical, tags, ParseMode::Canonicalize)
                .map(|(bundle, new_data, _, _)| Self { bundle, new_data })
                .map_err(|(_, e)| e)
        })?;

        if len != data.len() {
            Err(Error::AdditionalData)
        } else {
            Ok(b)
        }
    }
}

impl Id {
    /// Parses a byte slice into an `Id`.
    // Bouncing via RewriteError allows us to avoid the array completeness check when a semantic error occurs
    // so we don't shadow the semantic error by exiting the loop early and therefore reporting 'additional items'
    pub fn parse(data: &[u8]) -> Result<Self, Error> {
        let (b, len) = hardy_cbor::decode::parse_array(data, |a, shortest, tags| {
            Self::parse_inner(a, shortest, tags)
        })?;

        if len != data.len() {
            Err(Error::AdditionalData)
        } else {
            Ok(b)
        }
    }

    /// The inner parsing logic, called by `parse`.
    /// This function is responsible for parsing the primary block and then handing off
    /// to `parse_blocks` for the extension blocks.
    fn parse_inner(
        block_array: &mut hardy_cbor::decode::Array,
        mut canonical: bool,
        tags: &[u64],
    ) -> Result<Self, Error> {
        // Check for shortest/correct form
        canonical = canonical && !block_array.is_definite() && tags.is_empty();

        // Parse Primary block
        let block_start = block_array.offset();
        let primary_block = block_array
            .parse::<(primary_block::PrimaryBlock, bool)>()
            .map(|(v, s)| {
                canonical = canonical && s;
                v
            })
            .map_field_err::<Error>("Primary Block")?;

        let (bundle, e) = primary_block.into_bundle(block_start..block_array.offset());
        if let Some(e) = e {
            _ = block_array.skip_to_end(16);
            return Err(e);
        }

        // Skip all the blocks
        block_array.skip_to_end(16)?;

        Ok(bundle.id)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use hex_literal::hex;

    // Requirement: LLR 1.1.15
    #[test]
    fn tests() {
        // From Stephan Havermans testing
        assert!(matches!(
            RewrittenBundle::parse_with_keys(
                &hex!(
                    "9f89071844018202820301820100820100821b000000b5998c982b011a000493e042c9f6850602182700458202820200850704010042183485010101004454455354ff"
                ),
                &bpsec::key::KeySet::new(vec![]),
            ),
            Ok(RewrittenBundle::Invalid {
                error: Error::InvalidFlags,
                ..
            })
        ));
    }

    // TODO: Implement test for LLR 1.1.33: Processing must use Bundle Age block for expiry if Creation Time is zero.
    // Scenario: Parse a bundle with creation_timestamp = 0 and no Bundle Age block. Expect Error::MissingBundleAge.

    // TODO: Implement test for LLR 1.1.34: Processing must process and act on Hop Count extension block.
    // Scenario: Parse a bundle with a Hop Count block. Verify it is extracted into bundle.hop_count.

    // TODO: Implement test for LLR 1.1.14: Parser must indicate when bundle rewriting has occurred.
    // Scenario: Parse a non-canonical bundle (e.g., blocks out of order) that is otherwise valid. Expect RewrittenBundle::Rewritten.

    // TODO: Implement test for LLR 1.1.19: Parser must parse/validate extension blocks specified in RFC 9171.
    // Scenario: Verify correct parsing of PreviousNode, BundleAge, and HopCount blocks into the Bundle struct.

    // TODO: Implement test for LLR 1.1.1: Compliant with all mandatory requirements of CCSDS Bundle Protocol.
    // Scenario: Verify general compliance (e.g. no floats, correct structure).

    // TODO: Implement test for LLR 1.1.30: Processing must enforce bundle rewriting rules when discarding unrecognised blocks.
    // Scenario: Parse a bundle with an unrecognised block marked for deletion. Verify it is removed and bundle is rewritten.

    // TODO: Implement test for LLR 1.1.12: CBOR decoder must indicate if an incomplete item is found at end of buffer.
    // Scenario: Feed truncated bundle data. Expect Error::NeedMoreData (or equivalent).

    // TODO: Implement test for Trailing Data.
    // Scenario: Feed a valid bundle followed by extra bytes. Expect Error::AdditionalData.
}
