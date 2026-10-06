/*!
This module defines the structure and components of a BPv7 block, which is the
fundamental unit of a bundle. It includes definitions for block headers, flags,
and the generic `Block` struct that represents all extension blocks.
*/

use alloc::boxed::Box;
use core::{
    cmp::Ordering,
    fmt,
    hash::{Hash, Hasher},
    ops::Range,
};

use hardy_cbor::{
    decode::{FromCbor, parse_exact},
    encode::{Array, Encoder, Raw, ToCbor, emit_array},
};

use crate::{Error, crc};
/// Represents the processing control flags for a BPv7 block.
///
/// These flags, defined in RFC 9171 Section 4.2.4, control how a node should
/// process the block, especially in cases of failure or fragmentation.
///
/// A bit carried in [`unrecognised`](Self::unrecognised) that names a flag
/// is an alias of that flag: it encodes as the flag's bit, so the next
/// parser reads the flag set. Equality and hashing compare what the value
/// encodes, so an alias equals its named flag; code that reads the named
/// fields must [`canonicalize`](Self::canonicalize) first. Parsed values
/// are canonical, serde canonicalizes in both directions, and the methods
/// that take block flags canonicalize them:
/// [`builder::BlockBuilder::with_flags`](crate::builder::BlockBuilder::with_flags),
/// [`editor::BlockBuilder::with_flags`](crate::editor::BlockBuilder::with_flags),
/// and [`ExtensionEditor::insert`](crate::extension_editor::ExtensionEditor::insert).
/// Direct field writes are not canonicalized.
#[derive(Default, Debug, Clone)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(from = "FlagsRepr", into = "FlagsRepr")
)]
pub struct Flags {
    /// If set, the block must be replicated in every fragment of the bundle.
    pub must_replicate: bool,
    /// If set, a status report should be generated if block processing fails.
    pub report_on_failure: bool,
    /// If set, the entire bundle should be deleted if block processing fails.
    pub delete_bundle_on_failure: bool,
    /// If set, this block should be deleted if its processing fails.
    pub delete_block_on_failure: bool,

    /// A bitmask of the flag bits this implementation does not name; zero
    /// when there are none.
    ///
    /// Encoding carries every bit set here; a parsed `Flags` never holds a
    /// named bit in it.
    pub unrecognised: u64,
}

impl PartialEq for Flags {
    fn eq(&self, other: &Self) -> bool {
        u64::from(self) == u64::from(other)
    }
}

impl Eq for Flags {}

impl Hash for Flags {
    fn hash<H: Hasher>(&self, state: &mut H) {
        u64::from(self).hash(state);
    }
}

// The serde form: the same fields as `Flags`, so the stored shape is the
// plain field list; `Flags` converts through it, canonicalizing both ways.
#[cfg(feature = "serde")]
#[derive(serde::Serialize, serde::Deserialize)]
struct FlagsRepr {
    #[serde(default, skip_serializing_if = "<&bool as core::ops::Not>::not")]
    must_replicate: bool,
    #[serde(default, skip_serializing_if = "<&bool as core::ops::Not>::not")]
    report_on_failure: bool,
    #[serde(default, skip_serializing_if = "<&bool as core::ops::Not>::not")]
    delete_bundle_on_failure: bool,
    #[serde(default, skip_serializing_if = "<&bool as core::ops::Not>::not")]
    delete_block_on_failure: bool,
    #[serde(default, skip_serializing_if = "is_zero")]
    unrecognised: u64,
}

#[cfg(feature = "serde")]
impl From<FlagsRepr> for Flags {
    fn from(repr: FlagsRepr) -> Self {
        Self {
            must_replicate: repr.must_replicate,
            report_on_failure: repr.report_on_failure,
            delete_bundle_on_failure: repr.delete_bundle_on_failure,
            delete_block_on_failure: repr.delete_block_on_failure,
            unrecognised: repr.unrecognised,
        }
        .canonicalize()
    }
}

#[cfg(feature = "serde")]
impl From<Flags> for FlagsRepr {
    fn from(flags: Flags) -> Self {
        let flags = flags.canonicalize();
        Self {
            must_replicate: flags.must_replicate,
            report_on_failure: flags.report_on_failure,
            delete_bundle_on_failure: flags.delete_bundle_on_failure,
            delete_block_on_failure: flags.delete_block_on_failure,
            unrecognised: flags.unrecognised,
        }
    }
}

// serde's `skip_serializing_if` predicate for the `unrecognised` fields.
#[cfg(feature = "serde")]
pub(crate) const fn is_zero(bits: &u64) -> bool {
    *bits == 0
}

