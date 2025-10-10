/*!
This module defines the core `Bundle` structure and its components, providing the
primary interface for creating, parsing, and interacting with BPv7 bundles.
It orchestrates the various parts of a bundle, from the primary block to extension
blocks and payload, and handles parsing validation and security operations.
*/

use super::*;
use base64::prelude::*;

mod parse;
mod primary_block;

/// Represents the payload of a block.
///
/// The payload can either be a direct slice (`Range`) into the original bundle's
/// byte array, or an `Owned` byte slice. The `Owned` variant is used when the
/// payload has been decrypted from a Block Confidentiality Block (BCB) and
/// therefore does not correspond to a contiguous region of the original data.
pub enum Payload {
    /// A range of bytes within the original bundle data.
    Range(core::ops::Range<usize>),
    /// An owned byte slice, typically holding a decrypted payload.
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

/// Holds fragmentation information for a bundle.
///
/// As defined in RFC 9171 Section 4.2.1, this information is present in the
/// primary block if the bundle is a fragment of a larger original bundle.
#[derive(Debug, Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "bincode", derive(bincode::Encode, bincode::Decode))]
pub struct FragmentInfo {
    /// The offset of this fragment's payload within the original bundle's payload.
    pub offset: u64,
    /// The total length of the original bundle's payload.
    pub total_len: u64,
}

/// Contains the [`Id`] struct for uniquely identifying a bundle and related helpers.
pub mod id {
    use super::*;
    use thiserror::Error;

    /// Errors that can occur when parsing a bundle [`Id`] from a key.
    #[derive(Error, Debug)]
    pub enum Error {
        /// The key string is malformed and cannot be parsed.
        #[error("Bad bundle id key")]
        BadKey,

        /// The key is not valid Base64.
        #[error("Bad base64 encoding: {0}")]
        BadBase64(base64::DecodeError),

        /// A field within the decoded CBOR data is invalid.
        #[error("Failed to decode {field}: {source}")]
        InvalidField {
            field: &'static str,
            source: Box<dyn core::error::Error + Send + Sync>,
        },

        /// An error occurred during CBOR decoding.
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

/// Represents the unique identifier of a BPv7 bundle.
///
/// A bundle ID is a tuple of `(source EID, creation timestamp, fragment info)`.
/// This combination is guaranteed to be unique across the DTN.
#[derive(Default, Debug, Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "bincode", derive(bincode::Encode, bincode::Decode))]
pub struct Id {
    /// The EID of the node that created the bundle.
    pub source: eid::Eid,
    /// The creation timestamp, including a sequence number for uniqueness.
    pub timestamp: creation_timestamp::CreationTimestamp,
    /// Fragmentation information, if this bundle is a fragment.
    pub fragment_info: Option<FragmentInfo>,
}

impl Id {
    /// Deserializes a bundle ID from a compact, base64-encoded string representation.
    ///
    /// This is useful for using the bundle ID as a key in databases or other systems.
    pub fn from_key(k: &str) -> Result<Self, id::Error> {
        hardy_cbor::decode::parse_array(
            &BASE64_STANDARD_NO_PAD
                .decode(k)
                .map_err(id::Error::BadBase64)?,
            |array, _, _| {
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
                if !array.at_end()? {
                    Err(id::Error::BadKey)
                } else {
                    Ok(s)
                }
            },
        )
        .map(|v| v.0)
    }

    /// Serializes the bundle ID into a compact, base64-encoded string representation.
    ///
    /// This is useful for using the bundle ID as a key in databases or other systems.
    pub fn to_key(&self) -> String {
        BASE64_STANDARD_NO_PAD.encode(
            if let Some(fragment_info) = &self.fragment_info {
                hardy_cbor::encode::emit(&(
                    &self.source,
                    &self.timestamp,
                    fragment_info.offset,
                    fragment_info.total_len,
                ))
            } else {
                hardy_cbor::encode::emit(&(&self.source, &self.timestamp))
            }
            .0,
        )
    }
}

/// Represents the processing control flags for a BPv7 bundle.
///
/// These flags, defined in RFC 9171 Section 4.2.3, control how a node should
/// handle the bundle, such as whether it can be fragmented or if status reports
/// are requested.
#[derive(Default, Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "bincode", derive(bincode::Encode, bincode::Decode))]
pub struct Flags {
    /// If set, this bundle is a fragment of a larger bundle.
    pub is_fragment: bool,
    /// If set, the payload is an administrative record.
    pub is_admin_record: bool,
    /// If set, the bundle must not be fragmented.
    pub do_not_fragment: bool,
    /// If set, the destination application is requested to send an acknowledgement.
    pub app_ack_requested: bool,
    /// If set, status reports should include the time of the reported event.
    pub report_status_time: bool,
    /// If set, a status report should be generated upon bundle reception.
    pub receipt_report_requested: bool,
    /// If set, a status report should be generated upon bundle forwarding.
    pub forward_report_requested: bool,
    /// If set, a status report should be generated upon bundle delivery.
    pub delivery_report_requested: bool,
    /// If set, a status report should be generated upon bundle deletion.
    pub delete_report_requested: bool,

