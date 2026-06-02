//! BPA-local keyed Bundle parse pipelines. Each composes the per-section
//! [`hardy_bpv7::checks`] helpers (and [`rewrite::apply_rewrites`]) and
//! reshapes the structurally-parsed `Bundle` into the rich
//! [`Bpv7Bundle`] the BPA stores.
//!
//! Three entry points, differing in how much they rewrite:
//!
//! * [`parse_preserve_with_provider`] — keyed validation, **no
//!   rewriting**. Used by `dispatcher::restart` to re-check stored data
//!   on startup.
//! * [`parse_canonicalize_with_provider`] — keyed validation + canonical
//!   re-emit of non-shortest extension fields; does **not** remove
//!   unrecognised/unsupported blocks. Used by
//!   `dispatcher::local::local_dispatch_raw` (Service hand-in) and the
//!   write-filter loop in `filter::chain`.
//! * [`parse_full_with_provider`] — the ingress pipeline: drops
//!   `delete_block_on_failure`-flagged unknowns, cascades re-encryption
//!   of BCB-covered BIBs when their target list shrinks, and queues
//!   canonical re-emits. Used by `dispatcher::ingress`; on a partial
//!   parse it returns the recoverable bundle so the caller can emit a
//!   status report.
//!
//! The §C8 NoKey policy is the only behavioural difference between them.

use crate::bundle::Bpv7Bundle;
use crate::{HashMap, HashSet};
use bytes::Bytes;
use hardy_bpv7::{
    Bundle, block, bpsec, bundle_age, checks, editor::Chunk, eid, hop_info, parse, rewrite,
    status_report::ReasonCode,
};

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Reshape the parser-internal `Bundle` + the §D-extracted
/// extension fields into the rich [`Bpv7Bundle`] BPA stores.
fn reshape_to_rich(raw: Bundle, extracted: ExtractedExtensionFields) -> Bpv7Bundle {
    Bpv7Bundle {
        id: raw.primary.id,
        flags: raw.primary.flags,
        crc_type: raw.primary.crc_type,
        destination: raw.primary.destination,
        report_to: raw.primary.report_to,
        lifetime: raw.primary.lifetime,
        blocks: raw.blocks,
        previous_node: extracted.previous_node,
        age: extracted.age,
        hop_count: extracted.hop_count,
    }
}

/// Reshape freshly-built `Builder` output — a `Bundle` plus
/// its wire bytes — into the rich [`Bpv7Bundle`] BPA stores. Runs §D
/// extension-field extraction so any PreviousNode / BundleAge / HopCount
/// the builder emitted is reflected in the rich view. Used by the
/// locally-originated paths (`dispatcher::local`, `dispatcher::report`)
/// that build a bundle and immediately wrap it; the keyed parse
/// pipelines above would do this same reshape after redundant BPSec
/// validation a freshly-built bundle doesn't need.
pub(crate) fn rich_from_built(raw: Bundle, data: &[u8]) -> Result<Bpv7Bundle, hardy_bpv7::Error> {
    let extracted =
        extract_extension_block_fields(data, &raw.blocks, &HashMap::<u64, &[u8]>::new())?;
    Ok(reshape_to_rich(raw, extracted))
}

/// Map an Invalid-arm error to the status-report reason BPA should use
/// when emitting the deletion notice. Used by [`parse_full_with_provider`].
pub(crate) fn status_report_reason_for(error: &hardy_bpv7::Error) -> ReasonCode {
    if matches!(error, hardy_bpv7::Error::Unsupported(_)) {
        ReasonCode::BlockUnsupported
    } else {
        ReasonCode::BlockUnintelligible
    }
}

// ---------------------------------------------------------------------------
// Preserve — validate, no rewriting
// ---------------------------------------------------------------------------

