use super::*;
use base64::prelude::*;
use serde::{Deserialize, Serialize};

mod parse;
mod primary_block;

pub enum Payload {
    Range(core::ops::Range<usize>),
    Owned(zeroize::Zeroizing<Box<[u8]>>),
}

impl core::fmt::Debug for Payload {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Range(arg0) => write!(f, "Payload {} bytes", arg0.len()),
            Self::Owned(arg0) => write!(f, "Payload {} bytes", arg0.len()),
        }
    }
}

#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct FragmentInfo {
    pub offset: u64,
    pub total_len: u64,
}

pub mod id {
    use super::*;
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
            source: Box<dyn core::error::Error + Send + Sync>,
        },

        #[error(transparent)]
        InvalidCBOR(#[from] hardy_cbor::decode::Error),
    }
}

trait CaptureFieldIdErr<T> {
    fn map_field_id_err(self, field: &'static str) -> Result<T, id::Error>;
}

impl<T, E: Into<Box<dyn core::error::Error + Send + Sync>>> CaptureFieldIdErr<T>
    for core::result::Result<T, E>
{
    fn map_field_id_err(self, field: &'static str) -> Result<T, id::Error> {
        self.map_err(|e| id::Error::InvalidField {
            field,
            source: e.into(),
        })
    }
}

#[derive(Default, Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
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
                fragment_info: if array.count() == Some(4) {
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
                array.emit(&fragment_info.offset);
                array.emit(&fragment_info.total_len);
            })
        } else {
            hardy_cbor::encode::emit_array(Some(2), |array| {
                array.emit(&self.source);
                array.emit(&self.timestamp);
            })
        })
    }
}

#[derive(Default, Debug, Clone, Serialize, Deserialize)]
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

    #[serde(skip_serializing_if = "Option::is_none")]
    pub unrecognised: Option<u64>,
}

impl From<u64> for Flags {
    fn from(value: u64) -> Self {
        let mut flags = Self {
            unrecognised: {
                let u = value & !((2 ^ 20) - 1);
                if u == 0 { None } else { Some(u) }
            },
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
                        flags.unrecognised = Some(flags.unrecognised.unwrap_or_default() | 1 << b);
                    }
                }
            }
        }
        flags
    }
}

impl From<&Flags> for u64 {
    fn from(value: &Flags) -> Self {
        let mut flags = value.unrecognised.unwrap_or_default();
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

impl hardy_cbor::encode::ToCbor for Flags {
    fn to_cbor(&self, encoder: &mut hardy_cbor::encode::Encoder) {
        encoder.emit(&u64::from(self))
    }
}

impl hardy_cbor::decode::FromCbor for Flags {
    type Error = hardy_cbor::decode::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        hardy_cbor::decode::try_parse::<(u64, bool, usize)>(data)
            .map(|o| o.map(|(value, shortest, len)| (value.into(), shortest, len)))
    }
}

struct BlockSet<'a> {
    bundle: &'a Bundle,
    source_data: &'a [u8],
}

impl<'a> bpsec::BlockSet<'a> for BlockSet<'a> {
    fn block(&self, block_number: u64) -> Option<&block::Block> {
        self.bundle.blocks.get(&block_number)
    }

    fn block_payload(&self, block_number: u64) -> Option<&[u8]> {
        Some(&self.source_data[self.block(block_number)?.payload()])
    }
}

#[derive(Default, Debug, Clone, Serialize, Deserialize)]
pub struct Bundle {
    // From Primary Block
    #[serde(flatten)]
    pub id: Id,

    pub flags: Flags,
    pub crc_type: crc::CrcType,
    pub destination: eid::Eid,
    pub report_to: eid::Eid,
    pub lifetime: core::time::Duration,

    // Unpacked from extension blocks
    pub previous_node: Option<eid::Eid>,
    pub age: Option<core::time::Duration>,
    pub hop_count: Option<hop_info::HopInfo>,

    // The extension blocks
    pub blocks: HashMap<u64, block::Block>,
}

impl Bundle {
    pub(crate) fn emit_primary_block(&mut self, array: &mut hardy_cbor::encode::Array) {
        let start = array.offset();
        array.emit_raw(primary_block::PrimaryBlock::emit(self));

        // Replace existing block record
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
                extent: start..array.offset(),
                data: start..array.offset(),
                bib: None,
                bcb: None,
            },
        );
    }

    pub fn block_payload(
        &self,
        block_number: u64,
        source_data: &[u8],
        key_f: &impl bpsec::key::KeyStore,
    ) -> Result<Option<Payload>, Error> {
        let payload_block = self.blocks.get(&block_number).ok_or(Error::Altered)?;

        // Check for BCB
        let Some(bcb_block_number) = &payload_block.bcb else {
            // Check we won't panic
            _ = source_data
                .get(payload_block.payload())
                .ok_or(Error::Altered)?;

            return Ok(Some(Payload::Range(payload_block.payload())));
        };

        let bcb = self
            .blocks
            .get(bcb_block_number)
            .ok_or(Error::Altered)
            .and_then(|bcb_block| {
                source_data
                    .get(bcb_block.payload())
                    .ok_or(Error::Altered)
                    .and_then(|data| {
                        hardy_cbor::decode::parse::<bpsec::bcb::OperationSet>(data)
                            .map_err(|_| Error::Altered)
                    })
            })?;

        // Confirm we can decrypt if we have keys
        if let Some(plaintext) = bcb
            .operations
            .get(&block_number)
            .ok_or(Error::Altered)?
            .decrypt_any(
                key_f,
                bpsec::bcb::OperationArgs {
                    bpsec_source: &bcb.source,
                    target: block_number,
                    source: *bcb_block_number,
                    blocks: &BlockSet {
                        bundle: self,
                        source_data,
                    },
                },
            )?
        {
            Ok(Some(Payload::Owned(plaintext)))
        } else {
            Ok(None)
        }
    }
}

// For parsing a bundle plus 'minimal viability'
#[derive(Debug)]
pub enum ValidBundle {
    Valid(Bundle, bool),
    Rewritten(Bundle, Box<[u8]>, bool),
    Invalid(Bundle, status_report::ReasonCode, Error),
}
