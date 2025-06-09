use super::*;
use base64::prelude::*;

mod parse;
mod primary_block;

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