    /// A bitmask of any unrecognized flags encountered during parsing.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub unrecognised: Option<u64>,
}

impl From<u64> for Flags {
    fn from(value: u64) -> Self {
        let mut flags = Self::default();
        let mut unrecognised = value;

        if (value & (1 << 0)) != 0 {
            flags.is_fragment = true;
            unrecognised &= !(1 << 0);
        }
        if (value & (1 << 1)) != 0 {
            flags.is_admin_record = true;
            unrecognised &= !(1 << 1);
        }
        if (value & (1 << 2)) != 0 {
            flags.do_not_fragment = true;
            unrecognised &= !(1 << 2);
        }
        if (value & (1 << 5)) != 0 {
            flags.app_ack_requested = true;
            unrecognised &= !(1 << 5);
        }
        if (value & (1 << 6)) != 0 {
            flags.report_status_time = true;
            unrecognised &= !(1 << 6);
        }
        if (value & (1 << 14)) != 0 {
            flags.receipt_report_requested = true;
            unrecognised &= !(1 << 14);
        }
        if (value & (1 << 16)) != 0 {
            flags.forward_report_requested = true;
            unrecognised &= !(1 << 16);
        }
        if (value & (1 << 17)) != 0 {
            flags.delivery_report_requested = true;
            unrecognised &= !(1 << 17);
        }
        if (value & (1 << 18)) != 0 {
            flags.delete_report_requested = true;
            unrecognised &= !(1 << 18);
        }

        if unrecognised != 0 {
            flags.unrecognised = Some(unrecognised);
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
    type Result = ();

    fn to_cbor(&self, encoder: &mut hardy_cbor::encode::Encoder) -> Self::Result {
        encoder.emit(&u64::from(self))
    }
}

impl hardy_cbor::decode::FromCbor for Flags {
    type Error = hardy_cbor::decode::Error;

    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        hardy_cbor::decode::parse::<(u64, bool, usize)>(data)
            .map(|(value, shortest, len)| (value.into(), shortest, len))
    }
}

/// A view into a bundle's blocks for BPSec operations.
struct BlockSet<'a> {
    bundle: &'a Bundle,
    source_data: &'a [u8],
}

impl<'a> bpsec::BlockSet<'a> for BlockSet<'a> {
    /// Retrieves a reference to a block by its number.
    fn block(&self, block_number: u64) -> Option<&block::Block> {
        self.bundle.blocks.get(&block_number)
    }

    /// Retrieves the payload of a block as a byte slice.
    fn block_payload(&self, block_number: u64) -> Option<&[u8]> {
        Some(&self.source_data[self.block(block_number)?.payload()])
    }
}

/// Represents a complete BPv7 bundle.
///
/// This struct contains all the information from the primary block, data unpacked
/// from known extension blocks, and a map of all blocks present in the bundle.
/// The bundle's raw byte data is stored separately, and this struct provides
/// methods to access and interpret it.
#[derive(Default, Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "bincode", derive(bincode::Encode, bincode::Decode))]
pub struct Bundle {
    // From Primary Block
    /// The unique identifier for the bundle.
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub id: Id,

    /// The bundle-specific processing control flags.
    pub flags: Flags,
    /// The type of CRC used for the primary block's integrity check.
    pub crc_type: crc::CrcType,
    /// The EID of the bundle's final destination.
    pub destination: eid::Eid,
    /// The EID to which status reports should be sent.
    pub report_to: eid::Eid,
    /// The time duration after which the bundle should be considered expired.
    pub lifetime: core::time::Duration,

    // Unpacked from extension blocks
    /// The EID of the node that last forwarded the bundle.
    pub previous_node: Option<eid::Eid>,
    /// The age of the bundle, used if the source node has no clock.
    pub age: Option<core::time::Duration>,
    /// The hop limit and current hop count for the bundle.
    pub hop_count: Option<hop_info::HopInfo>,

    /// A map of all blocks in the bundle, keyed by their block number.
    pub blocks: HashMap<u64, block::Block>,
}

impl Bundle {
    /// Emits the primary block into a CBOR array during bundle creation.
    pub(crate) fn emit_primary_block(
        &mut self,
        array: &mut hardy_cbor::encode::Array,
    ) -> Result<(), Error> {
        let extent = array.emit(&hardy_cbor::encode::RawOwned::new(
            primary_block::PrimaryBlock::emit(self)?,
        ));

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
                data: extent.clone(),
                extent,
                bib: None,
                bcb: None,
            },
        );
        Ok(())
    }

    /// Retrieves the payload of a specific block by its number.
    ///
    /// This method handles the complexity of block-level security. If the target
    /// block is encrypted with a Block Confidentiality Block (BCB), this method
    /// will attempt to decrypt it using the provided `key_f` keystore.
    ///
    /// Returns `Ok(None)` if the payload is encrypted and cannot be decrypted.
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
                        hardy_cbor::decode::parse::<bpsec::bcb::OperationSet>(data).map_err(|e| {
                            Error::InvalidField {
                                field: "BCB Abstract Syntax Block",
                                source: e.into(),
                            }
                        })
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

/// Represents the result of parsing a bundle.
///
/// Parsing a bundle can have several outcomes depending on its validity,
/// canonical form, and the presence of security features.
#[derive(Debug)]
pub enum ValidBundle {
    /// The bundle was parsed successfully and was in canonical form.
    /// The boolean indicates if an unsupported block was encountered that requests a report.
    Valid(Bundle, bool),
    /// The bundle was valid but not in canonical CBOR form. A rewritten, canonical
    /// version of the bundle data is provided. The booleans indicate if a report
    /// is requested for an unsupported block and if the rewrite was due to non-canonical CBOR.
    Rewritten(Bundle, Box<[u8]>, bool, bool),
    /// The bundle was invalid. The partially-parsed `Bundle` is provided along with
    /// a `ReasonCode` for status reports and the specific `Error` that occurred.
    Invalid(Bundle, status_report::ReasonCode, Error),
}