/// Returns `(bundle, non_canonical, report_unsupported)` on success.
/// NoKey is soft in §B / §C7 / §C8.
#[allow(clippy::result_large_err)]
pub(crate) fn parse_preserve_with_provider<F>(
    data: Bytes,
    key_provider: F,
) -> Result<(Bpv7Bundle, bool, bool), hardy_bpv7::Error>
where
    F: FnOnce(&Bundle, &[u8]) -> Box<dyn bpsec::key::KeySource>,
{
    let parse::Parsed {
        data,
        bundle: mut raw,
        bcbs: bcb_ops,
        bibs: mut bib_ops,
    } = parse::parse(data)?;
    let key_source = key_provider(&raw, &data);

    // §A — observe report_unsupported; Preserve never schedules removals.
    let a1 = checks::classify_unrecognised_blocks(&raw.blocks, &[])?;
    let a2 = checks::classify_unsupported_bcbs(&raw.blocks, &bcb_ops)?;
    let a3 = checks::classify_unsupported_bibs(&raw.blocks, &bib_ops)?;
    let report_unsupported =
        a1.report_unsupported || a2.report_unsupported || a3.report_unsupported;

    // §B + §C8 + §C7 — composed keyed verification. NoKey is soft for
    // every §C8 block type (the BPA may lack keys for an inbound bundle it
    // didn't originate); a §C8 decrypt failure is rejected.
    let mut decrypted = HashMap::new();
    let no_updates = HashMap::new();
    let facts = checks::verify(
        &data,
        &*key_source,
        &mut raw.blocks,
        &bcb_ops,
        &mut bib_ops,
        &mut decrypted,
        &no_updates,
    )?;
    if !facts.failed.is_empty() {
        return Err(bpsec::Error::DecryptionFailed.into());
    }

    // §D — extract extension fields + observe non_canonical.
    let extracted = extract_extension_block_fields(&data, &raw.blocks, &decrypted)?;
    let non_canonical = extracted.non_canonical;

    Ok((
        reshape_to_rich(raw, extracted),
        non_canonical,
        report_unsupported,
    ))
}

// ---------------------------------------------------------------------------
// Canonicalize — validate + canonical re-emit (no block removal)
// ---------------------------------------------------------------------------

/// Returns `(bundle, Option<chunks>)` on success. `Some(chunks)` means
/// the bundle had non-canonical extension-field encodings and the
/// caller should flatten the chunks to get the canonical wire form.
/// `None` means the input was already canonical. NoKey is strict for
/// `HopCount` and unclocked `BundleAge` (Service hand-in is expected
/// to have all required keys); soft for `PreviousNode` and clocked
/// `BundleAge`. No block removal — unsupported blocks survive.
#[allow(clippy::result_large_err, clippy::type_complexity)]
pub(crate) fn parse_canonicalize_with_provider<F>(
    data: Bytes,
    key_provider: F,
) -> Result<(Bpv7Bundle, Option<Vec<Chunk>>), hardy_bpv7::Error>
where
    F: FnOnce(&Bundle, &[u8]) -> Box<dyn bpsec::key::KeySource>,
{
    let parse::Parsed {
        data,
        bundle: mut raw,
        bcbs: bcb_ops,
        bibs: mut bib_ops,
    } = parse::parse(data)?;
    let key_source = key_provider(&raw, &data);

    // §A — observe report_unsupported (Canonicalize doesn't act on it,
    // but we still want to catch `Unsupported(n)` for blocks flagged
    // `delete_bundle_on_failure`, which the classify helpers propagate
    // as `Err`).
    let _ = checks::classify_unrecognised_blocks(&raw.blocks, &[])?;
    let _ = checks::classify_unsupported_bcbs(&raw.blocks, &bcb_ops)?;
    let _ = checks::classify_unsupported_bibs(&raw.blocks, &bib_ops)?;

    // §B + §C8 + §C7 — composed keyed verification. NoKey on §C8 is fatal
    // for HopCount (RFC 9171 requires processing) and unclocked BundleAge
    // (only liveness signal); soft for clocked BundleAge and PreviousNode
    // (CLA provides previous node). A §C8 decrypt failure is rejected.
    let mut decrypted = HashMap::new();
    let mut to_update: HashMap<u64, Vec<u8>> = HashMap::new();
    let facts = checks::verify(
        &data,
        &*key_source,
        &mut raw.blocks,
        &bcb_ops,
        &mut bib_ops,
        &mut decrypted,
        &to_update,
    )?;
    if !facts.failed.is_empty() {
        return Err(bpsec::Error::DecryptionFailed.into());
    }
    let is_clocked = raw.primary.id.timestamp.is_clocked();
    for (_, block_type) in &facts.nokey_ext {
        match block_type {
            block::Type::HopCount => return Err(bpsec::Error::NoKey.into()),
            block::Type::BundleAge if !is_clocked => return Err(bpsec::Error::NoKey.into()),
            _ => {}
        }
    }

    // §D — queue canonical re-emits.
    let extracted = extract_extension_block_fields(&data, &raw.blocks, &decrypted)?;
    for (n, payload) in &extracted.canonical_rewrites {
        to_update.insert(*n, payload.clone());
    }

    // §E — apply the rewrites if any were queued; no removals.
    let chunks = if to_update.is_empty() {
        None
    } else {
        rewrite::apply_rewrites(&data, &raw, &*key_source, to_update, HashSet::new())?.map(
            |(new_raw, chunks)| {
                raw = new_raw;
                chunks
            },
        )
    };

    Ok((reshape_to_rich(raw, extracted), chunks))
}

// ---------------------------------------------------------------------------
// Full — validate + block removal + canonical re-emit
// ---------------------------------------------------------------------------

