/*!
This module defines the core bundle data model: the [`Bundle`] structure
(primary block + blocks map) together with its identifying types
([`Id`], [`Flags`], [`FragmentInfo`]). The wire parser lives in
[`crate::parse`]; the BPSec validation/transform primitives in
[`crate::checks`] and [`crate::rewrite`].
*/

use super::*;
use base64::prelude::*;
use primary_block::PrimaryBlock;

/// A parsed BPv7 bundle: the primary block plus the extension and payload
/// blocks keyed by block number. This is the crate's structural bundle
/// representation, produced by [`parse`](crate::parse::parse) and emitted
/// by [`Builder`](crate::builder::Builder) / [`Editor`](crate::editor::Editor).
#[derive(Debug)]
pub struct Bundle {
    pub primary: PrimaryBlock,
    pub blocks: HashMap<u64, block::Block>,
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

impl<T, E: Into<Box<dyn core::error::Error + Send + Sync>>> CaptureFieldIdErr<T> for Result<T, E> {
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
            &BASE64_URL_SAFE_NO_PAD
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
        BASE64_URL_SAFE_NO_PAD.encode(
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
                "[{} @ {} fragment {}/{}]",
                self.source, self.timestamp, fi.offset, fi.total_adu_length
            )
        } else {
            write!(f, "[{} @ {}]", self.source, self.timestamp)
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
    type Error = Error;

    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        let (value, shortest, len) =
            hardy_cbor::decode::parse::<(u64, bool, usize)>(data).map_err(Error::InvalidCBOR)?;
        if !shortest {
            return Err(Error::NotCanonical);
        }
        Ok((value.into(), true, len))
    }
}
