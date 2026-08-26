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

use bytes::Bytes;
use hardy_bpv7::{
    Bundle, block, bpsec, bundle_age, checks, editor::Chunk, eid, hop_info, parse, rewrite,
    status_report::ReasonCode,
};
use tracing::debug;

use super::Bpv7Bundle;
use crate::{HashMap, HashSet, cla::Segment, stream::Receiver};

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Reshape the parser-internal `Bundle` + the §D-extracted
/// extension fields into the rich [`Bpv7Bundle`] BPA stores.
pub fn reshape_to_rich(raw: Bundle, extracted: ExtractedExtensionFields) -> Bpv7Bundle {
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
pub fn rich_from_built(raw: Bundle, data: &[u8]) -> Result<Bpv7Bundle, hardy_bpv7::Error> {
    let extracted =
        extract_extension_block_fields(data, &raw.blocks, &HashMap::<u64, &[u8]>::new())?;
    Ok(reshape_to_rich(raw, extracted))
}

/// Map a keyed-validation error to the status-report reason BPA emits with the
/// deletion notice. Used by [`parse_headers`] and the ingress finalize path.
///
/// The RFC 9172 codes selectable here are the ones detectable without security
/// policy: `UnknownSecurityOperation` (an operation this node cannot understand
/// — unknown context id or parameter) and `FailedSecurityOperation` (an
/// operation that failed to verify/decrypt). `Missing`/`Unexpected` need
/// verifier/acceptor role policy that does not exist yet, and `Conflicting`
/// (BPSec protocol violations between operations) is rejected by the
/// structural parser before any reportable bundle exists. Per RFC 9172 §7.1,
/// policy SHOULD gate when security reason codes are sent at all; the global
/// `status_reports` switch is that gate for now.
pub fn status_report_reason_for(error: &hardy_bpv7::Error) -> ReasonCode {
    match error {
        hardy_bpv7::Error::Unsupported(_) => ReasonCode::BlockUnsupported,
        hardy_bpv7::Error::InvalidBPSec(
            bpsec::Error::UnrecognisedContext(_) | bpsec::Error::UnsupportedOperation,
        ) => ReasonCode::UnknownSecurityOperation,
        hardy_bpv7::Error::InvalidBPSec(
            bpsec::Error::DecryptionFailed | bpsec::Error::IntegrityCheckFailed,
        ) => ReasonCode::FailedSecurityOperation,
        _ => ReasonCode::BlockUnintelligible,
    }
}

/// Reception-report reason from the §A `report_on_failure` facts plus the
/// §5.1.1 failure-drop outcome. The RFC 9172 security codes outrank the
/// generic RFC 9171 block code when several fire: a dropped corrupt operation
/// is the most material event, then an operation this node cannot understand,
/// then an unrecognised plain block.
pub fn reception_reason_for(
    classification: &checks::Classification,
    failure_dropped: bool,
) -> ReasonCode {
    if failure_dropped {
        ReasonCode::FailedSecurityOperation
    } else if classification.report_unsupported_security {
        ReasonCode::UnknownSecurityOperation
    } else if classification.report_unsupported_block {
        ReasonCode::BlockUnsupported
    } else {
        ReasonCode::NoAdditionalInformation
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
pub fn parse_validate_with_provider<F>(
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

/// A liveness-critical extension block a forwarding node can't process without
/// its plaintext: `HopCount` (RFC 9171 §4.4.3 — the anti-"ping-pong" loop
/// defense, so it must stay processable) and, on a node with no clock,
/// `BundleAge` (its only expiry signal). Such a block is fatal whether it's
/// undecipherable (no key) or corrupt (failed authentication): either way we
/// can't enforce it, and forwarding without it risks a routing loop or an
/// immortal bundle. Contrast a non-liveness block, where the two failure modes
/// diverge — a corrupt one is stripped (RFC 9172 §5.1.1), an undecipherable one
/// is forwarded intact for a downstream security acceptor.
fn is_liveness_critical(block_type: block::Type, is_clocked: bool) -> bool {
    matches!(block_type, block::Type::HopCount)
        || (!is_clocked && matches!(block_type, block::Type::BundleAge))
}

/// Call-site NoKey policy: reject a bundle carrying a liveness-critical extension
/// block that couldn't be decrypted (no key) — see [`is_liveness_critical`].
/// `nokey` is the second element of [`parse_validate_with_provider`]'s result
/// (equivalently `VerifyFacts::nokey_ext`). A node that accepts/forwards applies
/// this; a restart re-check tolerates a key that has since rotated away and skips
/// it.
pub fn reject_undecryptable_liveness(
    nokey: &[(u64, block::Type)],
    is_clocked: bool,
) -> Result<(), hardy_bpv7::Error> {
    if nokey
        .iter()
        .any(|(_, block_type)| is_liveness_critical(*block_type, is_clocked))
    {
        return Err(bpsec::Error::NoKey.into());
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
pub struct HeaderVerify {
    pub raw: Bundle,
    pub extracted: ExtractedExtensionFields,
    /// Unrecognised / unsupported blocks to drop in the post-drain §E rewrite.
    pub to_remove: HashSet<u64>,
    /// Reception-report reason chosen from the §A `report_on_failure` facts
    /// and the §5.1.1 failure-drop outcome (see [`reception_reason_for`]);
    /// `NoAdditionalInformation` when none fired.
    pub report_reason: ReasonCode,
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
    pub fn gate_reason(&self, received_at: time::OffsetDateTime) -> Option<ReasonCode> {
        let primary = &self.raw.primary;
        let creation = primary.id.timestamp.as_datetime().unwrap_or_else(|| {
            // No clock: creation = ingress time − Bundle Age.
            received_at.saturating_sub(
                self.extracted
                    .age
                    .unwrap_or_default()
                    .try_into()
                    .expect("bundle age in ms is within time::Duration's i64-second range"),
            )
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
/// Why [`parse_headers`] failed. The first two are the CLA's business —
/// the transfer must not be acknowledged ([`Cancelled`](Self::Cancelled)) or
/// must be refused ([`TooLarge`](Self::TooLarge)) — while
/// [`Invalid`](Self::Invalid) is an internal drop: the transfer itself
/// completed, the content is just not a valid bundle.
// The recoverable bundle rides the cold error path by value — the same
// trade recorded by the `result_large_err` allow on `parse_headers`.
#[allow(clippy::large_enum_variant)]
pub(crate) enum HeaderFailure {
    /// The producer went away before the bundle completed.
    Cancelled,
    /// The accumulated stream crossed the caller's size bound.
    TooLarge { size: usize, max: usize },
    /// Structural or keyed-validation failure. When the bundle id was
    /// recoverable the caller emits a reception report with the reason,
    /// then drops.
    Invalid(Option<(Bpv7Bundle, ReasonCode)>),
}

/// `Ok` is the verified headers, the resident header `Bytes` (the whole bundle
/// when it fit, else the `consumed` prefix), and the payload `tail` the caller
/// drains — the drain continues the byte count this pass starts against
/// `max_size`, which here bounds hostile unbounded header chains. `Err` is a
/// [`HeaderFailure`]; see its variants for who handles what.
#[allow(clippy::result_large_err, clippy::type_complexity)]
pub async fn parse_headers<F>(
    stream: &mut dyn Receiver<Segment>,
    max_size: usize,
    key_provider: F,
) -> Result<(HeaderVerify, Bytes, Option<parse::PayloadTail>), HeaderFailure>
where
    F: FnOnce(&Bundle, &[u8]) -> Box<dyn bpsec::key::KeySource>,
{
    let mut parser = parse::BundleParser::default();
    // Drive the parser up to the header chain. `headers` is the resident bytes
    // (the whole bundle, or the `consumed` prefix for an oversized payload);
    // `tail` (if any) drains the rest back in `dispatcher::ingress`.
    let mut total: usize = 0;
    let (parsed, headers, tail) = loop {
        let (bytes, last) = match stream.recv().await {
            Ok(Segment::Next(b)) => (b, false),
            Ok(Segment::Final(b)) => (b, true),
            Err(_) => {
                debug!("Bundle stream cancelled");
                return Err(HeaderFailure::Cancelled);
            }
        };
        total = total.saturating_add(bytes.len());
        if total > max_size {
            return Err(HeaderFailure::TooLarge {
                size: total,
                max: max_size,
            });
        }
        match parser.push(bytes) {
            Ok(parse::ParserProgress::NeedMore(_)) if last => {
                debug!("Truncated bundle");
                return Err(HeaderFailure::Invalid(None));
            }
            Ok(parse::ParserProgress::NeedMore(_)) => {}
            Ok(parse::ParserProgress::Ready(whole)) => match parser.finish(whole.clone()) {
                Ok(parsed) => break (parsed, whole, None),
                Err(_) => return Err(HeaderFailure::Invalid(None)),
            },
            Ok(parse::ParserProgress::Partial { consumed, tail }) => {
                match parser.finish(consumed.clone()) {
                    Ok(parsed) => break (parsed, consumed, Some(tail)),
                    Err(_) => return Err(HeaderFailure::Invalid(None)),
                }
            }
            Err(e) => {
                debug!("Bundle structural parse failed: {e}");
                return Err(HeaderFailure::Invalid(None));
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
        Ok((extracted, to_remove, report_reason)) => Ok((
            HeaderVerify {
                raw,
                extracted,
                to_remove,
                report_reason,
                // After `verify`, the leftover `bib_ops` is exactly the block-1 set.
                deferred_bibs: bib_ops,
            },
            headers,
            tail,
        )),
        Err(error) => {
            debug!("Invalid bundle received: {error}");
            Err(HeaderFailure::Invalid(Some((
                reshape_to_rich(raw, ExtractedExtensionFields::default()),
                status_report_reason_for(&error),
            ))))
        }
    }
}

/// Header verification (§A classify → §B/§C8/§C7 verify → §D extract) against the
/// resident `headers` buffer — the `consumed` prefix for an oversized streamed
/// payload, or the whole bundle otherwise. Mutates `raw.blocks` (BIB coverage
/// stamps) and drains `bib_ops` to the block-1 (payload) leftovers that
/// [`finalize_with_provider`] re-verifies once the payload is resident; the §E
/// removals are deferred there too. Returns the extracted extension fields, the
/// blocks to remove, and the reception-report reason.
fn verify_headers(
    headers: &[u8],
    key_source: &dyn bpsec::key::KeySource,
    raw: &mut Bundle,
    bcb_ops: &HashMap<u64, bpsec::bcb::OperationSet>,
    bib_ops: &mut HashMap<u64, bpsec::bib::OperationSet>,
) -> Result<(ExtractedExtensionFields, HashSet<u64>, ReasonCode), hardy_bpv7::Error> {
    // §A — classify; collect deletables; the report_* facts feed the
    // reception-report reason below.
    let classification = checks::classify_unsupported(&raw.blocks, bcb_ops, bib_ops, &[])?;

    let mut to_remove: HashSet<u64> = HashSet::new();
    to_remove.extend(classification.unrecognised_deletable.iter().copied());
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

    // RFC 9172 §5.1.1 failure-drop. `facts.failed` carries only blocks whose
    // ciphertext failed authentication (corrupt) — undecipherable (NoKey) blocks
    // go to `facts.nokey_ext` and are handled below. A corrupt *payload* (block 1)
    // discards the whole bundle; a corrupt *non-payload* target is discarded and
    // the bundle forwarded (applied in the §E rewrite via `to_remove`). Only the
    // failed target is queued: the editor cascade strips it from its covering
    // BCB's OperationSet and drops the BCB only once it empties. A shared BCB
    // with a surviving co-target must stay — the payload is decrypted only at
    // delivery, so it always survives here, and naming the BCB itself in the
    // request would strand its ciphertext (`StrandsCiphertext`) and panic
    // `apply_rewrites`. §C8 never decrypts the payload and a payload BCB is
    // decrypted at delivery, so the block-1 branch is defensive. A corrupt
    // liveness-critical target can't be stripped-and-forwarded — see
    // `is_liveness_critical` — so it's fatal, exactly as its undecipherable
    // counterpart is below.
    let is_clocked = raw.primary.id.timestamp.is_clocked();
    for &target in &facts.failed {
        if target == 1
            || raw
                .blocks
                .get(&target)
                .is_some_and(|b| is_liveness_critical(b.block_type, is_clocked))
        {
            return Err(bpsec::Error::DecryptionFailed.into());
        }
        to_remove.insert(target);
    }
    // Anything still in `facts.failed` here was queued for failure-drop (the
    // fatal cases returned above) — surface that in the reception report.
    let report_reason = reception_reason_for(&classification, !facts.failed.is_empty());

    // Ingress accepts/forwards, so an undecipherable liveness block is fatal; any
    // other undecipherable block is forwarded intact for a downstream acceptor.
    reject_undecryptable_liveness(&facts.nokey_ext, is_clocked)?;

    // Drain `bib_ops` to exactly the op-sets `verify` left with a deferred
    // block-1 (payload) target — the leftover map IS the deferred set
    // `finalize_with_provider` re-verifies once the payload is resident. A no-op
    // on an all-resident buffer (`deferred_bibs` empty).
    bib_ops.retain(|n, _| facts.deferred_bibs.contains(n));

    // §D — decode the well-known extension fields into the rich view. Decode
    // only: no canonical re-emission is queued — `finalize_with_provider`
    // passes an empty rewrite map (see the §E note there; non-canonical CBOR
    // is rejected at parse). Extension blocks only — never the payload, so
    // header-resident.
    let extracted = extract_extension_block_fields(headers, &raw.blocks, &decrypted)?;

    Ok((extracted, to_remove, report_reason))
}

/// Post-drain finalize: verify the deferred block-1 BIB targets and apply the
/// queued §E block removals — both against the now-resident full bundle `whole`
/// — then reshape into the rich [`Bpv7Bundle`]. The key source is rebuilt here
/// (synchronously, never held across the drain's `await`) from the structural
/// `raw`. On a keyed failure returns the reshaped bundle for a status report.
#[allow(clippy::result_large_err, clippy::type_complexity)]
pub fn finalize_with_provider<F>(
    whole: &[u8],
    mut hv: HeaderVerify,
    key_provider: F,
) -> Result<(Bpv7Bundle, Option<Vec<Chunk>>, ReasonCode), (Bpv7Bundle, hardy_bpv7::Error)>
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
        hv.report_reason,
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
pub struct ExtractedExtensionFields {
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

#[cfg(test)]
mod tests {
    use hex_literal::hex;

    use super::*;

    // RFC 9172 §5.1.1 through the real ingress pipeline, with a *shared*
    // BCB — the RFC 9173 Appendix A.4 wire vector, where one BCB (block 2)
    // covers both the encrypted BIB (block 3) and the payload (block 1).
    // Corrupting the BIB's ciphertext must failure-drop only the BIB: the
    // §E cascade shrinks the shared BCB to the payload and the bundle
    // survives. Queuing the shared BCB itself would strand the payload
    // ciphertext (`StrandsCiphertext`) and panic `apply_rewrites` in the
    // ingress task.
    #[cfg(feature = "rfc9173")]
    #[tokio::test]
    async fn multi_target_bcb_failure_drop_survives_ingress() {
        let mut data = hex!(
            "9f88070000820282010282028202018202820201820018281a000f4240850b0300
             005846438ed6208eb1c1ffb94d952175167df0902902064a2983910c4fb2340790bf
             420a7d1921d5bf7c4721e02ab87a93ab1e0b75cf62e4948727c8b5dae46ed2af0543
             9b88029191850c0201005849820301020182028202018382014c5477656c76653132
             313231328202038204078281820150220ffc45c8a901999ecc60991dd78b29818201
             50d2c51cb2481792dae8b21d848cede99b8501010000582390eab6457593379298a8
             724e16e61f837488e127212b59ac91f8a86287b7d07630a122ff"
        )
        .to_vec();
        // Flip one byte of the BIB's ciphertext: the block-3 body is the
        // 70-byte string right after its `58 46` bytes header.
        let pos = data
            .windows(2)
            .position(|w| w == hex!("5846"))
            .expect("BIB body header present")
            + 2;
        data[pos] ^= 0x01;

        // The Appendix A vector keys, raw (the JWK forms are base64url of
        // these bytes).
        fn keys() -> Box<dyn bpsec::key::KeySource> {
            use bpsec::key::{EncAlgorithm, Key, KeyAlgorithm, KeySet, Operation, Type};
            Box::new(KeySet::new(vec![
                Key {
                    key_type: Type::OctetSequence {
                        key: hex!("1a2b1a2b1a2b1a2b1a2b1a2b1a2b1a2b").into(),
                    },
                    key_algorithm: Some(KeyAlgorithm::HS384),
                    enc_algorithm: None,
                    operations: Some([Operation::Verify].into_iter().collect()),
                    id: Some("ipn:2.1".into()),
                    key_use: None,
                },
                Key {
                    key_type: Type::OctetSequence {
                        key: b"qwertyuiopasdfghqwertyuiopasdfgh".as_slice().into(),
                    },
                    key_algorithm: None,
                    enc_algorithm: Some(EncAlgorithm::A256GCM),
                    operations: Some([Operation::Decrypt].into_iter().collect()),
                    id: Some("ipn:2.1".into()),
                    key_use: None,
                },
            ]))
        }

        let (tx, mut rx) = hardy_async::channel::bounded(1);
        hardy_async::channel::Sender::send(
            &tx,
            crate::cla::Segment::Final(crate::Bytes::from(data)),
        )
        .await
        .expect("channel open");

        let Ok((hv, headers, tail)) = parse_headers(&mut rx, 1 << 20, |_, _| keys()).await else {
            panic!("headers must verify: only the corrupt BIB target fails");
        };
        assert!(tail.is_none(), "the small bundle is fully resident");

        let (bundle, chunks, _reason) = finalize_with_provider(&headers, hv, |_, _| keys())
            .map_err(|(_, e)| e)
            .expect("§5.1.1 failure-drop: bundle survives a corrupt target of a shared BCB");
        assert!(chunks.is_some(), "the bundle must be rewritten");
        assert!(!bundle.blocks.contains_key(&3), "corrupt BIB dropped");
        assert!(
            bundle.blocks.contains_key(&2),
            "shared BCB survives, still covering the payload"
        );
        assert!(bundle.blocks.contains_key(&1), "payload survives");
    }

    #[test]
    fn reception_reason_precedence() {
        let mut c = checks::Classification::default();
        assert_eq!(
            reception_reason_for(&c, false),
            ReasonCode::NoAdditionalInformation
        );
        c.report_unsupported_block = true;
        assert_eq!(
            reception_reason_for(&c, false),
            ReasonCode::BlockUnsupported
        );
        c.report_unsupported_security = true;
        assert_eq!(
            reception_reason_for(&c, false),
            ReasonCode::UnknownSecurityOperation
        );
        assert_eq!(
            reception_reason_for(&c, true),
            ReasonCode::FailedSecurityOperation
        );
    }

    #[test]
    fn security_errors_map_to_rfc9172_reasons() {
        assert_eq!(
            status_report_reason_for(&hardy_bpv7::Error::Unsupported(2)),
            ReasonCode::BlockUnsupported
        );
        assert_eq!(
            status_report_reason_for(&bpsec::Error::UnrecognisedContext(99).into()),
            ReasonCode::UnknownSecurityOperation
        );
        assert_eq!(
            status_report_reason_for(&bpsec::Error::UnsupportedOperation.into()),
            ReasonCode::UnknownSecurityOperation
        );
        assert_eq!(
            status_report_reason_for(&bpsec::Error::DecryptionFailed.into()),
            ReasonCode::FailedSecurityOperation
        );
        assert_eq!(
            status_report_reason_for(&bpsec::Error::IntegrityCheckFailed.into()),
            ReasonCode::FailedSecurityOperation
        );
        assert_eq!(
            status_report_reason_for(&bpsec::Error::NoKey.into()),
            ReasonCode::BlockUnintelligible
        );
    }
}
