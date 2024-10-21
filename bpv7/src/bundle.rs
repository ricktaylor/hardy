use super::*;
use std::collections::HashSet;
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

    #[error("Bundle extension block must not be block number 0 or 1")]
    InvalidBlockNumber,

    #[error("Bundle has multiple {0:?} blocks")]
    DuplicateBlocks(BlockType),

    #[error("{0:?} block has additional data")]
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

// For parsing a bundle plus 'minimal viability'
#[derive(Debug)]
pub enum ValidBundle {
    Valid(Bundle),
    Invalid(Bundle),
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
    let (flags, s) = block
        .parse::<(u64, bool)>()
        .map(|(v, s)| (BundleFlags::from(v), s))
        .map_field_err("Bundle Processing Control Flags")?;
    shortest = shortest && s;

    // Parse CRC Type
    let crc_type = block.parse();

    // Parse EIDs
    let dest_eid = block.parse();
    let source_eid = block.parse();
    let (report_to, s) = block.parse().map_field_err("Report-to EID")?;
    shortest = shortest && s;

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

fn parse_known_block<T>(block: &Block, data: &[u8], shortest: &mut bool) -> Result<T, BundleError>
where
    T: cbor::decode::FromCbor<Error: From<cbor::decode::Error>>,
    BundleError: From<<T as cbor::decode::FromCbor>::Error>,
{
    let data = &block.block_data(data);
    let (v, s, len) = cbor::decode::parse(&data)?;
    if len != data.len() {
        Err(BundleError::BlockAdditionalData(block.block_type))
    } else {
        *shortest = *shortest && s;
        Ok(v)
    }
}

impl Bundle {
    fn parse_blocks(
        &mut self,
        blocks: &mut cbor::decode::Array,
        mut offset: usize,
        data: &[u8],
    ) -> Result<bool, BundleError> {
        let mut shortest = true;

        // Use an intermediate vector so we can check the payload was the last item
        let mut extension_blocks = Vec::new();
        while let Some((mut block, s, block_len)) =
            blocks.try_parse::<(block::BlockWithNumber, bool, usize)>()?
        {
            shortest = shortest && s;
            block.block.data_start += offset;
            offset += block_len;
            extension_blocks.push(block);
        }

        // Check the last block is the payload
        match extension_blocks.last() {
            None => return Err(BundleError::MissingPayload),
            Some(block) => {
                if block.block.block_type != BlockType::Payload {
                    return Err(BundleError::PayloadNotFinal);
                }
            }
        }

        // Add blocks
        for block::BlockWithNumber { number, block } in extension_blocks {
            if self.blocks.insert(number, block).is_some() {
                return Err(BundleError::DuplicateBlockNumber(number));
            }
        }

        // Check the blocks
        self.parse_extension_blocks(data).map(|s| shortest && s)
    }

    pub fn parse_extension_blocks(&mut self, data: &[u8]) -> Result<bool, BundleError> {
        // Check for RFC9171-specified extension blocks
        let mut seen_payload = false;
        let mut seen_previous_node = false;
        let mut seen_bundle_age = false;
        let mut seen_hop_count = false;
        let mut shortest = true;
        let mut bpsec_targets = HashSet::new();

        for block in self.blocks.values() {
            match &block.block_type {
                BlockType::Payload => {
                    if seen_payload {
                        return Err(BundleError::DuplicateBlocks(block.block_type));
                    }
                    seen_payload = true;
                }
                BlockType::PreviousNode => {
                    if seen_previous_node {
                        return Err(BundleError::DuplicateBlocks(block.block_type));
                    }
                    self.previous_node = Some(
                        parse_known_block(block, data, &mut shortest)
                            .map_field_err("Previous Node ID")?,
                    );
                    seen_previous_node = true;
                }
                BlockType::BundleAge => {
                    if seen_bundle_age {
                        return Err(BundleError::DuplicateBlocks(block.block_type));
                    }
                    self.age = Some(
                        parse_known_block(block, data, &mut shortest)
                            .map_field_err("Bundle Age")?,
                    );
                    seen_bundle_age = true;
                }
                BlockType::HopCount => {
                    if seen_hop_count {
                        return Err(BundleError::DuplicateBlocks(block.block_type));
                    }
                    self.hop_count = Some(
                        parse_known_block(block, data, &mut shortest)
                            .map_field_err("Hop Count Block")?,
                    );
                    seen_hop_count = true;
                }
                BlockType::BlockIntegrity | BlockType::BlockSecurity => {
                    parse_known_block::<bpsec::SecurityBlock>(block, data, &mut shortest)
                        .and_then(|sb| {
                            for target in sb.results.keys() {
                                sb.validate(block, self.blocks.get(target), data)?;

                                // Check uniqueness
                                if !bpsec_targets.insert((block.block_type, *target)) {
                                    return Err(bpsec::Error::DuplicateOpTarget.into());
                                }
                            }
                            Ok(())
                        })
                        .map_field_err("BPSec extension block")?;
                }
                _ => {}
            }
        }

        if !seen_bundle_age && self.id.timestamp.creation_time.is_none() {
            return Err(BundleError::MissingBundleAge);
        }

        Ok(shortest)
    }

