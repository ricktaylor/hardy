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

/// A key provider function that returns no keys.
/// Use this when parsing bundles that don't require decryption.
pub fn no_keys(_bundle: &Bundle, _data: &[u8]) -> Box<dyn bpsec::key::KeySource> {
    Box::new(bpsec::key::KeySet::EMPTY)
}

/// Holds fragmentation information for a bundle.
///
/// As defined in RFC 9171 Section 4.2.1, this information is present in the
/// primary block if the bundle is a fragment of a larger original bundle.
#[derive(Debug, Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct FragmentInfo {
    /// The offset of this fragment's payload within the original bundle's payload.
    pub offset: u64,
    /// The total length of the original bundle's payload.
    pub total_adu_length: u64,
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
pub struct Id {
    /// The EID of the node that created the bundle.
    pub source: eid::Eid,
    /// The creation timestamp, including a sequence number for uniqueness.
    pub timestamp: creation_timestamp::CreationTimestamp,
    /// Fragmentation information, if this bundle is a fragment.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
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
                            total_adu_length: array
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
                    fragment_info.total_adu_length,
                ))
            } else {
                hardy_cbor::encode::emit(&(&self.source, &self.timestamp))
            }
            .0,
        )
    }
}

impl core::fmt::Display for Id {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if let Some(fi) = &self.fragment_info {
            write!(
                f,
                "{}/{} fragment {}/{}",
                self.source, self.timestamp, fi.offset, fi.total_adu_length
            )
        } else {
            write!(f, "{}/{}", self.source, self.timestamp)
        }
    }
}

/// Represents the processing control flags for a BPv7 bundle.
///
/// These flags, defined in RFC 9171 Section 4.2.3, control how a node should
/// handle the bundle, such as whether it can be fragmented or if status reports
/// are requested.
#[derive(Default, Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Flags {
    /// If set, this bundle is a fragment of a larger bundle.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "<&bool as core::ops::Not>::not")
    )]
    pub is_fragment: bool,

    /// If set, the payload is an administrative record.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "<&bool as core::ops::Not>::not")
    )]
    pub is_admin_record: bool,

    /// If set, the bundle must not be fragmented.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "<&bool as core::ops::Not>::not")
    )]
    pub do_not_fragment: bool,

    /// If set, the destination application is requested to send an acknowledgement.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "<&bool as core::ops::Not>::not")
    )]
    pub app_ack_requested: bool,

    /// If set, status reports should include the time of the reported event.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "<&bool as core::ops::Not>::not")
    )]
    pub report_status_time: bool,

    /// If set, a status report should be generated upon bundle reception.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "<&bool as core::ops::Not>::not")
    )]
    pub receipt_report_requested: bool,

    /// If set, a status report should be generated upon bundle forwarding.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "<&bool as core::ops::Not>::not")
    )]
    pub forward_report_requested: bool,

    /// If set, a status report should be generated upon bundle delivery.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "<&bool as core::ops::Not>::not")
    )]
    pub delivery_report_requested: bool,

    /// If set, a status report should be generated upon bundle deletion.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "<&bool as core::ops::Not>::not")
    )]
    pub delete_report_requested: bool,

    /// A bitmask of any unrecognized flags encountered during parsing.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
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
        let mut flags = value.unrecognised.unwrap_or(0);
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

// A view into a bundle's blocks for BPSec operations.
struct VerifyBlockSet<'a, K: bpsec::key::KeySource + ?Sized> {
    bundle: &'a Bundle,
    source_data: &'a [u8],
    keys: &'a K,
}

impl<'a, K: bpsec::key::KeySource + ?Sized> bpsec::BlockSet<'a> for VerifyBlockSet<'a, K> {
    /// Retrieves a reference to a block by its number.
    fn block(
        &'a self,
        block_number: u64,
    ) -> Option<(&'a block::Block, Option<block::Payload<'a>>)> {
        let block = self.bundle.blocks.get(&block_number)?;
        Some((
            block,
            // Check for BCB
            if let Some(bcb_block_number) = &block.bcb {
                self.bundle
                    .decrypt_block_inner(
                        block_number,
                        *bcb_block_number,
                        self.source_data,
                        self.keys,
                    )
                    .ok()
            } else {
                block
                    .payload(self.source_data)
                    .map(block::Payload::Borrowed)
            },
        ))
    }
}

