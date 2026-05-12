/*!
Semantic comparison of BPv7 bundles, accounting for the encoding freedoms
in RFC 9171, RFC 9172, and RFC 9173.

See `bpv7/docs/bundle_compare.md` for the full design.
*/

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Display;

use hardy_cbor::decode::{self, FromCbor};

use crate::block::{Block, Type};
use crate::bpsec::{bcb, bib, no_keys};
use crate::bundle::{Bundle, ParsedBundle};
use crate::eid::Eid;
use crate::{Error, HashMap};

/// Comparison mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CompareMode {
    /// Bundles must be *identical*: two different encodings of the same bundle.
    /// Only CBOR encoding freedoms are tolerated.
    #[default]
    Strict,
    /// Bundles must be *equivalent*: same semantic content, but CRC type
    /// differences are tolerated (e.g., after security block removal where
    /// CRC restoration is policy-dependent per RFC 9173 Section 3.8.2, 4.8.2).
    Relaxed,
}

/// Compare two bundles from their raw bytes using strict mode.
///
/// Returns a list of human-readable differences. Empty means identical.
pub fn compare_bundles(data_a: &[u8], data_b: &[u8]) -> Result<Vec<String>, Error> {
    compare_bundles_with_mode(data_a, data_b, CompareMode::Strict)
}

/// Compare two bundles from their raw bytes with the given mode.
///
/// Returns a list of human-readable differences. Empty means identical/equivalent.
pub fn compare_bundles_with_mode(
    data_a: &[u8],
    data_b: &[u8],
    mode: CompareMode,
) -> Result<Vec<String>, Error> {
    let parsed_a = ParsedBundle::parse(data_a, no_keys)?;
    let parsed_b = ParsedBundle::parse(data_b, no_keys)?;

    let side_a = BundleSide::new(&parsed_a.bundle, data_a);
    let side_b = BundleSide::new(&parsed_b.bundle, data_b);

    Ok(compare_parsed(&side_a, &side_b, mode))
}

/// One side of a bundle comparison: parsed bundle, raw data, and precomputed
/// block-type grouping and index map.
struct BundleSide<'a> {
    bundle: &'a Bundle,
    data: &'a [u8],
    /// Blocks grouped by type code: (Type, sorted block numbers).
    by_type: BTreeMap<u64, (Type, Vec<u64>)>,
    /// Block number to (block_type, index) for target resolution.
    index: BTreeMap<u64, (Type, usize)>,
}

impl<'a> BundleSide<'a> {
    fn new(bundle: &'a Bundle, data: &'a [u8]) -> Self {
        let by_type = blocks_by_type(bundle);
        let mut index = BTreeMap::new();
        index.insert(0, (Type::Primary, 0));
        for (bt, bns) in by_type.values() {
            for (idx, bn) in bns.iter().enumerate() {
                index.insert(*bn, (*bt, idx));
            }
        }
        Self {
            bundle,
            data,
            by_type,
            index,
        }
    }
}