    pub fn emit_primary_block(&mut self, offset: usize) -> Vec<u8> {
        let block_data = crc::append_crc_value(
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
                    a.emit::<u64>(self.flags.into());
                    // CRC
                    a.emit::<u64>(self.crc_type.into());
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
        );

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

    pub fn canonicalise(&mut self, source_data: &[u8]) -> Result<Vec<u8>, BundleError> {
        // Begin indefinite array
        let mut data = vec![(4 << 5) | 31u8];

        // Emit primary block
        let block_data = self.emit_primary_block(data.len());
        data.extend(block_data);

        // Stash payload block for last
        let mut payload_block = self.blocks.remove(&1).expect("No payload block!");

        // Emit extension blocks
        let mut to_remove = Vec::new();
        for (block_number, block) in &mut self.blocks {
            let block_data = match &block.block_type {
                BlockType::Primary | BlockType::Payload => {
                    // Skip
                    continue;
                }
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
                BlockType::Unrecognised(_) if block.flags.delete_block_on_failure => {
                    to_remove.push(*block_number);
                    continue;
                }
                BlockType::Unrecognised(_) => block.emit(
                    *block_number,
                    &cbor::encode::emit(block.block_data(source_data).into_vec()),
                    data.len(),
                ),
                BlockType::BlockIntegrity | BlockType::BlockSecurity => {
                    //todo!()
                    block.emit(
                        *block_number,
                        &cbor::encode::emit(block.block_data(source_data).into_vec()),
                        data.len(),
                    )
                }
            };
            data.extend(block_data);
        }

        // Remove invalid blocks
        for block_number in to_remove {
            self.blocks.remove(&block_number);
        }

        // Emit payload block
        let block_data = payload_block.emit(
            1,
            &cbor::encode::emit(payload_block.block_data(source_data).into_vec()),
            data.len(),
        );
        data.extend(block_data);
        self.blocks.insert(1, payload_block);

        // End indefinite array
        data.push(0xFF);

        Ok(data)
    }
}

impl cbor::decode::FromCbor for ValidBundle {
    type Error = BundleError;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        let Some(((bundle, mut valid, shortest), len)) =
            cbor::decode::try_parse_array(data, |blocks, mut shortest, tags| {
                // Check for shortest/correct form
                shortest = shortest && !blocks.is_definite();
                if shortest {
                    // Appendix B of RFC9171
                    let mut seen_55799 = false;
                    for tag in &tags {
                        match *tag {
                            255799 if !seen_55799 => seen_55799 = true,
                            _ => {
                                shortest = false;
                                break;
                            }
                        }
                    }
                }

                // Parse Primary block
                let block_start = blocks.offset();
                let ((mut bundle, mut valid), block_len) = blocks
                    .parse_array(|block, s, tags| {
                        shortest = shortest && s && tags.is_empty() && block.is_definite();

                        parse_primary_block(data, block, block_start).map(|(bundle, valid, s)| {
                            shortest = shortest && s;
                            (bundle, valid)
                        })
                    })
                    .map_field_err("Primary Block")?;

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

                    if let Ok(s) = bundle.parse_blocks(blocks, block_start + block_len, data) {
                        shortest = shortest && s;
                    } else {
                        // Don't return an Err, just capture validity
                        valid = false;
                    }
                }

                if !valid {
                    // Just skip over the blocks, avoiding any further parsing
                    blocks.skip_to_end(16)?;
                }

                Ok::<_, BundleError>((bundle, valid, shortest))
            })?
        else {
            return Ok(None);
        };

        if len < data.len() {
            valid = false;
        }

        Ok(Some((
            if valid {
                ValidBundle::Valid(bundle)
            } else {
                ValidBundle::Invalid(bundle)
            },
            if valid { shortest } else { false },
            len,
        )))
    }
}

#[cfg(test)]
use std::io::Write;

#[test]
fn fuzz_tests() {
    let data = &hex_literal::hex!(
        "9f88070000820282010282028202018202820201820118281a000f4240850b0200
            005856810101018202820201828201078203008181820158403bdc69b3a34a2b5d3a
            8554368bd1e808f606219d2a10a846eae3886ae4ecc83c4ee550fdfb1cc636b904e2
            f1a73e303dcd4b6ccece003e95e8164dcc89a156e185010100005823526561647920
            746f2067656e657261746520612033322d62797465207061796c6f6164ff"
    );

    let r = cbor::decode::parse(data);

    dbg!(&r);

    if let Ok((ValidBundle::Valid(mut bundle), false)) = r {
        let data = bundle.canonicalise(data).unwrap();

        let mut file = std::fs::File::create("rewritten_bundle").unwrap();
        file.write_all(data.as_ref()).unwrap();

        let r = cbor::decode::parse(&data);

        dbg!(&r);

        let Ok((ValidBundle::Valid(_), true)) = r else {
            panic!("Rewrite borked");
        };
    }
}
