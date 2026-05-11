use std::collections::{BTreeMap, BTreeSet};

use hardy_bpv7::block;
use hardy_bpv7::bpsec;
use hardy_bpv7::bundle;

/// Options for bundle comparison.
#[derive(Default)]
pub struct CompareOptions {
    /// Ignore CRC type differences between blocks.
    /// CRC type is an implementation choice per RFC 9171 Section 4.2.1.
    pub ignore_crc: bool,
}

/// Compare two parsed bundles for semantic equivalence with default options.
///
/// Returns a list of human-readable differences. Empty means equivalent.
pub fn compare_bundles<K>(
    a: &bundle::Bundle,
    data_a: &[u8],
    b: &bundle::Bundle,
    data_b: &[u8],
    keys: &K,
) -> Vec<String>
where
    K: bpsec::key::KeySource + ?Sized,
{
    compare_bundles_with_options(a, data_a, b, data_b, keys, &CompareOptions::default())
}

/// Compare two parsed bundles for semantic equivalence.
///
/// Byte-compares whole blocks where the encoding is deterministic
/// (primary, payload, regular extension blocks). Falls back to semantic
/// comparison for security blocks where the RFC allows non-deterministic
/// encodings (target array ordering per RFC 9172 Section 3.6, random IV
/// per RFC 9173 Section 4.3.1).
///
/// Returns a list of human-readable differences. Empty means equivalent.
pub fn compare_bundles_with_options<K>(
    a: &bundle::Bundle,
    data_a: &[u8],
    b: &bundle::Bundle,
    data_b: &[u8],
    keys: &K,
    options: &CompareOptions,
) -> Vec<String>
where
    K: bpsec::key::KeySource + ?Sized,
{
    let mut diffs = Vec::new();

    // Primary block: whole-block byte compare (unless ignore_crc,
    // since CRC presence changes the primary block encoding)
    if options.ignore_crc {
        if a.id != b.id {
            diffs.push("Primary: id differs".to_string());
        }
        if a.destination != b.destination {
            diffs.push("Primary: destination differs".to_string());
        }
        if a.report_to != b.report_to {
            diffs.push("Primary: report_to differs".to_string());
        }
        if a.lifetime != b.lifetime {
            diffs.push("Primary: lifetime differs".to_string());
        }
        if a.flags != b.flags {
            diffs.push("Primary: flags differ".to_string());
        }
    } else {
        let primary_a = a
            .blocks
            .get(&0)
            .and_then(|blk| data_a.get(blk.extent.clone()));
        let primary_b = b
            .blocks
            .get(&0)
            .and_then(|blk| data_b.get(blk.extent.clone()));
        match (primary_a, primary_b) {
            (Some(pa), Some(pb)) if pa != pb => {
                diffs.push(format!(
                    "Primary: bytes differ ({} vs {} bytes)",
                    pa.len(),
                    pb.len()
                ));
            }
            (None, _) | (_, None) => {
                diffs.push("Primary: block missing".to_string());
            }
            _ => {}
        }
    }

    // Extension blocks: group by type, compare
    let blocks_a = blocks_by_type(a);
    let blocks_b = blocks_by_type(b);

    let types_a: BTreeSet<_> = blocks_a.keys().collect();
    let types_b: BTreeSet<_> = blocks_b.keys().collect();

    for t in types_a.difference(&types_b) {
        diffs.push(format!("{:?}: present in A, missing in B", blocks_a[t].0));
    }
    for t in types_b.difference(&types_a) {
        diffs.push(format!("{:?}: missing in A, present in B", blocks_b[t].0));
    }

    for type_code in types_a.intersection(&types_b) {
        let (bt, ref a_bns) = blocks_a[type_code];
        let (_, ref b_bns) = blocks_b[type_code];

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

            match bt {
                // BIB: semantic (target order unspecified per RFC 9172 Section 3.6)
                block::Type::BlockIntegrity => {
                    let ad = a.block_data(*a_bn, data_a, keys).ok();
                    let bd = b.block_data(*b_bn, data_b, keys).ok();
                    match (ad, bd) {
                        (Some(ad), Some(bd)) => {
                            compare_bib(ad.as_ref(), bd.as_ref(), &tag, &mut diffs);
                        }
                        _ => diffs.push(format!("{tag}: data unavailable")),
                    }
                }

                // BCB: semantic (target order + random IV)
                block::Type::BlockSecurity => {
                    let ad = a.block_data(*a_bn, data_a, keys).ok();
                    let bd = b.block_data(*b_bn, data_b, keys).ok();
                    match (ad, bd) {
                        (Some(ad), Some(bd)) => {
                            compare_bcb(ad.as_ref(), bd.as_ref(), &tag, &mut diffs);
                        }
                        _ => diffs.push(format!("{tag}: data unavailable")),
                    }
                }

                // Everything else: compare flags, CRC type, and data bytes
                _ => {
                    let blk_a = &a.blocks[a_bn];
                    let blk_b = &b.blocks[b_bn];

                    if blk_a.flags != blk_b.flags {
                        diffs.push(format!("{tag}: flags differ"));
                    }
                    if !options.ignore_crc && blk_a.crc_type != blk_b.crc_type {
                        diffs.push(format!(
                            "{tag}: CRC type {:?} vs {:?}",
                            blk_a.crc_type, blk_b.crc_type
                        ));
                    }

                    let ab = blk_a.payload(data_a);
                    let bb = blk_b.payload(data_b);
                    match (ab, bb) {
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
            }
        }
    }

    diffs
}

/// Group block numbers by type code. Returns (block::Type, sorted block numbers).
fn blocks_by_type(bundle: &bundle::Bundle) -> BTreeMap<u64, (block::Type, Vec<u64>)> {
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

/// BIB: compare source, targets (as set), and HMAC results per target.
fn compare_bib(a: &[u8], b: &[u8], tag: &str, diffs: &mut Vec<String>) {
    let ops_a = hardy_cbor::decode::parse::<bpsec::bib::OperationSet>(a);
    let ops_b = hardy_cbor::decode::parse::<bpsec::bib::OperationSet>(b);

    match (ops_a, ops_b) {
        (Ok(oa), Ok(ob)) => {
            if oa.source != ob.source {
                diffs.push(format!("{tag}: source {} vs {}", oa.source, ob.source));
            }

            let targets_a: BTreeSet<_> = oa.operations.keys().collect();
            let targets_b: BTreeSet<_> = ob.operations.keys().collect();
            if targets_a != targets_b {
                diffs.push(format!("{tag}: targets {targets_a:?} vs {targets_b:?}"));
                return;
            }

            for target in &targets_a {
                let ra = bib_hmac_bytes(&oa.operations[target]);
                let rb = bib_hmac_bytes(&ob.operations[target]);
                if let (Some(ra), Some(rb)) = (ra, rb) {
                    if ra != rb {
                        diffs.push(format!("{tag}: HMAC for target {target} differs"));
                    }
                }
            }
        }
        _ => diffs.push(format!("{tag}: failed to parse ASB")),
    }
}

/// BCB: compare source and targets (as set). Skip IV and ciphertext.
fn compare_bcb(a: &[u8], b: &[u8], tag: &str, diffs: &mut Vec<String>) {
    let ops_a = hardy_cbor::decode::parse::<bpsec::bcb::OperationSet>(a);
    let ops_b = hardy_cbor::decode::parse::<bpsec::bcb::OperationSet>(b);

    match (ops_a, ops_b) {
        (Ok(oa), Ok(ob)) => {
            if oa.source != ob.source {
                diffs.push(format!("{tag}: source {} vs {}", oa.source, ob.source));
            }
            let targets_a: BTreeSet<_> = oa.operations.keys().collect();
            let targets_b: BTreeSet<_> = ob.operations.keys().collect();
            if targets_a != targets_b {
                diffs.push(format!("{tag}: targets {targets_a:?} vs {targets_b:?}"));
            }
        }
        _ => diffs.push(format!("{tag}: failed to parse ASB")),
    }
}

/// Extract HMAC bytes from a BIB operation.
fn bib_hmac_bytes(op: &bpsec::bib::Operation) -> Option<&[u8]> {
    match op {
        bpsec::bib::Operation::HMAC_SHA2(o) => Some(&o.results.0),
        _ => None,
    }
}
