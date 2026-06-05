//! BPA-local keyed Bundle parse pipelines. Each composes the per-section
//! [`hardy_bpv7::checks`] helpers (and [`rewrite::apply_rewrites`]) and
//! reshapes the structurally-parsed `Bundle` into the rich
//! [`Bpv7Bundle`] the BPA stores.
//!
//! Two entry points. Neither canonicalises: non-canonical CBOR is rejected at
//! parse (RFC 9171 §4.1), and rewriting it is a configurable mutating-filter
//! concern (see `docs/streaming_pipeline_design.md` §5.2.2), not parser work.
//!
//! * [`parse_validate_with_provider`] — one-shot keyed validation of a complete
//!   buffer, no block removal. It returns the list of liveness-critical
//!   extension blocks that couldn't be decrypted (no key); the caller decides
//!   what to do with it. `dispatcher::restart` ignores it (re-check stored data
//!   on startup, tolerating a since-rotated key), while `dispatcher::local` and
//!   `filter::chain` pass it to [`reject_undecryptable_liveness`] (locally
//!   originated / re-emitted bytes must be fully decryptable).
//! * [`parse_headers`] + [`finalize_with_provider`] — the ingress pipeline,
//!   split so the streaming gate can early-reject before the payload is spooled.
//!   The header pass drops `delete_block_on_failure`-flagged unknowns, cascades
//!   re-encryption of BCB-covered BIBs when their target list shrinks, and drains
//!   BPSec down to the deferred block-1 (payload) targets; the finalize pass
//!   verifies those and applies the block removals once the payload is resident.
//!   Used by `dispatcher::ingress`; on a keyed failure returns the recoverable
//!   bundle so the caller can emit a status report.

use super::Bpv7Bundle;
use crate::cla::Segment;
use crate::stream::Receiver;
use crate::{HashMap, HashSet};
use bytes::Bytes;
use hardy_bpv7::{
    Bundle, block, bpsec, bundle_age, checks, editor::Chunk, eid, hop_info, parse, rewrite,
    status_report::ReasonCode,
};
use tracing::debug;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Reshape the parser-internal `Bundle` + the §D-extracted
/// extension fields into the rich [`Bpv7Bundle`] BPA stores.
pub(crate) fn reshape_to_rich(raw: Bundle, extracted: ExtractedExtensionFields) -> Bpv7Bundle {
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

/// Map a keyed-validation error to the status-report reason BPA emits with the
/// deletion notice. Used by [`parse_headers`] and the ingress finalize path.
pub(crate) fn status_report_reason_for(error: &hardy_bpv7::Error) -> ReasonCode {
    if matches!(error, hardy_bpv7::Error::Unsupported(_)) {
        ReasonCode::BlockUnsupported
    } else {
        ReasonCode::BlockUnintelligible
    }
}

// ---------------------------------------------------------------------------
// Validate — one-shot keyed validation of a complete buffer, no rewriting
// ---------------------------------------------------------------------------

/// One-shot keyed validation of a complete in-memory bundle. Returns the
/// validated rich [`Bpv7Bundle`] **and** `nokey_ext` — the §C8 extension blocks
/// that were BCB-encrypted but undecryptable (no key). It produces those facts;
/// it does **not** adjudicate them — whether an undecryptable block is fatal is a
/// call-site policy (see [`reject_undecryptable_liveness`]). This keeps
/// extension-block policy at the point of use, matching the eventual
/// decode-on-demand model rather than baking it into the parse layer.
///
/// No block removal, no rewriting — non-canonical CBOR is rejected at parse
/// (RFC 9171 §4.1), and re-emitting it is a configurable mutating-filter concern
/// (see `docs/streaming_pipeline_design.md` §5.2.2), not standard-parser work.
#[allow(clippy::result_large_err, clippy::type_complexity)]
pub(crate) fn parse_validate_with_provider<F>(
    data: Bytes,
    key_provider: F,
) -> Result<(Bpv7Bundle, Vec<(u64, block::Type)>), hardy_bpv7::Error>
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

    // §A — no removals scheduled, but `?` still catches an Unsupported
    // `delete_bundle_on_failure` block.
    checks::classify_unsupported(&raw.blocks, &bcb_ops, &bib_ops, &[])?;

    // §B + §C8 + §C7 — composed keyed verification. A §C8 decrypt failure is
    // rejected. (A complete buffer, so `verify` drains the op-maps fully — block
    // 1 is verified inline.)
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

    // §D — extract extension fields into the rich form.
    let extracted = extract_extension_block_fields(&data, &raw.blocks, &decrypted)?;
    Ok((reshape_to_rich(raw, extracted), facts.nokey_ext.into_vec()))
}

