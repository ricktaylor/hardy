/*!
This module defines the core bundle data model: the [`Bundle`] structure
(primary block + blocks map) together with its identifying types
([`Id`], [`Flags`], [`FragmentInfo`]). The wire parser lives in
[`crate::parse`]; the BPSec validation/transform primitives in
[`crate::checks`] and [`crate::rewrite`].
*/

use super::*;
use alloc::collections::{BTreeMap, BTreeSet};
use base64::prelude::*;
use bpsec::{bcb, bib};
use hardy_cbor::decode::{self, FromCbor};
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

impl Bundle {
    /// Compare this bundle against `other` for semantic equivalence,
    /// tolerating the encoding freedoms in RFC 9171, RFC 9172, and
    /// RFC 9173: block order, block numbering, non-canonical re-encodings
    /// of known extension blocks, and BPSec target ordering all compare
    /// equal. CRC type and presence are a transport choice and are
    /// ignored.
    ///
    /// Both bundles must already be parsed; `self_data` / `other_data` are
    /// the backing wire buffers their block offsets index into (the
    /// [`data`](crate::parse::Parsed::data) returned alongside each by
    /// [`parse`](crate::parse::parse)). This answers the yes/no question
    /// round-trip and conformance tests need; the `bundle compare` CLI in
    /// `hardy-bpv7-tools` layers a human-readable diff on top of the same
    /// rules.
    pub fn semantic_eq(&self, self_data: &[u8], other: &Bundle, other_data: &[u8]) -> bool {
        // Primary block — semantic fields only; CRC is a transport choice.
        let (pa, pb) = (&self.primary, &other.primary);
        if pa.id != pb.id
            || pa.destination != pb.destination
            || pa.report_to != pb.report_to
            || pa.lifetime != pb.lifetime
            || pa.flags != pb.flags
        {
            return false;
        }

        // Block identity is by type + position, not block number, so group
        // each side by type code and pair the groups up.
        let by_type_a = blocks_by_type(self);
        let by_type_b = blocks_by_type(other);
        if !by_type_a.keys().eq(by_type_b.keys()) {
            return false;
        }

        let index_a = build_index(&by_type_a);
        let index_b = build_index(&by_type_b);

        for (type_code, (bt, a_bns)) in &by_type_a {
            let (_, b_bns) = &by_type_b[type_code];
            if a_bns.len() != b_bns.len() {
                return false;
            }
            for (a_bn, b_bn) in a_bns.iter().zip(b_bns) {
                let blk_a = &self.blocks[a_bn];
                let blk_b = &other.blocks[b_bn];
                if blk_a.flags != blk_b.flags {
                    return false;
                }
                let eq = match bt {
                    block::Type::BlockIntegrity if blk_a.bcb.is_none() && blk_b.bcb.is_none() => {
                        bpsec_block_eq::<bib::OperationSet>(
                            blk_a, self_data, blk_b, other_data, &index_a, &index_b,
                        )
                    }
                    block::Type::BlockSecurity => bpsec_block_eq::<bcb::OperationSet>(
                        blk_a, self_data, blk_b, other_data, &index_a, &index_b,
                    ),
                    // Known extension blocks compare by decoded content, so a
                    // non-canonical re-encoding of the same value is equal.
                    // Encrypted bodies are opaque — fall through to raw bytes.
                    block::Type::PreviousNode | block::Type::BundleAge | block::Type::HopCount
                        if blk_a.bcb.is_none() && blk_b.bcb.is_none() =>
                    {
                        known_extension_eq(*bt, blk_a, self_data, blk_b, other_data)
                    }
                    _ => block_data_eq(blk_a, self_data, blk_b, other_data),
                };
                if !eq {
                    return false;
                }
            }
        }
        true
    }
}

/// Group block numbers by type code, each list sorted ascending. The
/// primary block (number 0) is handled separately and excluded.
fn blocks_by_type(bundle: &Bundle) -> BTreeMap<u64, (block::Type, Vec<u64>)> {
    let mut map: BTreeMap<u64, (block::Type, Vec<u64>)> = BTreeMap::new();
    for (&bn, blk) in &bundle.blocks {
        if bn == 0 {
            continue;
        }
        let type_code: u64 = blk.block_type.into();
        map.entry(type_code)
            .or_insert_with(|| (blk.block_type, Vec::new()))
            .1
            .push(bn);
    }
    for v in map.values_mut() {
        v.1.sort();
    }
    map
}

/// Map each block number to its (type, position-within-type) so that
/// BPSec targets resolve across renumbering and reordering.
fn build_index(
    by_type: &BTreeMap<u64, (block::Type, Vec<u64>)>,
) -> BTreeMap<u64, (block::Type, usize)> {
    let mut index = BTreeMap::new();
    index.insert(0, (block::Type::Primary, 0));
    for (bt, bns) in by_type.values() {
        for (idx, bn) in bns.iter().enumerate() {
            index.insert(*bn, (*bt, idx));
        }
    }
    index
}