/// Compare two already-parsed bundles.
fn compare_parsed(a: &BundleSide, b: &BundleSide, mode: CompareMode) -> Vec<String> {
    let mut diffs = Vec::new();
    let ignore_crc = mode == CompareMode::Relaxed;

    compare_primary(a.bundle, b.bundle, ignore_crc, &mut diffs);

    let types_a: BTreeSet<_> = a.by_type.keys().collect();
    let types_b: BTreeSet<_> = b.by_type.keys().collect();

    for t in types_a.difference(&types_b) {
        diffs.push(format!("{:?}: present in A, missing in B", a.by_type[t].0));
    }
    for t in types_b.difference(&types_a) {
        diffs.push(format!("{:?}: missing in A, present in B", b.by_type[t].0));
    }

    for type_code in types_a.intersection(&types_b) {
        let (bt, ref a_bns) = a.by_type[type_code];
        let (_, ref b_bns) = b.by_type[type_code];

        if a_bns.len() != b_bns.len() {
            diffs.push(format!(
                "{bt:?}: {} block(s) in A, {} in B",
                a_bns.len(),
                b_bns.len()
            ));
            continue;
        }

        for (i, (a_bn, b_bn)) in a_bns.iter().zip(b_bns.iter()).enumerate() {
            let tag = if a_bns.len() > 1 {
                format!("{bt:?}[{i}]")
            } else {
                format!("{bt:?}")
            };

            let blk_a = &a.bundle.blocks[a_bn];
            let blk_b = &b.bundle.blocks[b_bn];

            if blk_a.flags != blk_b.flags {
                diffs.push(format!("{tag}: flags differ"));
            }
            if !ignore_crc && blk_a.crc_type != blk_b.crc_type {
                diffs.push(format!(
                    "{tag}: CRC type {:?} vs {:?}",
                    blk_a.crc_type, blk_b.crc_type
                ));
            }

            match bt {
                Type::BlockIntegrity if blk_a.bcb.is_none() && blk_b.bcb.is_none() => {
                    compare_security_block::<bib::OperationSet>(
                        blk_a, blk_b, a, b, &tag, &mut diffs,
                    );
                }
                Type::BlockSecurity => {
                    compare_security_block::<bcb::OperationSet>(
                        blk_a, blk_b, a, b, &tag, &mut diffs,
                    );
                }
                _ => {
                    compare_block_data(blk_a, a.data, blk_b, b.data, &tag, &mut diffs);
                }
            }
        }
    }

    diffs
}

/// Compare primary block parsed fields.
fn compare_primary(a: &Bundle, b: &Bundle, ignore_crc: bool, diffs: &mut Vec<String>) {
    if a.id != b.id {
        diffs.push("Primary: id differs".into());
    }
    if a.destination != b.destination {
        diffs.push("Primary: destination differs".into());
    }
    if a.report_to != b.report_to {
        diffs.push("Primary: report_to differs".into());
    }
    if a.lifetime != b.lifetime {
        diffs.push("Primary: lifetime differs".into());
    }
    if a.flags != b.flags {
        diffs.push("Primary: flags differ".into());
    }
    if !ignore_crc && a.crc_type != b.crc_type {
        diffs.push(format!(
            "Primary: CRC type {:?} vs {:?}",
            a.crc_type, b.crc_type
        ));
    }
}

/// Compare block data bytes.
fn compare_block_data(
    blk_a: &Block,
    data_a: &[u8],
    blk_b: &Block,
    data_b: &[u8],
    tag: &str,
    diffs: &mut Vec<String>,
) {
    match (blk_a.payload(data_a), blk_b.payload(data_b)) {
        (Some(a_bytes), Some(b_bytes)) if a_bytes != b_bytes => {
            diffs.push(format!(
                "{tag}: data differs ({} vs {} bytes)",
                a_bytes.len(),
                b_bytes.len()
            ));
        }
        (None, _) | (_, None) => {
            diffs.push(format!("{tag}: data unavailable"));
        }
        _ => {}
    }
}