// A view into a bundle's blocks for BPSec operations.
struct DecryptBlockSet<'a, K: bpsec::key::KeySource + ?Sized> {
    inner: VerifyBlockSet<'a, K>,

    // To avoid recursion
    target: u64,
}

impl<'a, K: bpsec::key::KeySource + ?Sized> bpsec::BlockSet<'a> for DecryptBlockSet<'a, K> {
    /// Retrieves a reference to a block by its number.
    fn block(
        &'a self,
        block_number: u64,
    ) -> Option<(&'a block::Block, Option<block::Payload<'a>>)> {
        if self.target != block_number {
            self.inner.block(block_number)
        } else {
            let block = self.inner.bundle.blocks.get(&block_number)?;
            Some((
                block,
                block
                    .payload(self.inner.source_data)
                    .map(block::Payload::Borrowed),
            ))
        }
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
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub previous_node: Option<eid::Eid>,

    /// The age of the bundle, used if the source node has no clock.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub age: Option<core::time::Duration>,

    /// The hop limit and current hop count for the bundle.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
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
        let extent = array.emit(&hardy_cbor::encode::Raw(
            &primary_block::PrimaryBlock::emit(self)?,
        ));

        // Replace existing block record
        self.blocks.insert(
            0,
            primary_block::PrimaryBlock::as_block(self.crc_type, extent),
        );
        Ok(())
    }

    /// Retrieves the payload of a specific block by its number.
    ///
    /// This method handles the complexity of block-level security. If the target
    /// block is encrypted with a Block Confidentiality Block (BCB), this method
    /// will attempt to decrypt it using the provided `key_source` key source.
    pub fn block_data<'a, K>(
        &self,
        block_number: u64,
        source_data: &'a [u8],
        key_source: &K,
    ) -> Result<block::Payload<'a>, Error>
    where
        K: bpsec::key::KeySource + ?Sized,
    {
        let target_block = self
            .blocks
            .get(&block_number)
            .ok_or(Error::MissingBlock(block_number))?;

        // Check for BCB
        if let Some(bcb_block_number) = &target_block.bcb {
            self.decrypt_block_inner(block_number, *bcb_block_number, source_data, key_source)
        } else {
            target_block
                .payload(source_data)
                .map(block::Payload::Borrowed)
                .ok_or(Error::Altered)
        }
    }

    /// Retrieves the payload of a specific block by its number.
    ///
    /// This method handles the complexity of block-level security. If the target
    /// block is encrypted with a Block Confidentiality Block (BCB), this method
    /// will attempt to decrypt it using the provided `key_source` key source.
    pub fn decrypt_block_data<'a, K>(
        &self,
        block_number: u64,
        source_data: &'a [u8],
        key_source: &K,
    ) -> Result<block::Payload<'a>, Error>
    where
        K: bpsec::key::KeySource + ?Sized,
    {
        let bcb_block_number = self
            .blocks
            .get(&block_number)
            .ok_or(Error::MissingBlock(block_number))?
            .bcb
            .ok_or(Error::InvalidBPSec(bpsec::Error::NotEncrypted))?;

        self.decrypt_block_inner(block_number, bcb_block_number, source_data, key_source)
    }

    fn decrypt_block_inner<'a, K>(
        &self,
        target: u64,
        bcb_block_number: u64,
        source_data: &'a [u8],
        key_source: &K,
    ) -> Result<block::Payload<'a>, Error>
    where
        K: bpsec::key::KeySource + ?Sized,
    {
        let bcb_block = self.blocks.get(&bcb_block_number).ok_or(Error::Altered)?;

        let bcb = hardy_cbor::decode::parse::<bpsec::bcb::OperationSet>(
            bcb_block.payload(source_data).ok_or(Error::Altered)?,
        )
        .map_err(|e| Error::InvalidField {
            field: "BCB Abstract Syntax Block",
            source: e.into(),
        })?;

        // Confirm we can decrypt if we have keys
        bcb.operations
            .get(&target)
            .ok_or(Error::Altered)?
            .decrypt(
                key_source,
                bpsec::bcb::OperationArgs {
                    bpsec_source: &bcb.source,
                    target,
                    source: bcb_block_number,
                    blocks: &DecryptBlockSet {
                        inner: VerifyBlockSet {
                            bundle: self,
                            source_data,
                            keys: key_source,
                        },
                        target,
                    },
                },
            )
            .map(block::Payload::Decrypted)
            .map_err(Error::InvalidBPSec)
    }

    /// Verifies the payload of a specific block by its number.
    ///
    /// This method handles the complexity of block-level security. If the target
    /// block is encrypted with a Block Integrity Block (BIB), this method
    /// will attempt to verify it using the provided `key_source` key source.
    /// Returns a boolean indicating if the block had a BIB
    pub fn verify_block<K>(
        &self,
        block_number: u64,
        source_data: &[u8],
        key_source: &K,
    ) -> Result<bool, Error>
    where
        K: bpsec::key::KeySource + ?Sized,
    {
        let target_block = self
            .blocks
            .get(&block_number)
            .ok_or(Error::MissingBlock(block_number))?;

        // Check for BIB
        let bib_block_number = match &target_block.bib {
            block::BibCoverage::Some(n) => n,
            block::BibCoverage::None => return Ok(false),
            block::BibCoverage::Maybe => return Err(bpsec::Error::MaybeHasBib(block_number).into()),
        };

        let bib_block = self.blocks.get(bib_block_number).ok_or(Error::Altered)?;

        // Check for BCB
        let bib_data = if let Some(bcb_block_number) = &bib_block.bcb {
            self.decrypt_block_inner(
                *bib_block_number,
                *bcb_block_number,
                source_data,
                key_source,
            )?
        } else {
            bib_block
                .payload(source_data)
                .map(block::Payload::Borrowed)
                .ok_or(Error::Altered)?
        };

        let bib = hardy_cbor::decode::parse::<bpsec::bib::OperationSet>(bib_data.as_ref())
            .map_err(|e| Error::InvalidField {
                field: "BIB Abstract Syntax Block",
                source: e.into(),
            })?;

        // Confirm we can verify if we have keys
        bib.operations
            .get(&block_number)
            .ok_or(Error::Altered)?
            .verify(
                key_source,
                bpsec::bib::OperationArgs {
                    bpsec_source: &bib.source,
                    target: block_number,
                    source: *bib_block_number,
                    blocks: &VerifyBlockSet {
                        bundle: self,
                        source_data,
                        keys: key_source,
                    },
                },
            )
            .map_err(Error::InvalidBPSec)
            .map(|_| true)
    }
}

