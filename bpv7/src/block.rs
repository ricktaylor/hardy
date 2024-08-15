use super::*;
use bundle::CaptureFieldErr;

#[derive(Debug, Clone)]
pub struct Block {
    pub block_type: BlockType,
    pub flags: BlockFlags,
    pub crc_type: CrcType,
    pub data_offset: usize,
    pub data_len: usize,
}

pub struct BlockWithNumber {
    pub number: u64,
    pub block: Block,
}

impl cbor::decode::FromCbor for BlockWithNumber {
    type Error = BundleError;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, usize)>, Self::Error> {
        cbor::decode::try_parse_array(data, |block, tags| {
            if block.count().is_none() {
                trace!("Parsing extension block of indefinite length")
            }
            if !tags.is_empty() {
                trace!("Parsing extension block with tags");
            }

            let block_type = block
                .parse::<u64>()
                .map_field_err("Block type code")?
                .into();

            let block_number = block.parse::<u64>().map_field_err("Block number")?;
            if block_number == 0 {
                return Err(BundleError::InvalidBlockNumber);
            }

            let flags = block
                .parse::<u64>()
                .map_field_err("Block processing control flags")?
                .into();
            let crc_type = block.parse::<CrcType>().map_field_err("CRC type")?;

            // Stash start of data
            let ((data_offset, _), data_len) =
                block.parse_value(|value, data_start, tags| match value {
                    cbor::decode::Value::Bytes(v, chunked) => {
                        if chunked {
                            trace!("Parsing chunked extension block data");
                        }
                        if !tags.is_empty() {
                            trace!("Parsing extension block data with tags");
                        }
                        Ok((data_start, v.len()))
                    }
                    value => Err(cbor::decode::Error::IncorrectType(
                        "Byte String".to_string(),
                        value.type_name(!tags.is_empty()),
                    )),
                })?;

            // Check CRC
            crc::parse_crc_value(data, block, crc_type)?;

            Ok(BlockWithNumber {
                number: block_number,
                block: Block {
                    block_type,
                    flags,
                    crc_type,
                    data_offset,
                    data_len,
                },
            })
        })
    }
}
