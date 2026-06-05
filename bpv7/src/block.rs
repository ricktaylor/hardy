/*!
This module defines the structure and components of a BPv7 block, which is the
fundamental unit of a bundle. It includes definitions for block headers, flags,
and the generic `Block` struct that represents all extension blocks.
*/

use super::*;
use core::ops::Range;

/// Represents the processing control flags for a BPv7 block.
///
/// These flags, defined in RFC 9171 Section 4.2.2, control how a node should
/// process the block, especially in cases of failure or fragmentation.
#[derive(Default, Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Flags {
    /// If set, the block must be replicated in every fragment of the bundle.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "<&bool as core::ops::Not>::not")
    )]
    pub must_replicate: bool,
    /// If set, a status report should be generated if block processing fails.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "<&bool as core::ops::Not>::not")
    )]
    pub report_on_failure: bool,
    /// If set, the entire bundle should be deleted if block processing fails.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "<&bool as core::ops::Not>::not")
    )]
    pub delete_bundle_on_failure: bool,
    /// If set, this block should be deleted if its processing fails.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "<&bool as core::ops::Not>::not")
    )]
    pub delete_block_on_failure: bool,

    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    /// A bitmask of any unrecognized flags encountered during parsing.
    pub unrecognised: Option<u64>,
}

impl From<&Flags> for u64 {
    fn from(value: &Flags) -> Self {
        let mut flags = value.unrecognised.unwrap_or(0);
        if value.must_replicate {
            flags |= 1 << 0;
        }
        if value.report_on_failure {
            flags |= 1 << 1;
        }
        if value.delete_bundle_on_failure {
            flags |= 1 << 2;
        }
        if value.delete_block_on_failure {
            flags |= 1 << 4;
        }
        flags
    }
}

impl From<u64> for Flags {
    fn from(value: u64) -> Self {
        let mut flags = Self::default();
        let mut unrecognised = value;

        if (value & 1) != 0 {
            flags.must_replicate = true;
            unrecognised &= !1;
        }
        if (value & 2) != 0 {
            flags.report_on_failure = true;
            unrecognised &= !2;
        }
        if (value & 4) != 0 {
            flags.delete_bundle_on_failure = true;
            unrecognised &= !4;
        }
        if (value & 16) != 0 {
            flags.delete_block_on_failure = true;
            unrecognised &= !16;
        }

        if unrecognised != 0 {
            flags.unrecognised = Some(unrecognised);
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

impl Flags {
    pub fn primary() -> Self {
        Self {
            must_replicate: true,
            report_on_failure: true,
            delete_bundle_on_failure: true,
            delete_block_on_failure: false,
            unrecognised: None,
        }
    }
}

/// The type of a BPv7 block, as defined in RFC 9171 Section 4.2.1.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Type {
    /// Primary Block (type code 0).
    Primary,
    /// Payload Block (type code 1).
    Payload,
    /// Previous Node Block (type code 6).
    PreviousNode,
    /// Bundle Age Block (type code 7).
    BundleAge,
    /// Hop Count Block (type code 10).
    HopCount,
    /// Block Integrity Block (from BPSec, RFC 9172).
    BlockIntegrity,
    /// Block Confidentiality Block (from BPSec, RFC 9172).
    BlockSecurity,
    /// An unrecognized block type with its type code.
    Unrecognised(u64),
}

impl From<Type> for u64 {
    fn from(value: Type) -> Self {
        match value {
            Type::Primary => 0,
            Type::Payload => 1,
            Type::PreviousNode => 6,
            Type::BundleAge => 7,
            Type::HopCount => 10,
            Type::BlockIntegrity => 11,
            Type::BlockSecurity => 12,
            Type::Unrecognised(v) => v,
        }
    }
}

impl From<u64> for Type {
    fn from(value: u64) -> Self {
        match value {
            0 => Type::Primary,
            1 => Type::Payload,
            6 => Type::PreviousNode,
            7 => Type::BundleAge,
            10 => Type::HopCount,
            11 => Type::BlockIntegrity,
            12 => Type::BlockSecurity,
            value => Type::Unrecognised(value),
        }
    }
}

impl hardy_cbor::encode::ToCbor for Type {
    type Result = ();

    fn to_cbor(&self, encoder: &mut hardy_cbor::encode::Encoder) -> Self::Result {
        encoder.emit(&u64::from(*self))
    }
}

impl hardy_cbor::decode::FromCbor for Type {
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

/// Represents the payload of a block.
///
/// The payload can either be a direct slice (`Borrowed`) into the original bundle's
/// byte array, or an `Decrypted` byte slice. The `Decrypted` variant is used when the
/// payload has been decrypted from a Block Confidentiality Block (BCB) and
/// therefore does not correspond to a contiguous region of the original data.
pub enum Payload<'a> {
    /// A slice within the original bundle data.
    Borrowed(&'a [u8]),
    /// An owned byte slice, typically holding a decrypted payload.
    Decrypted(zeroize::Zeroizing<Box<[u8]>>),
}

impl Payload<'_> {
    pub fn len(&self) -> usize {
        match self {
            Payload::Borrowed(items) => items.len(),
            Payload::Decrypted(zeroizing) => zeroizing.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Payload::Borrowed(items) => items.is_empty(),
            Payload::Decrypted(zeroizing) => zeroizing.is_empty(),
        }
    }
}

impl core::fmt::Debug for Payload<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Just delegate to the underlying slice formatter
        self.as_ref().fmt(f)
    }
}

impl AsRef<[u8]> for Payload<'_> {
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::Borrowed(arg0) => arg0,
            Self::Decrypted(arg0) => arg0.as_ref(),
        }
    }
}