/// Group block numbers by type code. Returns (Type, sorted block numbers).
fn blocks_by_type(bundle: &Bundle) -> BTreeMap<u64, (Type, Vec<u64>)> {
    let mut map: BTreeMap<u64, (Type, Vec<u64>)> = BTreeMap::new();
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

/// Resolve target block numbers to (block_type, index) tuples.
fn resolve_targets(
    targets: &BTreeSet<&u64>,
    index_map: &BTreeMap<u64, (Type, usize)>,
) -> BTreeSet<(Type, usize)> {
    targets
        .iter()
        .filter_map(|&&bn| index_map.get(&bn).copied())
        .collect()
}

/// Trait abstracting over BIB and BCB operation sets for the generic
/// `compare_security_block` function.
trait OperationSet: FromCbor<Error: Display + From<decode::Error>> {
    type Operation;
    fn source(&self) -> &Eid;
    fn operations(&self) -> &HashMap<u64, Self::Operation>;
    fn compare_operation(
        a: &Self::Operation,
        b: &Self::Operation,
        tag: &str,
        target: (Type, usize),
        diffs: &mut Vec<String>,
    );
}

impl OperationSet for bib::OperationSet {
    type Operation = bib::Operation;
    fn source(&self) -> &Eid {
        &self.source
    }
    fn operations(&self) -> &HashMap<u64, Self::Operation> {
        &self.operations
    }
    fn compare_operation(
        a: &bib::Operation,
        b: &bib::Operation,
        tag: &str,
        target: (Type, usize),
        diffs: &mut Vec<String>,
    ) {
        match (a, b) {
            #[cfg(feature = "rfc9173")]
            (bib::Operation::HMAC_SHA2(a), bib::Operation::HMAC_SHA2(b)) => {
                if a.parameters != b.parameters {
                    diffs.push(format!("{tag}: parameters for target {target:?} differ"));
                }
                if a.results.0 != b.results.0 {
                    diffs.push(format!("{tag}: HMAC for target {target:?} differs"));
                }
            }
            _ => {
                diffs.push(format!("{tag}: context mismatch for target {target:?}"));
            }
        }
    }
}

impl OperationSet for bcb::OperationSet {
    type Operation = bcb::Operation;
    fn source(&self) -> &Eid {
        &self.source
    }
    fn operations(&self) -> &HashMap<u64, Self::Operation> {
        &self.operations
    }
    fn compare_operation(
        a: &bcb::Operation,
        b: &bcb::Operation,
        tag: &str,
        target: (Type, usize),
        diffs: &mut Vec<String>,
    ) {
        match (a, b) {
            #[cfg(feature = "rfc9173")]
            (bcb::Operation::AES_GCM(a), bcb::Operation::AES_GCM(b)) => {
                if a.parameters != b.parameters {
                    diffs.push(format!("{tag}: parameters for target {target:?} differ"));
                }
                if a.results.0 != b.results.0 {
                    diffs.push(format!("{tag}: ciphertext for target {target:?} differs"));
                }
            }
            _ => {
                diffs.push(format!("{tag}: context mismatch for target {target:?}"));
            }
        }
    }
}

/// Compare two security blocks (BIB or BCB) semantically.
///
/// Parses both ASBs, compares source EID and resolved targets, then delegates
/// per-target operation comparison to `S::compare_operation`.
fn compare_security_block<S: OperationSet>(
    blk_a: &Block,
    blk_b: &Block,
    a: &BundleSide,
    b: &BundleSide,
    tag: &str,
    diffs: &mut Vec<String>,
) {
    let (Some(a_data), Some(b_data)) = (blk_a.payload(a.data), blk_b.payload(b.data)) else {
        diffs.push(format!("{tag}: data unavailable"));
        return;
    };

    let Ok(set_a) = decode::parse::<S>(a_data) else {
        diffs.push(format!("{tag}: failed to parse ASB in A"));
        return;
    };
    let Ok(set_b) = decode::parse::<S>(b_data) else {
        diffs.push(format!("{tag}: failed to parse ASB in B"));
        return;
    };

    if set_a.source() != set_b.source() {
        diffs.push(format!(
            "{tag}: source {} vs {}",
            set_a.source(),
            set_b.source()
        ));
    }

    let targets_a: BTreeSet<_> = set_a.operations().keys().collect();
    let targets_b: BTreeSet<_> = set_b.operations().keys().collect();

    let resolved_a = resolve_targets(&targets_a, &a.index);
    let resolved_b = resolve_targets(&targets_b, &b.index);

    if resolved_a != resolved_b {
        diffs.push(format!("{tag}: targets {resolved_a:?} vs {resolved_b:?}"));
        return;
    }

    let r2raw_a: BTreeMap<_, _> = targets_a
        .iter()
        .filter_map(|&&bn| a.index.get(&bn).map(|&r| (r, bn)))
        .collect();
    let r2raw_b: BTreeMap<_, _> = targets_b
        .iter()
        .filter_map(|&&bn| b.index.get(&bn).map(|&r| (r, bn)))
        .collect();

    for resolved in &resolved_a {
        S::compare_operation(
            &set_a.operations()[&r2raw_a[resolved]],
            &set_b.operations()[&r2raw_b[resolved]],
            tag,
            *resolved,
            diffs,
        );
    }
}