/// Returns `Ok((bundle, Option<chunks>, non_canonical, report_unsupported))`
/// on success. On failure returns either `Err((Some(bundle), error))` —
/// partial parse, bundle ID is recoverable so the caller may emit a status
/// report (see [`status_report_reason_for`]) — or `Err((None, error))`
/// for a hard parse failure (no bundle, no status report possible).
#[allow(clippy::type_complexity, clippy::result_large_err)]
pub(crate) fn parse_full_with_provider<F>(
    data: Bytes,
    key_provider: F,
) -> Result<(Bpv7Bundle, Option<Vec<Chunk>>, bool, bool), (Option<Bpv7Bundle>, hardy_bpv7::Error)>
where
    F: FnOnce(&Bundle, &[u8]) -> Box<dyn bpsec::key::KeySource>,
{
    let parse::Parsed {
        data,
        bundle: mut raw,
        bcbs: bcb_ops,
        bibs: bib_ops,
    } = parse::parse(data).map_err(|e| (None, e))?;
    let key_source = key_provider(&raw, &data);

    // From here on, any error has a recoverable bundle to attach.
    let result = parse_full_inner(&data, &*key_source, &mut raw, bcb_ops, bib_ops);

    match result {
        Ok((chunks, extracted, report_unsupported)) => {
            let non_canonical = chunks.is_some() || extracted.non_canonical;
            Ok((
                reshape_to_rich(raw, extracted),
                chunks,
                non_canonical,
                report_unsupported,
            ))
        }
        Err(e) => Err((
            Some(reshape_to_rich(raw, ExtractedExtensionFields::default())),
            e,
        )),
    }
}

/// The bulk of [`parse_full_with_provider`]'s logic, isolated so the
/// caller can wrap errors with the partial-bundle reshape uniformly.
#[allow(clippy::type_complexity)]
fn parse_full_inner(
    data: &[u8],
    key_source: &dyn bpsec::key::KeySource,
    raw: &mut Bundle,
    bcb_ops: HashMap<u64, bpsec::bcb::OperationSet>,
    mut bib_ops: HashMap<u64, bpsec::bib::OperationSet>,
) -> Result<(Option<Vec<Chunk>>, ExtractedExtensionFields, bool), hardy_bpv7::Error> {
    // §A — classify; collect deletables; observe report_unsupported.
    let a1 = checks::classify_unrecognised_blocks(&raw.blocks, &[])?;
    let a2 = checks::classify_unsupported_bcbs(&raw.blocks, &bcb_ops)?;
    let a3 = checks::classify_unsupported_bibs(&raw.blocks, &bib_ops)?;
    let report_unsupported =
        a1.report_unsupported || a2.report_unsupported || a3.report_unsupported;

    let mut to_remove: HashSet<u64> = HashSet::new();
    to_remove.extend(a1.deletable);
    for n in &a3.deletable {
        to_remove.insert(*n);
        bib_ops.remove(n);
    }

    // §B + §C8 + §C7 — composed keyed verification. NoKey on §C8 as in
    // [`parse_canonicalize_with_provider`]. A §C8 decrypt failure is
    // rejected.
    let mut decrypted = HashMap::new();
    let mut to_update: HashMap<u64, Vec<u8>> = HashMap::new();
    let facts = checks::verify(
        data,
        key_source,
        &mut raw.blocks,
        &bcb_ops,
        &mut bib_ops,
        &mut decrypted,
        &to_update,
    )?;
    if !facts.failed.is_empty() {
        return Err(bpsec::Error::DecryptionFailed.into());
    }
    let is_clocked = raw.primary.id.timestamp.is_clocked();
    for (_, block_type) in &facts.nokey_ext {
        match block_type {
            block::Type::HopCount => return Err(bpsec::Error::NoKey.into()),
            block::Type::BundleAge if !is_clocked => return Err(bpsec::Error::NoKey.into()),
            _ => {}
        }
    }

    // §D — queue canonical re-emits.
    let extracted = extract_extension_block_fields(data, &raw.blocks, &decrypted)?;
    for (n, payload) in &extracted.canonical_rewrites {
        to_update.insert(*n, payload.clone());
    }

    // §E — apply rewrites if there's anything to do.
    let chunks = if to_update.is_empty() && to_remove.is_empty() {
        None
    } else {
        rewrite::apply_rewrites(data, raw, key_source, to_update, to_remove)?.map(
            |(new_raw, chunks)| {
                *raw = new_raw;
                chunks
            },
        )
    };

    Ok((chunks, extracted, report_unsupported))
}

