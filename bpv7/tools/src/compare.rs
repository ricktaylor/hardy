/*!
Semantic comparison of BPv7 bundles, accounting for the encoding freedoms
in RFC 9171, RFC 9172, and RFC 9173.

See `bpv7/docs/bundle_compare.md` for the full design.

Lives in `hardy-bpv7-tools` (not `hardy-bpv7`) because it's a
tool/test utility, not parser-layer library code — `bundle compare`
in the CLI calls into it, and the in-module `tests` submodule below
exercises it directly.
*/

use core::fmt::Display;
use hardy_bpv7::{
    Bundle, Error,
    block::{Block, Type},
    bpsec::{bcb, bib},
    bundle_age::BundleAge,
    crc::CrcType,
    eid::Eid,
    hop_info::HopInfo,
    parse,
};
use hardy_cbor::decode::{self, FromCbor};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Compare two bundles from their raw bytes.
///
/// Returns a list of human-readable differences. Empty means identical.
pub fn compare_bundles(data_a: &[u8], data_b: &[u8]) -> Result<Vec<String>, Error> {
    let parse::Parsed {
        data: data_a,
        bundle: bundle_a,
        ..
    } = parse::parse(bytes::Bytes::copy_from_slice(data_a))?;
    let parse::Parsed {
        data: data_b,
        bundle: bundle_b,
        ..
    } = parse::parse(bytes::Bytes::copy_from_slice(data_b))?;

    let side_a = BundleSide::new(&bundle_a, &data_a);
    let side_b = BundleSide::new(&bundle_b, &data_b);

    Ok(compare_parsed(&side_a, &side_b))
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
fn compare_parsed(a: &BundleSide, b: &BundleSide) -> Vec<String> {
    let mut diffs = Vec::new();

    compare_primary(a.bundle, b.bundle, &mut diffs);

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
            compare_crc(blk_a.crc_type, blk_b.crc_type, &tag, &mut diffs);

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
                // Known extension blocks compare by decoded *content*, so a
                // non-canonical re-encoding of the same value is equivalent
                // (the whole point of a semantic compare). Encrypted bodies
                // are opaque — fall through to the raw-bytes comparison.
                Type::PreviousNode | Type::BundleAge | Type::HopCount
                    if blk_a.bcb.is_none() && blk_b.bcb.is_none() =>
                {
                    compare_known_extension(bt, blk_a, a.data, blk_b, b.data, &tag, &mut diffs);
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
fn compare_primary(a: &Bundle, b: &Bundle, diffs: &mut Vec<String>) {
    if a.primary.id != b.primary.id {
        diffs.push("Primary: id differs".into());
    }
    if a.primary.destination != b.primary.destination {
        diffs.push("Primary: destination differs".into());
    }
    if a.primary.report_to != b.primary.report_to {
        diffs.push("Primary: report_to differs".into());
    }
    if a.primary.lifetime != b.primary.lifetime {
        diffs.push("Primary: lifetime differs".into());
    }
    if a.primary.flags != b.primary.flags {
        diffs.push("Primary: flags differ".into());
    }
    compare_crc(a.primary.crc_type, b.primary.crc_type, "Primary", diffs);
}

/// Compare CRC presence. The exact CRC type (CRC-16 vs CRC-32) is an
/// implementation choice, but having CRC vs none is semantically meaningful.
fn compare_crc(a: CrcType, b: CrcType, tag: &str, diffs: &mut Vec<String>) {
    let has_a = !matches!(a, CrcType::None);
    let has_b = !matches!(b, CrcType::None);
    if has_a != has_b {
        diffs.push(format!("{tag}: CRC {:?} vs {:?}", a, b));
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

/// Compare a known extension block (PreviousNode / BundleAge / HopCount)
/// by its decoded value rather than its wire bytes, so two encodings of
/// the same value compare equal.
fn compare_known_extension(
    bt: Type,
    blk_a: &Block,
    data_a: &[u8],
    blk_b: &Block,
    data_b: &[u8],
    tag: &str,
    diffs: &mut Vec<String>,
) {
    let (Some(a_body), Some(b_body)) = (blk_a.payload(data_a), blk_b.payload(data_b)) else {
        diffs.push(format!("{tag}: data unavailable"));
        return;
    };
    match bt {
        Type::PreviousNode => compare_decoded::<Eid>(a_body, b_body, tag, diffs),
        Type::BundleAge => compare_decoded::<BundleAge>(a_body, b_body, tag, diffs),
        Type::HopCount => compare_decoded::<HopInfo>(a_body, b_body, tag, diffs),
        _ => unreachable!("compare_known_extension called for non-extension block"),
    }
}

/// Decode `T` from both bodies and compare the values. A non-canonical
/// encoding still compares by content — that tolerance lives in
/// `T::from_cbor`, which accepts it — but trailing bytes after the item are
/// rejected via [`decode::parse_exact`]. A decode failure or value mismatch
/// is recorded as a diff.
fn compare_decoded<T>(a_body: &[u8], b_body: &[u8], tag: &str, diffs: &mut Vec<String>)
where
    T: FromCbor<Error: Display + From<decode::Error>> + PartialEq,
{
    match (
        decode::parse_exact::<T>(a_body),
        decode::parse_exact::<T>(b_body),
    ) {
        (Ok(a), Ok(b)) => {
            if a != b {
                diffs.push(format!("{tag}: content differs"));
            }
        }
        (Err(e), _) => diffs.push(format!("{tag}: failed to decode in A: {e}")),
        (_, Err(e)) => diffs.push(format!("{tag}: failed to decode in B: {e}")),
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
        bib::OperationSet::source(self)
    }
    fn operations(&self) -> &HashMap<u64, Self::Operation> {
        bib::OperationSet::operations(self)
    }
    fn compare_operation(
        a: &bib::Operation,
        b: &bib::Operation,
        tag: &str,
        target: (Type, usize),
        diffs: &mut Vec<String>,
    ) {
        // hardy-bpv7-tools hardcodes `hardy-bpv7`'s `rfc9173` feature on
        // (see Cargo.toml), so the `HMAC_SHA2` variant always exists —
        // no `#[cfg(feature = "rfc9173")]` gate needed here. If
        // bpv7-tools ever makes rfc9173 optional, add an `rfc9173`
        // feature flag here and forward it to hardy-bpv7.
        match (a, b) {
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
        bcb::OperationSet::source(self)
    }
    fn operations(&self) -> &HashMap<u64, Self::Operation> {
        bcb::OperationSet::operations(self)
    }
    fn compare_operation(
        a: &bcb::Operation,
        b: &bcb::Operation,
        tag: &str,
        target: (Type, usize),
        diffs: &mut Vec<String>,
    ) {
        match (a, b) {
            // See the BIB comment above re: no `#[cfg(feature = "rfc9173")]`.
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

#[cfg(test)]
mod tests {
    use super::compare_bundles;
    use bytes::Bytes;
    use hardy_bpv7::parse;
    use hex_literal::hex;

    // Original plain bundle (RFC 9173, Section A.3.1.4)
    const ORIGINAL: &[u8] = &hex!(
        "9F88070000820282010282028202018202820201820018281A000F42408507020000410085010100005823526561647920746F2067656E657261746520612033322D62797465207061796C6F6164FF"
    );

    // BIB signing payload (block 1) and BundleAge (block 2), source ipn:3.1.
    const BIB_BUNDLE: &[u8] = &hex!(
        "9F88070000820282010282028202018202820201820018281A000F4240850B030000587582010201008202820301828182015830F75FE4C37F76F046165855BD5FF72FBFD4E3A64B4695C40E2B787DA005AE819F0A2E30A2E8B325527DE8AEFB52E73D7181820158306EE5CA30AB3A1BF1E7F645EB21418FFC129BACFB69677FDAE0D08CB63159358FA86BE682538299B4B7E53C04FE03FDE88507020000410085010100005823526561647920746F2067656E657261746520612033322D62797465207061796C6F6164FF"
    );

    /// Run both implementations and assert they agree on equivalence.
    /// Returns the diff list for further assertions.  If they disagree,
    /// the diff text is the diagnostic.
    fn compare(a: &[u8], b: &[u8]) -> Vec<String> {
        let pa = parse::parse(Bytes::copy_from_slice(a)).expect("parse a");
        let pb = parse::parse(Bytes::copy_from_slice(b)).expect("parse b");
        let semantic = pa.bundle.semantic_eq(&pa.data, &pb.bundle, &pb.data);
        let diffs = compare_bundles(a, b).expect("compare_bundles");
        assert_eq!(
            semantic,
            diffs.is_empty(),
            "semantic_eq={semantic} disagrees with compare_bundles ({} diff(s)): {diffs:?}",
            diffs.len()
        );
        diffs
    }

    // One smoke test per major code path in compare_bundles, cross-checking
    // that it agrees with semantic_eq.  cmp.rs owns exhaustive equivalence
    // coverage; these four catch divergence between the two implementations.

    #[test]
    fn identical_bundles() {
        assert!(compare(ORIGINAL, ORIGINAL).is_empty());
    }

    #[test]
    fn different_extension_content_detected() {
        // HopCount limit 30 vs 99 — exercises the decoded known-extension path.
        let limit_30 = hex!(
            "9F88070000820282010282028202018202820201820018281A000F4240"
            "85070200004100"
            "850A0300004482181E00"
            "85010100005823526561647920746F2067656E657261746520612033322D62797465207061796C6F6164"
            "FF"
        );
        let limit_99 = hex!(
            "9F88070000820282010282028202018202820201820018281A000F4240"
            "85070200004100"
            "850A0300004482186300"
            "85010100005823526561647920746F2067656E657261746520612033322D62797465207061796C6F6164"
            "FF"
        );
        assert!(!compare(&limit_30, &limit_99).is_empty());
    }

    #[test]
    fn bib_hmac_difference_caught() {
        // BIB_BUNDLE with the last byte of the first HMAC changed (0x71 → 0x70)
        // — exercises the security-block difference path.
        let tampered = hex!(
            "9F88070000820282010282028202018202820201820018281A000F4240850B030000587582010201008202820301828182015830F75FE4C37F76F046165855BD5FF72FBFD4E3A64B4695C40E2B787DA005AE819F0A2E30A2E8B325527DE8AEFB52E73D7081820158306EE5CA30AB3A1BF1E7F645EB21418FFC129BACFB69677FDAE0D08CB63159358FA86BE682538299B4B7E53C04FE03FDE88507020000410085010100005823526561647920746F2067656E657261746520612033322D62797465207061796C6F6164FF"
        );
        assert!(!compare(BIB_BUNDLE, &tampered).is_empty());
    }

    #[test]
    fn different_bib_target_order_is_equivalent() {
        // BIB targets [1,2] vs [2,1] — exercises the security-block equivalence
        // path (target resolution by type+position, not block number).
        let targets_12 = hex!(
            "9F88070000820282010282028202018202820201820018281A000F4240"
            "850B030000587582010201008202820301828182015830F75FE4C37F76F046165855BD5FF72FBFD4E3A64B4695C40E2B787DA005AE819F0A2E30A2E8B325527DE8AEFB52E73D7181820158306EE5CA30AB3A1BF1E7F645EB21418FFC129BACFB69677FDAE0D08CB63159358FA86BE682538299B4B7E53C04FE03FDE8"
            "8507020000 4100"
            "85010100005823526561647920746F2067656E657261746520612033322D62797465207061796C6F6164"
            "FF"
        );
        let targets_21 = hex!(
            "9F88070000820282010282028202018202820201820018281A000F4240"
            "850B030000587582020101008202820301828182015830 6EE5CA30AB3A1BF1E7F645EB21418FFC129BACFB69677FDAE0D08CB63159358FA86BE682538299B4B7E53C04FE03FDE8 8182015830 F75FE4C37F76F046165855BD5FF72FBFD4E3A64B4695C40E2B787DA005AE819F0A2E30A2E8B325527DE8AEFB52E73D71"
            "8507020000 4100"
            "85010100005823526561647920746F2067656E657261746520612033322D62797465207061796C6F6164"
            "FF"
        );
        assert_ne!(targets_12.as_slice(), targets_21.as_slice());
        assert!(compare(&targets_12, &targets_21).is_empty());
    }
}