impl Flags {
    /// Folds every bit of [`unrecognised`](Self::unrecognised) that names a
    /// flag into its named field; genuinely unrecognised bits are kept.
    ///
    /// A hand-built `unrecognised` encodes bit for bit, so `1 << 1`
    /// *is* `report_on_failure` to the next parser. Policy that reads the
    /// named fields must canonicalize first, or it misreads the flags the
    /// bytes carry.
    #[must_use]
    pub fn canonicalize(self) -> Self {
        Self::from(u64::from(&self))
    }

    /// Whether no bit of [`unrecognised`](Self::unrecognised) names a flag —
    /// the form [`canonicalize`](Self::canonicalize) returns. Equality
    /// cannot tell an alias from its canonical form; this can.
    #[must_use]
    pub fn is_canonical(&self) -> bool {
        Self::from(self.unrecognised).unrecognised == self.unrecognised
    }
}

impl From<&Flags> for u64 {
    fn from(value: &Flags) -> Self {
        let mut flags = value.unrecognised;
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

        if (value & (1 << 0)) != 0 {
            flags.must_replicate = true;
            unrecognised &= !(1 << 0);
        }
        if (value & (1 << 1)) != 0 {
            flags.report_on_failure = true;
            unrecognised &= !(1 << 1);
        }
        if (value & (1 << 2)) != 0 {
            flags.delete_bundle_on_failure = true;
            unrecognised &= !(1 << 2);
        }
        if (value & (1 << 4)) != 0 {
            flags.delete_block_on_failure = true;
            unrecognised &= !(1 << 4);
        }

        flags.unrecognised = unrecognised;
        flags
    }
}

impl ToCbor for Flags {
    type Result = ();

    fn to_cbor(&self, encoder: &mut Encoder) -> Self::Result {
        encoder.emit(&u64::from(self))
    }
}

impl FromCbor for Flags {
    type Error = Error;

    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        let (value, len) = crate::error::parse_canonical::<u64, _>(data, Error::NotCanonical)?;
        Ok((value.into(), true, len))
    }
}

impl Flags {
    /// The processing-control flags for a primary block (RFC 9171 §4.2.3):
    /// must-replicate, report-on-failure, and delete-bundle-on-failure set.
    ///
    /// Nominal: the primary block has no flags field on the wire, so these
    /// describe block 0's entry in a bundle's block map and are never
    /// encoded — a bundle that forbids `report_on_failure` on its blocks
    /// (see [`PrimaryBlock::forbids_report_on_failure`](crate::primary_block::PrimaryBlock::forbids_report_on_failure))
    /// still shows it here.
    pub fn primary() -> Self {
        Self {
            must_replicate: true,
            report_on_failure: true,
            delete_bundle_on_failure: true,
            delete_block_on_failure: false,
            unrecognised: 0,
        }
    }
}

/// The type of a BPv7 block, as defined in RFC 9171 Section 4.2.1.
///
/// An [`Unrecognised`](Type::Unrecognised) code that names a known type is
/// an alias of that type: it encodes as the type's code. Equality, ordering
/// and hashing compare the code, so an alias equals its named variant;
/// pattern matching does not, so code that matches on named variants must
/// [`canonicalize`](Type::canonicalize) first. Serde canonicalizes in both
/// directions.
#[derive(Debug, Copy, Clone)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(from = "TypeRepr", into = "TypeRepr")
)]
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

impl Type {
    /// Folds an [`Unrecognised`](Type::Unrecognised) alias of a known type
    /// code back to its named variant; named variants and genuinely
    /// unrecognised codes are returned unchanged.
    ///
    /// A hand-built `Unrecognised(v)` encodes as the raw code `v` on the
    /// wire, so `Unrecognised(11)` *is* a Block Integrity Block to the next
    /// parser. Policy that matches on named variants must canonicalize
    /// first, or it misreads the type the bytes carry.
    #[must_use]
    pub fn canonicalize(self) -> Self {
        Self::from(u64::from(self))
    }

    /// Whether this is not an [`Unrecognised`](Type::Unrecognised) alias of
    /// a known code — the form [`canonicalize`](Type::canonicalize) returns.
    /// Equality cannot tell an alias from its named variant; this can.
    #[must_use]
    pub fn is_canonical(&self) -> bool {
        match self {
            Self::Unrecognised(code) => matches!(Self::from(*code), Self::Unrecognised(_)),
            _ => true,
        }
    }
}

impl PartialEq for Type {
    fn eq(&self, other: &Self) -> bool {
        u64::from(*self) == u64::from(*other)
    }
}

impl Eq for Type {}