// ---------------------------------------------------------------------------
// §D — extension-block field extraction
//
// Decodes the well-known PreviousNode / BundleAge / HopCount extension
// blocks into typed values for the rich [`Bpv7Bundle`], and observes
// non-canonical encodings (+ canonical re-emit candidates for the
// rewrite pipelines). This is BPA policy — bpv7 keeps only the
// structural parse + per-section BPSec primitives.
// ---------------------------------------------------------------------------

/// `(block_number, canonical_payload)` pairs for non-encrypted extension
/// blocks whose canonical re-emit differs from the wire bytes.
pub(crate) type CanonicalRewrites = Vec<(u64, Vec<u8>)>;

/// Output of [`extract_extension_block_fields`].
#[derive(Debug, Default)]
pub(crate) struct ExtractedExtensionFields {
    pub previous_node: Option<eid::Eid>,
    pub age: Option<core::time::Duration>,
    pub hop_count: Option<hop_info::HopInfo>,
    /// `true` iff any extracted field's encoding was non-canonical.
    pub non_canonical: bool,
    /// Non-encrypted blocks whose canonical re-emit differs from the wire
    /// bytes. Encrypted blocks only set `non_canonical`.
    pub canonical_rewrites: CanonicalRewrites,
}

/// CBOR-decode `T` from `data`, requiring it to consume the whole slice.
fn parse_exact<T>(data: &[u8], field: &'static str) -> Result<T, hardy_bpv7::Error>
where
    T: hardy_cbor::decode::FromCbor,
    T::Error: From<hardy_cbor::decode::Error> + Into<Box<dyn core::error::Error + Send + Sync>>,
{
    match hardy_cbor::decode::parse::<(T, usize)>(data) {
        Err(e) => Err(hardy_bpv7::Error::InvalidField {
            field,
            source: e.into(),
        }),
        Ok((_, len)) if len != data.len() => Err(hardy_bpv7::Error::InvalidField {
            field,
            source: hardy_bpv7::Error::AdditionalData.into(),
        }),
        Ok((t, _)) => Ok(t),
    }
}

/// Decode `PreviousNode` / `BundleAge` / `HopCount` block bodies into an
/// [`ExtractedExtensionFields`]. Payload bytes come from `decrypted_data`
/// (BCB-decrypted by §C8) when present, else the wire bytes in `data`. A
/// BCB-encrypted block absent from `decrypted_data` (NoKey) is skipped.
/// Generic over the decrypted-plaintext container so the BPSec
/// `Zeroizing` type never needs naming here.
pub(crate) fn extract_extension_block_fields<V: AsRef<[u8]>>(
    data: &[u8],
    blocks: &HashMap<u64, block::Block>,
    decrypted_data: &HashMap<u64, V>,
) -> Result<ExtractedExtensionFields, hardy_bpv7::Error> {
    let mut out = ExtractedExtensionFields::default();

    let candidates: Vec<(u64, block::Type)> = blocks
        .iter()
        .filter_map(|(&n, b)| {
            matches!(
                b.block_type,
                block::Type::PreviousNode | block::Type::BundleAge | block::Type::HopCount
            )
            .then_some((n, b.block_type))
        })
        .collect();

    for (block_number, block_type) in candidates {
        let target_block = blocks.get(&block_number).expect("filtered above");
        let is_encrypted = target_block.bcb.is_some();
        let payload: Option<&[u8]> = if let Some(plaintext) = decrypted_data.get(&block_number) {
            Some(plaintext.as_ref())
        } else if is_encrypted {
            None
        } else {
            // `data` is the complete in-memory bundle from `parse::parse`.
            target_block.payload(data)
        };
        let Some(payload) = payload else { continue };

        match block_type {
            block::Type::PreviousNode => {
                let (v, shortest) =
                    parse_exact::<(eid::Eid, bool)>(payload, "Previous Node Block")?;
                if !shortest {
                    out.non_canonical = true;
                    if !is_encrypted {
                        out.canonical_rewrites
                            .push((block_number, hardy_cbor::encode::emit(&v).0));
                    }
                }
                out.previous_node = Some(v);
            }
            block::Type::BundleAge => {
                // `BundleAge::from_cbor` is strict-canonical end-to-end
                // (bare uint has no indefinite-length form), so only
                // trailing garbage (handled by `parse_exact`) remains.
                let v = parse_exact::<bundle_age::BundleAge>(payload, "Bundle Age Block")?;
                out.age = Some(v.into());
            }
            block::Type::HopCount => {
                let (v, shortest) =
                    parse_exact::<(hop_info::HopInfo, bool)>(payload, "Hop Count Block")?;
                if !shortest {
                    out.non_canonical = true;
                    if !is_encrypted {
                        out.canonical_rewrites
                            .push((block_number, hardy_cbor::encode::emit(&v).0));
                    }
                }
                out.hop_count = Some(v);
            }
            _ => unreachable!("filtered above"),
        }
    }
    Ok(out)
}