/// Compare a block's raw payload bytes.
fn block_data_eq(blk_a: &block::Block, data_a: &[u8], blk_b: &block::Block, data_b: &[u8]) -> bool {
    match (blk_a.payload(data_a), blk_b.payload(data_b)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// Compare a known extension block (PreviousNode / BundleAge / HopCount)
/// by its decoded value rather than its wire bytes.
fn known_extension_eq(
    bt: block::Type,
    blk_a: &block::Block,
    data_a: &[u8],
    blk_b: &block::Block,
    data_b: &[u8],
) -> bool {
    let (Some(a_body), Some(b_body)) = (blk_a.payload(data_a), blk_b.payload(data_b)) else {
        return false;
    };
    match bt {
        block::Type::PreviousNode => decoded_eq::<eid::Eid>(a_body, b_body),
        block::Type::BundleAge => decoded_eq::<bundle_age::BundleAge>(a_body, b_body),
        block::Type::HopCount => decoded_eq::<hop_info::HopInfo>(a_body, b_body),
        _ => block_data_eq(blk_a, data_a, blk_b, data_b),
    }
}

/// Decode `T` from both bodies and compare the values. A non-canonical
/// encoding still compares by content — that tolerance lives in
/// `T::from_cbor`, which accepts it — but trailing bytes after the item are
/// rejected via [`decode::parse_exact`]. A decode failure on either side is
/// treated as not equal.
fn decoded_eq<T>(a_body: &[u8], b_body: &[u8]) -> bool
where
    T: FromCbor<Error: From<decode::Error>> + PartialEq,
{
    match (
        decode::parse_exact::<T>(a_body),
        decode::parse_exact::<T>(b_body),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Abstracts BIB and BCB operation sets for the generic
/// [`bpsec_block_eq`].
trait OperationSet: FromCbor<Error: From<decode::Error>> {
    type Operation;
    fn source(&self) -> &eid::Eid;
    fn operations(&self) -> &HashMap<u64, Self::Operation>;
    fn operation_eq(a: &Self::Operation, b: &Self::Operation) -> bool;
}

impl OperationSet for bib::OperationSet {
    type Operation = bib::Operation;
    fn source(&self) -> &eid::Eid {
        &self.source
    }
    fn operations(&self) -> &HashMap<u64, Self::Operation> {
        &self.operations
    }
    fn operation_eq(a: &bib::Operation, b: &bib::Operation) -> bool {
        match (a, b) {
            #[cfg(feature = "rfc9173")]
            (bib::Operation::HMAC_SHA2(a), bib::Operation::HMAC_SHA2(b)) => {
                a.parameters == b.parameters && a.results.0 == b.results.0
            }
            _ => false,
        }
    }
}

impl OperationSet for bcb::OperationSet {
    type Operation = bcb::Operation;
    fn source(&self) -> &eid::Eid {
        &self.source
    }
    fn operations(&self) -> &HashMap<u64, Self::Operation> {
        &self.operations
    }
    fn operation_eq(a: &bcb::Operation, b: &bcb::Operation) -> bool {
        match (a, b) {
            #[cfg(feature = "rfc9173")]
            (bcb::Operation::AES_GCM(a), bcb::Operation::AES_GCM(b)) => {
                a.parameters == b.parameters && a.results.0 == b.results.0
            }
            _ => false,
        }
    }
}

/// Compare two security blocks (BIB or BCB) semantically: same source
/// EID, same target set (resolved to type + position), and equal
/// per-target operations.
fn bpsec_block_eq<S: OperationSet>(
    blk_a: &block::Block,
    data_a: &[u8],
    blk_b: &block::Block,
    data_b: &[u8],
    index_a: &BTreeMap<u64, (block::Type, usize)>,
    index_b: &BTreeMap<u64, (block::Type, usize)>,
) -> bool {
    let (Some(a_data), Some(b_data)) = (blk_a.payload(data_a), blk_b.payload(data_b)) else {
        return false;
    };
    let (Ok(set_a), Ok(set_b)) = (decode::parse::<S>(a_data), decode::parse::<S>(b_data)) else {
        return false;
    };

    if set_a.source() != set_b.source() {
        return false;
    }

    let targets_a: BTreeSet<_> = set_a.operations().keys().collect();
    let targets_b: BTreeSet<_> = set_b.operations().keys().collect();
    let resolved_a = resolve_targets(&targets_a, index_a);
    let resolved_b = resolve_targets(&targets_b, index_b);
    if resolved_a != resolved_b {
        return false;
    }

    let r2raw_a: BTreeMap<_, _> = targets_a
        .iter()
        .filter_map(|&&bn| index_a.get(&bn).map(|&r| (r, bn)))
        .collect();
    let r2raw_b: BTreeMap<_, _> = targets_b
        .iter()
        .filter_map(|&&bn| index_b.get(&bn).map(|&r| (r, bn)))
        .collect();

    resolved_a.iter().all(|resolved| {
        S::operation_eq(
            &set_a.operations()[&r2raw_a[resolved]],
            &set_b.operations()[&r2raw_b[resolved]],
        )
    })
}

/// Resolve target block numbers to (type, position) tuples, dropping any
/// that don't resolve (a dangling target is caught elsewhere).
fn resolve_targets(
    targets: &BTreeSet<&u64>,
    index: &BTreeMap<u64, (block::Type, usize)>,
) -> BTreeSet<(block::Type, usize)> {
    targets
        .iter()
        .filter_map(|&&bn| index.get(&bn).copied())
        .collect()
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