impl PartialOrd for Type {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Type {
    fn cmp(&self, other: &Self) -> Ordering {
        u64::from(*self).cmp(&u64::from(*other))
    }
}

impl Hash for Type {
    fn hash<H: Hasher>(&self, state: &mut H) {
        u64::from(*self).hash(state);
    }
}

// The serde form: the same variants as `Type`, so the stored shape is
// unchanged; `Type` converts through it, canonicalizing both ways.
#[cfg(feature = "serde")]
#[derive(serde::Serialize, serde::Deserialize)]
enum TypeRepr {
    Primary,
    Payload,
    PreviousNode,
    BundleAge,
    HopCount,
    BlockIntegrity,
    BlockSecurity,
    Unrecognised(u64),
}

#[cfg(feature = "serde")]
impl From<TypeRepr> for Type {
    fn from(repr: TypeRepr) -> Self {
        match repr {
            TypeRepr::Primary => Self::Primary,
            TypeRepr::Payload => Self::Payload,
            TypeRepr::PreviousNode => Self::PreviousNode,
            TypeRepr::BundleAge => Self::BundleAge,
            TypeRepr::HopCount => Self::HopCount,
            TypeRepr::BlockIntegrity => Self::BlockIntegrity,
            TypeRepr::BlockSecurity => Self::BlockSecurity,
            TypeRepr::Unrecognised(code) => Self::Unrecognised(code),
        }
        .canonicalize()
    }
}

#[cfg(feature = "serde")]
impl From<Type> for TypeRepr {
    fn from(block_type: Type) -> Self {
        match block_type.canonicalize() {
            Type::Primary => Self::Primary,
            Type::Payload => Self::Payload,
            Type::PreviousNode => Self::PreviousNode,
            Type::BundleAge => Self::BundleAge,
            Type::HopCount => Self::HopCount,
            Type::BlockIntegrity => Self::BlockIntegrity,
            Type::BlockSecurity => Self::BlockSecurity,
            Type::Unrecognised(code) => Self::Unrecognised(code),
        }
    }
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

impl ToCbor for Type {
    type Result = ();

    fn to_cbor(&self, encoder: &mut Encoder) -> Self::Result {
        encoder.emit(&u64::from(*self))
    }
}

impl FromCbor for Type {
    type Error = Error;

    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        let (value, len) = crate::error::parse_canonical::<u64, _>(data, Error::NotCanonical)?;
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
    /// A borrowed slice: the block's wire bytes within the original bundle
    /// data, or — when lent by a caching reader such as
    /// [`DecryptingReader`](crate::bpsec::DecryptingReader) — a decrypted
    /// payload owned by the reader for its lifetime. Only take it as a
    /// sub-slice of the bundle buffer where the lender guarantees that.
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

impl fmt::Debug for Payload<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
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
    /// it's unknown whether any of them target this block. The parser
    /// marks only BCB-covered blocks `Maybe`: as built under RFC 9172 §3.9,
    /// an encrypted BIB targets only blocks a BCB also covers. A partial
    /// acceptor that decrypts a target but not the BIB over it leaves a
    /// block no BCB covers under an encrypted BIB; that block reads `None`.
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
        let (Ok(start), Ok(end)) = (usize::try_from(r.start), usize::try_from(r.end)) else {
            return None;
        };
        source.get(start..end)
    }

    /// Decode this block's payload (from `source`) as a single CBOR `T`, with a
    /// smuggling check — no trailing bytes after the item (see
    /// [`hardy_cbor::decode::parse_exact`]). `Ok(None)` if there is no plaintext
    /// to decode in place: the payload's bytes aren't resident in `source` (an
    /// over-claiming extent in a headers-only buffer), or the block is
    /// BCB-covered so its wire bytes are ciphertext — decode the decrypted
    /// plaintext with [`hardy_cbor::decode::parse_exact`] instead.
    ///
    /// A decode failure surfaces as `T`'s own error via [`Error`]'s `From`
    /// conversions; the decode is canonical iff `T`'s `FromCbor` is.
    pub fn extract<T>(&self, source: &[u8]) -> Result<Option<T>, Error>
    where
        T: FromCbor,
        T::Error: From<hardy_cbor::decode::Error>,
        Error: From<T::Error>,
    {
        if self.bcb.is_some() {
            return Ok(None);
        }
        self.payload(source)
            .map(parse_exact)
            .transpose()
            .map_err(Error::from)
    }

    /// Emits the block as a CBOR-encoded byte array.
    /// This is an internal function used during bundle creation.
    pub(crate) fn emit(
        &mut self,
        block_number: u64,
        data: &[u8],
        array: &mut Array,
    ) -> Result<(), Error> {
        let extent = array.emit(&Raw(&crc::append_crc_value(
            self.crc_type,
            emit_array(
                Some(if matches!(self.crc_type, crc::CrcType::None) {
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
                    if !matches!(self.crc_type, crc::CrcType::None) {
                        a.skip_value();
                    }
                },
            ),
        )?));
        self.extent = extent.start as u64..extent.end as u64;
        Ok(())
    }
}
