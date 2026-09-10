/*!
Semantic bundle comparison ([`Bundle::semantic_eq`]).

The tolerance rules this module implements — which encoding freedoms of
RFC 9171/9172/9173 compare equal — are specified in
`bpv7/docs/bundle_compare.md`; the `bundle compare` CLI in
`hardy-bpv7-tools` layers a human-readable diff on the same rules.
*/

use alloc::{
    collections::{BTreeMap, BTreeSet},
    vec::Vec,
};

use hardy_cbor::decode::{self, FromCbor};

use crate::{
    HashMap,
    bpsec::{bcb, bib},
    bundle::{Block, BlockType, Bundle},
    bundle_age, eid, hop_info,
};

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
    ///
    /// Security-block comparison is exact only for contexts this build
    /// recognises: operations with an unrecognised context id — including
    /// every BIB/BCB operation when the `rfc9173` feature is disabled —
    /// compare as *not* equal, so byte-identical bundles carrying such
    /// blocks report as semantically different. This is deliberate:
    /// without the context's semantics, parameter/result equivalence
    /// cannot be judged, and a false "equal" is the worse failure mode
    /// for the round-trip tests this serves.
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
        let by_type_a = self.blocks_by_type();
        let by_type_b = other.blocks_by_type();
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
                    BlockType::BlockIntegrity if blk_a.bcb.is_none() && blk_b.bcb.is_none() => {
                        bpsec_block_eq::<bib::OperationSet>(
                            blk_a, self_data, blk_b, other_data, &index_a, &index_b,
                        )
                    }
                    BlockType::BlockSecurity => bpsec_block_eq::<bcb::OperationSet>(
                        blk_a, self_data, blk_b, other_data, &index_a, &index_b,
                    ),
                    // Known extension blocks compare by decoded content, so a
                    // non-canonical re-encoding of the same value is equal.
                    // Encrypted bodies are opaque — fall through to raw bytes.
                    BlockType::PreviousNode | BlockType::BundleAge | BlockType::HopCount
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

/// Map each block number to its (type, position-within-type) so that
/// BPSec targets resolve across renumbering and reordering.
fn build_index(
    by_type: &BTreeMap<u64, (BlockType, Vec<u64>)>,
) -> BTreeMap<u64, (BlockType, usize)> {
    let mut index = BTreeMap::new();
    index.insert(0, (BlockType::Primary, 0));
    for (bt, bns) in by_type.values() {
        for (idx, bn) in bns.iter().enumerate() {
            index.insert(*bn, (*bt, idx));
        }
    }
    index
}

/// Compare a block's raw payload bytes.
fn block_data_eq(blk_a: &Block, data_a: &[u8], blk_b: &Block, data_b: &[u8]) -> bool {
    match (blk_a.payload(data_a), blk_b.payload(data_b)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// Compare a known extension block (PreviousNode / BundleAge / HopCount)
/// by its decoded value rather than its wire bytes.
fn known_extension_eq(
    bt: BlockType,
    blk_a: &Block,
    data_a: &[u8],
    blk_b: &Block,
    data_b: &[u8],
) -> bool {
    let (Some(a_body), Some(b_body)) = (blk_a.payload(data_a), blk_b.payload(data_b)) else {
        return false;
    };
    match bt {
        BlockType::PreviousNode => decoded_eq::<eid::Eid>(a_body, b_body),
        BlockType::BundleAge => decoded_eq::<bundle_age::BundleAge>(a_body, b_body),
        BlockType::HopCount => decoded_eq::<hop_info::HopInfo>(a_body, b_body),
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
    blk_a: &Block,
    data_a: &[u8],
    blk_b: &Block,
    data_b: &[u8],
    index_a: &BTreeMap<u64, (BlockType, usize)>,
    index_b: &BTreeMap<u64, (BlockType, usize)>,
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
    index: &BTreeMap<u64, (BlockType, usize)>,
) -> BTreeSet<(BlockType, usize)> {
    targets
        .iter()
        .filter_map(|&&bn| index.get(&bn).copied())
        .collect()
}
