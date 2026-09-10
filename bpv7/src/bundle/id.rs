/*!
The bundle identity type ([`BundleId`]) and its compact key form.

An id round-trips through a base64url-without-padding encoding of the
canonical CBOR array of its components, used as a database or wire key by
consumers.
*/

use core::fmt;

use alloc::{boxed::Box, string::String};

use base64::{Engine, prelude::BASE64_URL_SAFE_NO_PAD};
use thiserror::Error;

use crate::{creation_timestamp::CreationTimestamp, eid};

/// Errors that can occur when parsing a bundle [`BundleId`] from a key.
#[derive(Error, Debug)]
pub enum BundleIdError {
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
        source: Box<BundleIdError>,
    },

    /// An EID field failed to parse.
    #[error(transparent)]
    InvalidEid(#[from] eid::Error),

    /// A bundle-domain field (the creation timestamp) failed to parse.
    #[error(transparent)]
    InvalidBundle(Box<crate::Error>),

    /// An error occurred during CBOR decoding.
    #[error(transparent)]
    InvalidCBOR(#[from] hardy_cbor::decode::Error),
}

impl From<crate::Error> for BundleIdError {
    fn from(e: crate::Error) -> Self {
        Self::InvalidBundle(Box::new(e))
    }
}

trait CaptureFieldIdErr<T> {
    fn map_field_id_err(self, field: &'static str) -> core::result::Result<T, BundleIdError>;
}

impl<T, E: Into<BundleIdError>> CaptureFieldIdErr<T> for core::result::Result<T, E> {
    fn map_field_id_err(self, field: &'static str) -> core::result::Result<T, BundleIdError> {
        self.map_err(|e| BundleIdError::InvalidField {
            field,
            source: Box::new(e.into()),
        })
    }
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

/// Represents the unique identifier of a BPv7 bundle.
///
/// A bundle ID is a tuple of `(source EID, creation timestamp, fragment info)`.
/// This combination is guaranteed to be unique across the DTN.
#[derive(Default, Debug, Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BundleId {
    /// The EID of the node that created the bundle.
    pub source: eid::Eid,
    /// The creation timestamp, including a sequence number for uniqueness.
    pub timestamp: CreationTimestamp,
    /// Fragmentation information, if this bundle is a fragment.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub fragment_info: Option<FragmentInfo>,
}

impl BundleId {
    /// Deserializes a bundle ID from a compact, base64-encoded string representation.
    ///
    /// This is useful for using the bundle ID as a key in databases or other systems.
    pub fn from_key(k: &str) -> core::result::Result<Self, BundleIdError> {
        hardy_cbor::decode::parse_array(
            &BASE64_URL_SAFE_NO_PAD
                .decode(k)
                .map_err(BundleIdError::BadBase64)?,
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
                    Err(BundleIdError::BadKey)
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

impl fmt::Display for BundleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
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