/// Call-site NoKey policy: reject a bundle carrying a liveness-critical extension
/// block that couldn't be decrypted (no key) — `HopCount` (RFC 9171 requires
/// processing it) or unclocked `BundleAge` (the only liveness signal). `nokey` is
/// the second element of [`parse_validate_with_provider`]'s result (equivalently
/// `VerifyFacts::nokey_ext`). A node that accepts/forwards applies this; a restart
/// re-check tolerates a key that has since rotated away and skips it.
pub(crate) fn reject_undecryptable_liveness(
    nokey: &[(u64, block::Type)],
    is_clocked: bool,
) -> Result<(), hardy_bpv7::Error> {
    for (_, block_type) in nokey {
        match block_type {
            block::Type::HopCount => return Err(bpsec::Error::NoKey.into()),
            block::Type::BundleAge if !is_clocked => return Err(bpsec::Error::NoKey.into()),
            _ => {}
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Full ingress — split into a pre-drain header pass and a post-drain finalize
// so the streaming gate can early-reject before the payload is spooled.
// ---------------------------------------------------------------------------

/// Result of the pre-drain header pass: everything the streaming gate needs to
/// decide whether to drain, plus the inputs [`finalize_with_provider`] needs to
/// finish once the payload is resident. `raw` is kept **un-reshaped** so a key
/// source can still be built (`key_provider` takes a structural `Bundle`) for
/// the post-drain payload verify and rewrite.
pub(crate) struct HeaderVerify {
    pub raw: Bundle,
    pub extracted: ExtractedExtensionFields,
    /// Unrecognised / unsupported blocks to drop in the post-drain §E rewrite.
    pub to_remove: HashSet<u64>,
    pub report_unsupported: bool,
    /// BIB op-sets `checks::verify` left targeting the not-yet-resident payload
    /// (block 1) — re-verified against the full bundle by
    /// [`finalize_with_provider`]. Empty when the payload was resident. A block-1
    /// *BCB* (payload confidentiality) needs no deferral — it's decrypted at
    /// delivery via [`hardy_bpv7::bpsec::block_data`].
    pub deferred_bibs: HashMap<u64, bpsec::bib::OperationSet>,
}

impl HeaderVerify {
    /// Header-only early-reject reason, if any: the bundle is past its lifetime,
    /// or a Hop Count block has reached its limit. Computed straight off the
    /// parsed primary + extracted extension fields, so the streaming gate can
    /// run it before the payload is drained (no reshape into the rich form).
    pub(crate) fn gate_reason(&self, received_at: time::OffsetDateTime) -> Option<ReasonCode> {
        let primary = &self.raw.primary;
        let creation = primary.id.timestamp.as_datetime().unwrap_or_else(|| {
            // No clock: creation = ingress time − Bundle Age. The unwrap is safe;
            // bundle age is at most u64::MAX milliseconds.
            received_at.saturating_sub(self.extracted.age.unwrap_or_default().try_into().unwrap())
        });
        let expiry =
            creation.saturating_add(primary.lifetime.try_into().unwrap_or(time::Duration::MAX));
        if expiry <= time::OffsetDateTime::now_utc() {
            Some(ReasonCode::LifetimeExpired)
        } else if self
            .extracted
            .hop_count
            .as_ref()
            .is_some_and(|h| h.count > h.limit)
        {
            Some(ReasonCode::HopLimitExceeded)
        } else {
            None
        }
    }
}

/// Drive the structural parser off the segment stream up to the parsed header
/// chain (*without* draining an oversized payload), then run the keyed header
/// verification against the resident bytes — the streaming gate's whole pre-drain
/// stage in one call. `checks::verify` drains the payload-block BPSec into
/// [`HeaderVerify::deferred_bibs`] for the post-drain [`finalize_with_provider`].
///
/// `Ok` is the verified headers, the resident header `Bytes` (the whole bundle
/// when it fit, else the `consumed` prefix), and the payload `tail` the caller
/// drains. `Err(None)` is a structural / truncation / cancellation drop with no
/// recoverable bundle. `Err(Some((bundle, reason)))` is a keyed-validation
/// failure whose bundle id *is* recoverable — reshaped here so the caller only
/// has to emit a reception report with `reason`, then drop.
#[allow(clippy::result_large_err, clippy::type_complexity)]
pub(crate) async fn parse_headers<F>(
    stream: &dyn Receiver<Segment>,
    key_provider: F,
) -> Result<(HeaderVerify, Bytes, Option<parse::PayloadTail>), Option<(Bpv7Bundle, ReasonCode)>>
where
    F: FnOnce(&Bundle, &[u8]) -> Box<dyn bpsec::key::KeySource>,
{
    let mut parser = parse::BundleParser::default();
    // Drive the parser up to the header chain. `headers` is the resident bytes
    // (the whole bundle, or the `consumed` prefix for an oversized payload);
    // `tail` (if any) drains the rest back in `dispatcher::ingress`.
    let (parsed, headers, tail) = loop {
        let (bytes, last) = match stream.recv().await {
            Ok(Segment::Next(b)) => (b, false),
            Ok(Segment::Final(b)) => (b, true),
            Err(_) => {
                debug!("Bundle stream cancelled");
                return Err(None);
            }
        };
        match parser.push(bytes) {
            Ok(parse::ParserProgress::NeedMore(_)) if last => {
                debug!("Truncated bundle");
                return Err(None);
            }
            Ok(parse::ParserProgress::NeedMore(_)) => {}
            Ok(parse::ParserProgress::Ready(whole)) => match parser.finish(whole.clone()) {
                Ok(parsed) => break (parsed, whole, None),
                Err(_) => return Err(None),
            },
            Ok(parse::ParserProgress::Partial { consumed, tail }) => {
                match parser.finish(consumed.clone()) {
                    Ok(parsed) => break (parsed, consumed, Some(tail)),
                    Err(_) => return Err(None),
                }
            }
            Err(e) => {
                debug!("Bundle structural parse failed: {e}");
                return Err(None);
            }
        }
    };

    // Header verification (§A–§D) against the resident bytes. On a keyed failure
    // the recoverable `raw` is reshaped so the caller need only emit a reception
    // report; on success it moves into the returned `HeaderVerify`.
    let parse::Parsed {
        bundle: mut raw,
        bcbs: bcb_ops,
        bibs: mut bib_ops,
        ..
    } = parsed;
    let key_source = key_provider(&raw, &headers);
    match verify_headers(&headers, &*key_source, &mut raw, &bcb_ops, &mut bib_ops) {
        Ok((extracted, to_remove, report_unsupported)) => Ok((
            HeaderVerify {
                raw,
                extracted,
                to_remove,
                report_unsupported,
                // After `verify`, the leftover `bib_ops` is exactly the block-1 set.
                deferred_bibs: bib_ops,
            },
            headers,
            tail,
        )),
        Err(error) => {
            debug!("Invalid bundle received: {error}");
            Err(Some((
                reshape_to_rich(raw, ExtractedExtensionFields::default()),
                status_report_reason_for(&error),
            )))
        }
    }
}

/// Header verification (§A classify → §B/§C8/§C7 verify → §D extract) against the
/// resident `headers` buffer — the `consumed` prefix for an oversized streamed
/// payload, or the whole bundle otherwise. Mutates `raw.blocks` (BIB coverage
/// stamps) and drains `bib_ops` to the block-1 (payload) leftovers that
/// [`finalize_with_provider`] re-verifies once the payload is resident; the §E
/// removals are deferred there too. Returns the extracted extension fields, the
/// blocks to remove, and `report_unsupported`.
fn verify_headers(
    headers: &[u8],
    key_source: &dyn bpsec::key::KeySource,
    raw: &mut Bundle,
    bcb_ops: &HashMap<u64, bpsec::bcb::OperationSet>,
    bib_ops: &mut HashMap<u64, bpsec::bib::OperationSet>,
) -> Result<(ExtractedExtensionFields, HashSet<u64>, bool), hardy_bpv7::Error> {
    // §A — classify; collect deletables; observe report_unsupported.
    let classification = checks::classify_unsupported(&raw.blocks, bcb_ops, bib_ops, &[])?;
    let report_unsupported = classification.report_unsupported;

    let mut to_remove: HashSet<u64> = HashSet::new();
    to_remove.extend(classification.unrecognised_deletable);
    for n in &classification.bib_deletable {
        to_remove.insert(*n);
        bib_ops.remove(n);
    }

    // §B + §C8 + §C7 — composed keyed verification. NoKey on §C8 is fatal for
    // HopCount and unclocked BundleAge; a §C8/§B decrypt failure is rejected.
    // `verify` drains `bib_ops`/`bcb_ops` to the block-1 (payload) leftovers.
    let mut decrypted = HashMap::new();
    let to_update_seed: HashMap<u64, Vec<u8>> = HashMap::new();
    let facts = checks::verify(
        headers,
        key_source,
        &mut raw.blocks,
        bcb_ops,
        bib_ops,
        &mut decrypted,
        &to_update_seed,
    )?;
    if !facts.failed.is_empty() {
        return Err(bpsec::Error::DecryptionFailed.into());
    }
    // Ingress accepts/forwards, so an undecryptable liveness block is fatal.
    reject_undecryptable_liveness(&facts.nokey_ext, raw.primary.id.timestamp.is_clocked())?;

    // Drain `bib_ops` to exactly the op-sets `verify` left with a deferred
    // block-1 (payload) target — the leftover map IS the deferred set
    // `finalize_with_provider` re-verifies once the payload is resident. A no-op
    // on an all-resident buffer (`deferred_bibs` empty).
    bib_ops.retain(|n, _| facts.deferred_bibs.contains(n));

    // §D — extract extension fields. Any canonical re-emits ride along in
    // `extracted.canonical_rewrites`; `finalize_with_provider` applies them after
    // the drain. Extension blocks only — never the payload, so header-resident.
    let extracted = extract_extension_block_fields(headers, &raw.blocks, &decrypted)?;

    Ok((extracted, to_remove, report_unsupported))
}

/// Post-drain finalize: verify the deferred block-1 BIB targets and apply the
/// queued §E block removals — both against the now-resident full bundle `whole`
/// — then reshape into the rich [`Bpv7Bundle`]. The key source is rebuilt here
/// (synchronously, never held across the drain's `await`) from the structural
/// `raw`. On a keyed failure returns the reshaped bundle for a status report.
#[allow(clippy::result_large_err, clippy::type_complexity)]
pub(crate) fn finalize_with_provider<F>(
    whole: &[u8],
    mut hv: HeaderVerify,
    key_provider: F,
) -> Result<(Bpv7Bundle, Option<Vec<Chunk>>, bool), (Bpv7Bundle, hardy_bpv7::Error)>
where
    F: FnOnce(&Bundle, &[u8]) -> Box<dyn bpsec::key::KeySource>,
{
    let key_source = key_provider(&hv.raw, whole);

    // Deferred payload pass: verify exactly the block-1 BIB targets (header
    // targets were already checked in the header pass — no repeated crypto).
    if !hv.deferred_bibs.is_empty() {
        let no_decrypted = HashMap::new();
        let no_updates = HashMap::new();
        if let Err(e) = checks::verify_payload(
            whole,
            &*key_source,
            &hv.raw.blocks,
            &hv.deferred_bibs,
            &no_decrypted,
            &no_updates,
        ) {
            return Err((
                reshape_to_rich(hv.raw, ExtractedExtensionFields::default()),
                e,
            ));
        }
    }

    // §E — apply block removals (`delete_block_on_failure` unknowns + dropped
    // BIBs) if any. Needs the whole bundle: the Editor copies/references every
    // block, including the payload. Canonical re-emits are not done here (see the
    // module docs) — only removals, hence the empty rewrite map.
    let chunks = if hv.to_remove.is_empty() {
        None
    } else {
        match rewrite::apply_rewrites(whole, &hv.raw, &*key_source, HashMap::new(), hv.to_remove) {
            Ok(rewritten) => rewritten.map(|(new_raw, chunks)| {
                hv.raw = new_raw;
                chunks
            }),
            Err(e) => {
                return Err((
                    reshape_to_rich(hv.raw, ExtractedExtensionFields::default()),
                    e,
                ));
            }
        }
    };

    Ok((
        reshape_to_rich(hv.raw, hv.extracted),
        chunks,
        hv.report_unsupported,
    ))
}

// ---------------------------------------------------------------------------
// §D — extension-block field extraction
//
// Decodes the well-known PreviousNode / BundleAge / HopCount extension blocks
// into typed values for the rich [`Bpv7Bundle`]. BPA policy — bpv7 keeps only
// the structural parse + per-section BPSec primitives.
// ---------------------------------------------------------------------------

/// Output of [`extract_extension_block_fields`].
#[derive(Debug, Default)]
pub(crate) struct ExtractedExtensionFields {
    pub previous_node: Option<eid::Eid>,
    pub age: Option<core::time::Duration>,
    pub hop_count: Option<hop_info::HopInfo>,
}

/// Decode one `PreviousNode` / `BundleAge` / `HopCount` field: the BCB-decrypted
/// plaintext when §C8 supplied it (smuggling-checked via
/// [`hardy_cbor::decode::parse_exact`]), else the block's wire payload via
/// [`block::Block::extract`] (`None` for an encrypted block with no plaintext, or
/// a not-resident payload). Selecting wire-vs-decrypted is BPA policy; the decode
/// + smuggling check are bpv7's.
fn decode_field<T>(
    block: &block::Block,
    source: &[u8],
    decrypted: Option<&[u8]>,
) -> Result<Option<T>, hardy_bpv7::Error>
where
    T: hardy_cbor::decode::FromCbor,
    T::Error: From<hardy_cbor::decode::Error>,
    hardy_bpv7::Error: From<T::Error>,
{
    match decrypted {
        Some(plaintext) => Ok(Some(hardy_cbor::decode::parse_exact(plaintext)?)),
        // BCB-encrypted with no plaintext from §C8 — the wire payload is
        // ciphertext, so there's nothing to decode in place.
        None if block.bcb.is_some() => Ok(None),
        None => block.extract(source),
    }
}

/// Decode `PreviousNode` / `BundleAge` / `HopCount` block bodies into an
/// [`ExtractedExtensionFields`]. Non-canonical encodings are rejected at decode
/// (RFC 9171 §4.1), not re-emitted — canonicalisation is a configurable mutating
/// filter. Generic over the decrypted-plaintext container so the BPSec
/// `Zeroizing` type never needs naming here.
fn extract_extension_block_fields<V: AsRef<[u8]>>(
    data: &[u8],
    blocks: &HashMap<u64, block::Block>,
    decrypted_data: &HashMap<u64, V>,
) -> Result<ExtractedExtensionFields, hardy_bpv7::Error> {
    let mut out = ExtractedExtensionFields::default();

    // Iterate `blocks` directly — no per-bundle `candidates` Vec to allocate
    // (this runs for every bundle, and a Previous Node block is near-universal).
    for (&block_number, target_block) in blocks {
        let decrypted = decrypted_data.get(&block_number).map(AsRef::as_ref);
        match target_block.block_type {
            block::Type::PreviousNode => {
                out.previous_node = decode_field(target_block, data, decrypted)?;
            }
            block::Type::BundleAge => {
                out.age = decode_field::<bundle_age::BundleAge>(target_block, data, decrypted)?
                    .map(Into::into);
            }
            block::Type::HopCount => {
                out.hop_count = decode_field(target_block, data, decrypted)?;
            }
            _ => {}
        }
    }
    Ok(out)
}
