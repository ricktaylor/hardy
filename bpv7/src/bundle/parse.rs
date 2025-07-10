use super::*;
use alloc::borrow::Cow;
use error::CaptureFieldErr;

impl Bundle {
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
        key_f: &impl bpsec::key::KeyStore,
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
            let (bcb_block, bcb, s) = self
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

            // Check targets
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

                if !blocks_to_remove.contains(target_number) {
                    // Confirm we can decrypt if we have keys
                    let decrypt = op.decrypt_any(
                        key_f,
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

                    if decrypt.protects_primary_block {
                        protects_primary_block.insert(bcb_block_number);
                    }

                    // Stash the decrypted data
                    if let Some(block_data) = decrypt.plaintext {
                        decrypted_data.insert(*target_number, block_data);
                    } else if let block::Type::BlockIntegrity = target_block.block_type {
                        // We can't decrypt the BIB, therefore we cannot check the BIB
                        bibs_to_check.remove(target_number);
                    }
                }
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
                    self.age = Some(core::time::Duration::from_millis(v));
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

                if noncanonical_blocks.get(target_number) == Some(&true) {
                    // Non-canonical block with signature is unrecoverable
                    return Err(Error::NonCanonical(*target_number));
                }

                if !blocks_to_remove.contains(target_number) {
                    let r = op.validate(bpsec::bib::OperationArgs {
                        bpsec_source: &bib.source,
                        target: target_block,
                        target_number: *target_number,
                        target_payload: target_block.payload(source_data),
                        source: bib_block,
                        source_number: bib_block_number,
                        primary_block: &primary_block_data,
                    })?;

                    if r.protects_primary_block {
                        protects_primary_block.insert(bib_block_number);
                    }
                }
            }

            // Remove targets scheduled for removal
            let old_len = bib.operations.len();
            bib.operations.retain(|k, _| !blocks_to_remove.contains(k));
            if bib.operations.is_empty() {
                noncanonical_blocks.remove(&bib_block_number);
                protects_primary_block.remove(&bib_block_number);
                blocks_to_remove.insert(bib_block_number);
                continue;
            } else if !canonical || bib.operations.len() != old_len {
                noncanonical_blocks.insert(bib_block_number, true);
                bibs.insert(bib_block_number, bib);
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
        if let crc::CrcType::None = self.crc_type {
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
                            hardy_cbor::encode::emit(&(self.age.unwrap().as_millis() as u64))
                                .into(),
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
        for (bib_block_number, bib) in bibs {
            noncanonical_blocks.remove(&bib_block_number);
            new_payloads.insert(bib_block_number, hardy_cbor::encode::emit(&bib).into());
        }

        // Update BCBs
        for (bcb_block_number, bcb) in bcbs {
            noncanonical_blocks.remove(&bcb_block_number);
            new_payloads.insert(bcb_block_number, hardy_cbor::encode::emit(&bcb).into());
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
}

impl ValidBundle {
    pub fn parse(data: &[u8], key_f: &impl bpsec::key::KeyStore) -> Result<Self, Error> {
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
                key_f,
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