/// Represents the integrity block (BIB) coverage state for a block.
///
/// This enum tracks whether a block is protected by a Block Integrity Block (BIB)
/// and handles the case where encrypted BIBs couldn't be decrypted during parsing,
/// meaning their targets are unknown.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(tag = "state", content = "block_number"))]
pub enum BibCoverage {
    /// No BIB is known to target this block.
    #[default]
    None,
    /// A BIB at the given block number targets this block.
    Some(u64),
    /// There are encrypted BIBs that couldn't be decrypted during parsing;
    /// it's unknown whether any of them target this block.
    Maybe,
}

#[cfg(feature = "serde")]
fn bib_is_none(bib: &BibCoverage) -> bool {
    matches!(bib, BibCoverage::None)
}

/// Represents a generic BPv7 extension block within a bundle.
///
/// This struct holds the common metadata for all blocks, such as the type, flags,
/// and CRC information. The actual data of the block is not stored directly but
/// is referenced by the `extent` and `data` ranges, which point to slices
/// within the full bundle's byte representation.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Block {
    /// The type of the block.
    #[cfg_attr(feature = "serde", serde(rename = "type"))]
    pub block_type: Type,
    /// The block-specific processing control flags.
    pub flags: Flags,
    /// The type of CRC used for this block's integrity check.
    pub crc_type: crc::CrcType,
    /// The BIB coverage state for this block.
    #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "bib_is_none"))]
    pub bib: BibCoverage,
    /// The block number of the Block Confidentiality Block (BCB) that protects this block, if any.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub bcb: Option<u64>,
    /// The range of bytes in the source data that this block occupies,
    /// including the CBOR array wrapper. `u64` to match the wire-stream
    /// offset domain (CBOR offsets are `u64`, streamed bundles need not
    /// fit in `usize`). Callers with the bundle in memory cast to
    /// `usize` at the slice point.
    pub extent: Range<u64>,
    /// The range of bytes within the `extent` that represents the
    /// block-specific data. `u64`, see [`Block::extent`].
    pub data: Range<u64>,
}

impl Default for Block {
    fn default() -> Self {
        Self {
            block_type: Type::Payload,
            flags: Flags::default(),
            crc_type: crc::CrcType::None,
            bib: BibCoverage::None,
            bcb: None,
            extent: 0..0,
            data: 0..0,
        }
    }
}

impl Block {
    /// Bundle-absolute byte offsets of the block's payload within the
    /// wire stream. Honest `Range<u64>` — callers that have the bundle
    /// in memory cast to `usize` at the slice point.
    pub fn payload_range(&self) -> Range<u64> {
        self.extent.start + self.data.start..self.extent.start + self.data.end
    }

    /// Returns the block's payload bytes, sliced from `source`.
    ///
    /// `source` MUST be the complete, contiguous bundle byte stream the
    /// block's offsets were parsed against (the `Bytes` returned by
    /// [`parse::parse`](crate::parse::parse), or the
    /// buffer a `Builder`/`Editor` produced) — the offsets are
    /// bundle-absolute. Returns `None` if they fall outside `source`.
    ///
    /// This is the in-memory convenience over [`Self::payload_range`]: it
    /// assumes the whole bundle is resident. When the bundle body may not
    /// be held in RAM (e.g. range-reading a large bundle from storage),
    /// use [`Self::payload_range`] and fetch the range directly instead.
    pub fn payload<'a>(&self, source: &'a [u8]) -> Option<&'a [u8]> {
        let r = self.payload_range();
        source.get(r.start as usize..r.end as usize)
    }

    /// Decode this block's payload (from `source`) as a single CBOR `T`, with a
    /// smuggling check — no trailing bytes after the item (see
    /// [`hardy_cbor::decode::parse_exact`]). `Ok(None)` if the payload's bytes
    /// aren't resident in `source` (an over-claiming extent in a headers-only
    /// buffer).
    ///
    /// The payload is assumed **plaintext**: it's the caller's contract to decrypt
    /// a BCB-protected block first and decode the plaintext with
    /// [`hardy_cbor::decode::parse_exact`] — calling this on ciphertext mis-decodes
    /// or errors. A decode failure surfaces as `T`'s own error via [`Error`]'s
    /// `From` conversions; the decode is canonical iff `T`'s `FromCbor` is.
    pub fn extract<T>(&self, source: &[u8]) -> Result<Option<T>, Error>
    where
        T: hardy_cbor::decode::FromCbor,
        T::Error: From<hardy_cbor::decode::Error>,
        Error: From<T::Error>,
    {
        self.payload(source)
            .map(hardy_cbor::decode::parse_exact)
            .transpose()
            .map_err(Error::from)
    }

    /// Emits the block as a CBOR-encoded byte array.
    /// This is an internal function used during bundle creation.
    pub(crate) fn emit(
        &mut self,
        block_number: u64,
        data: &[u8],
        array: &mut hardy_cbor::encode::Array,
    ) -> Result<(), Error> {
        let extent = array.emit(&hardy_cbor::encode::Raw(&crc::append_crc_value(
            self.crc_type,
            hardy_cbor::encode::emit_array(
                Some(if let crc::CrcType::None = self.crc_type {
                    5
                } else {
                    6
                }),
                |a| {
                    a.emit(&self.block_type);
                    a.emit(&block_number);
                    a.emit(&self.flags);
                    a.emit(&self.crc_type);

                    let data_range = a.emit(&hardy_cbor::encode::Bytes(data));
                    self.data = data_range.start as u64..data_range.end as u64;

                    // CRC
                    if let crc::CrcType::None = self.crc_type {
                    } else {
                        a.skip_value();
                    }
                },
            ),
        )?));
        self.extent = extent.start as u64..extent.end as u64;
        Ok(())
    }
}