/// Represents the result of parsing and rewriting a bundle.
///
/// Parsing a bundle can have several outcomes depending on its validity,
/// canonical form, and the presence of security features.
#[derive(Debug)]
pub enum RewrittenBundle {
    /// The bundle was parsed successfully and was in canonical form.
    /// The boolean indicates if an unsupported block was encountered that requests a report.
    Valid {
        bundle: Bundle,
        report_unsupported: bool,
    },
    /// The bundle was valid but not in canonical CBOR form. A rewritten, canonical
    /// version of the bundle data is provided. The booleans indicate if a report
    /// is requested for an unsupported block and if the rewrite was due to non-canonical CBOR.
    Rewritten {
        bundle: Bundle,
        new_data: Box<[u8]>,
        report_unsupported: bool,
        non_canonical: bool,
    },
    /// The bundle was invalid. The partially-parsed `Bundle` is provided along with
    /// a `ReasonCode` for status reports and the specific `Error` that occurred.
    Invalid {
        bundle: Bundle,
        reason: status_report::ReasonCode,
        error: Error,
    },
}

/// Represents the result of parsing a bundle without rewriting.
///
/// Parsing a bundle can have several outcomes depending on its validity,
/// canonical form, and the presence of security features.
#[derive(Debug)]
pub struct ParsedBundle {
    pub bundle: Bundle,
    pub report_unsupported: bool,
    pub non_canonical: bool,
}

/// Result of parsing a bundle with canonicalization but no block removal.
///
/// Used for validating locally-originated bundles from Services.
/// This variant:
/// - DOES rewrite to canonical CBOR form (if needed)
/// - DOES perform BPSec validation (if keys provided)
/// - DOES NOT remove unknown extension blocks
/// - DOES NOT remove blocks with `delete_block_on_failure` flag
#[derive(Debug)]
pub struct CheckedBundle {
    /// The parsed bundle structure.
    pub bundle: Bundle,
    /// The rewritten bundle data if canonicalization was needed, `None` if already canonical.
    pub new_data: Option<Box<[u8]>>,
    /// True if an unsupported block was encountered that requests a report.
    pub report_unsupported: bool,
}
