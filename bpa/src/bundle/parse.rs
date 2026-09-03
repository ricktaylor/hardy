//! BPA-local keyed Bundle parse pipelines. Each composes the per-section
//! [`hardy_bpv7::checks`] helpers and returns the structurally-parsed
//! `Bundle` together with the §D-decoded extension fields the BPA records in
//! metadata.
//!
//! Two entry points. Neither canonicalises: non-canonical CBOR is rejected at
//! parse (RFC 9171 §4.1), and rewriting it is a configurable mutating-filter
//! concern (see `docs/streaming_pipeline_design.md` §5.2.2), not parser work.
//!
//! * [`parse_validate_with_provider`] — one-shot keyed validation of a complete
//!   buffer, no block removal. It returns the list of BCB-protected well-known
//!   extension blocks that couldn't be decrypted (no key); the caller decides
//!   what to do with it. `dispatcher::restart` ignores it (re-check stored data
//!   on startup, tolerating a since-rotated key), while `dispatcher::local` and
//!   `filter::chain` pass it to [`reject_undecryptable_liveness`], which applies
//!   the liveness policy (locally originated / re-emitted bytes must be fully
//!   decryptable).
//! * [`parse_headers`] — the streaming ingress header pass, which the gate can
//!   early-reject on before the payload is spooled. It classifies and
//!   *schedules* the removals — the `delete_block_on_failure`-flagged unknowns
//!   and the §5.1.1 failure-drops ([`HeaderVerify::to_remove`]) — and, in the
//!   same keyed pass, begins incremental verification of the BIB targets
//!   deferred to the not-yet-resident payload
//!   ([`HeaderVerify::deferred_verifiers`], via
//!   [`hardy_bpv7::checks::begin_payload_verification`]); the dispatcher's
//!   payload drain feeds those as the payload streams. The bundle is
//!   **stored as received** — no editing on input — so the removals ride the
//!   metadata and are applied per attempt at the output doors (the egress
//!   rewrite head, the deliver strip), where the BPSec cascade for a
//!   BCB-covered BIB whose target list shrinks runs. Used by
//!   `dispatcher::ingress`; on a keyed failure returns the recoverable bundle
//!   so the caller can emit a status report.

use bytes::Bytes;
use hardy_bpv7::{
    Bundle as Bpv7Bundle, block, bpsec, bundle_age, checks, parse, status_report::ReasonCode,
};
use tracing::debug;

use super::ExtensionFields;
use crate::{HashMap, HashSet, cla::Segment, stream::Receiver};

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Extract the well-known extension-block fields from freshly-built `Builder`
/// output — a structural `Bundle` plus its wire bytes — so any PreviousNode /
/// BundleAge / HopCount the builder emitted reaches the bundle's metadata. Used
/// by the locally-originated paths (`dispatcher::local`, `dispatcher::report`)
/// that build a bundle and immediately wrap it; the keyed parse pipelines above
/// would do this same extraction after redundant BPSec validation a freshly-built
/// bundle doesn't need.
pub fn extract_from_built(
    bundle: &Bpv7Bundle,
    data: &[u8],
) -> Result<ExtensionFields, hardy_bpv7::Error> {
    extract_extension_block_fields(data, &bundle.blocks, &HashMap::<u64, &[u8]>::new())
}

/// Map a keyed-validation error to the status-report reason BPA emits with the
/// deletion notice. Used by [`parse_headers`] and the payload drain's
/// [`TailFailure::reason_code`](super::tail::TailFailure::reason_code).
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

/// The §5.6 reception-reporting facts the header verify established: what
/// reason the reception assertion carries, and whether a block's own
/// `report_on_failure` flag demands the report be emitted regardless of the
/// bundle-level receipt flag (§5.6 Step 4's block-flag-alone trigger). The
/// nonsense states — a demanded "No additional information", a non-demanded
/// "Block unsupported" — are unrepresentable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceptionReport {
    /// Nothing beyond §5.6 Step 2: reason "No additional information",
    /// emitted only when the bundle requests reception reports.
    Requested,
    /// A §5.1.1 failure-drop was scheduled: reason "Failed security
    /// operation", still bundle-flag-gated (RFC 9172 keeps failure
    /// reporting at requested-MAY level).
    FailureDropped,
    /// A block's `report_on_failure` flag demands the report: emitted even
    /// when the bundle-level receipt flag is clear. Carries "Block
    /// unsupported" / "Unknown security operation" — or "Failed security
    /// operation" when a failure-drop also fired and outranks them in the
    /// record's one reason slot. Never produced for an admin-record or
    /// anonymous bundle: the parser rejects the flag combination (§4.2.4).
    Demanded(ReasonCode),
}

impl ReceptionReport {
    /// The reason code the reception assertion carries.
    pub fn reason(&self) -> ReasonCode {
        match self {
            Self::Requested => ReasonCode::NoAdditionalInformation,
            Self::FailureDropped => ReasonCode::FailedSecurityOperation,
            Self::Demanded(reason) => *reason,
        }
    }

    /// §5.6 Step 4's block-flag-alone trigger: the report is emitted even
    /// when the bundle-level receipt flag is clear.
    pub fn demanded(&self) -> bool {
        matches!(self, Self::Demanded(_))
    }
}

/// Reception-reporting facts from the §A `report_on_failure` classification
/// plus the §5.1.1 failure-drop outcome. The RFC 9172 security codes outrank
/// the generic RFC 9171 block code when several fire: a dropped corrupt
/// operation is the most material event, then an operation this node cannot
/// understand, then an unrecognised plain block — but only the block-flag
/// facts make the report [`Demanded`](ReceptionReport::Demanded).
pub fn reception_report_for(
    classification: &checks::Classification,
    failure_dropped: bool,
) -> ReceptionReport {
    let demanded =
        classification.report_unsupported_security || classification.report_unsupported_block;
    match (demanded, failure_dropped) {
        (false, false) => ReceptionReport::Requested,
        (false, true) => ReceptionReport::FailureDropped,
        (true, _) => ReceptionReport::Demanded(if failure_dropped {
            ReasonCode::FailedSecurityOperation
        } else if classification.report_unsupported_security {
            ReasonCode::UnknownSecurityOperation
        } else {
            ReasonCode::BlockUnsupported
        }),
    }
}

// ---------------------------------------------------------------------------
// Validate — one-shot keyed validation of a complete buffer, no rewriting
// ---------------------------------------------------------------------------

/// One-shot keyed validation of a complete in-memory bundle. Returns the
/// validated structural [`Bpv7Bundle`], its decoded [`ExtensionFields`],
/// **and** `nokey_ext` — the §C8 extension blocks
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
) -> Result<(Bpv7Bundle, ExtensionFields, Vec<(u64, block::Type)>), hardy_bpv7::Error>
where
    F: FnOnce(&Bpv7Bundle, &[u8]) -> Box<dyn bpsec::key::KeySource>,
{
    let parse::Parsed {
        data,
        mut bundle,
        bcbs: bcb_ops,
        bibs: mut bib_ops,
    } = parse::parse(data)?;
    let key_source = key_provider(&bundle, &data);

    // §A — no removals scheduled, but `?` still catches an Unsupported
    // `delete_bundle_on_failure` block.
    checks::classify_unsupported(&bundle.blocks, &bcb_ops, &bib_ops, &[])?;

    // §B + §C8 + §C7 — composed keyed verification. A §C8 decrypt failure is
    // rejected. (A complete buffer, so `verify` drains the op-maps fully — block
    // 1 is verified inline.)
    let mut decrypted = HashMap::new();
    let no_updates = HashMap::new();
    let facts = checks::verify(
        &data,
        &*key_source,
        &mut bundle.blocks,
        &bcb_ops,
        &mut bib_ops,
        &mut decrypted,
        &no_updates,
    )?;
    if !facts.failed.is_empty() {
        return Err(bpsec::Error::DecryptionFailed.into());
    }

    // §D — extract extension fields; the caller writes them into metadata.
    let extracted = extract_extension_block_fields(&data, &bundle.blocks, &decrypted)?;
    Ok((bundle, extracted, facts.nokey_ext.into_vec()))
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
// Streaming ingress — the pre-drain header pass. The gate early-rejects on it
// before the payload is spooled; the dispatcher's drain then streams and
// verifies the payload.
// ---------------------------------------------------------------------------

/// Result of the pre-drain header pass: everything the streaming gate needs to
/// decide whether to drain, plus the inputs the dispatcher's payload drain
/// needs to finish once the payload streams.
pub struct HeaderVerify {
    pub bundle: Bpv7Bundle,
    pub extracted: ExtensionFields,
    /// Unrecognised / unsupported blocks to drop in the post-drain §E rewrite.
    pub to_remove: HashSet<u64>,
    /// The §5.6 reception-reporting facts (see [`reception_report_for`]).
    /// Carried on the reception assertion whether the bundle is accepted or
    /// rejected downstream — Step 4's facts precede either outcome — though
    /// a reject's deletion reason takes the report's one reason slot when
    /// both are asserted.
    pub report: ReceptionReport,
    /// One incremental verifier per BIB op-set `checks::verify` left targeting
    /// the not-yet-resident payload (block 1), each paired with its BIB's
    /// block number for failure attribution. Begun (via
    /// [`hardy_bpv7::checks::begin_payload_verification`]) inside the header
    /// pass, where the key source already exists: the `!Send` source is
    /// resolved once per bundle and stays sync-scoped — only these `Send`
    /// verifiers (carrying copied key material, the recorded exception) cross
    /// the drain's `await`s. The dispatcher's payload drain feeds and settles
    /// them. Empty when the payload was resident. A block-1 *BCB* (payload
    /// confidentiality) needs no deferral — it's decrypted at delivery via
    /// [`hardy_bpv7::bpsec::block_data`].
    pub deferred_verifiers: Vec<(u64, bpsec::bib::Verifier)>,
}

impl HeaderVerify {
    /// Header-only early-reject reason, if any: the bundle is past its lifetime,
    /// or a Hop Count block has reached its limit. Computed straight off the
    /// parsed primary + extracted extension fields, so the streaming gate can
    /// run it before the payload is drained (no reshape into the rich form).
    pub fn gate_reason(&self, received_at: time::OffsetDateTime) -> Option<ReasonCode> {
        let primary = &self.bundle.primary;
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

/// Why [`parse_headers`] failed. The first two are the CLA's business —
/// the transfer must not be acknowledged ([`Cancelled`](Self::Cancelled)) or
/// must be refused ([`TooLarge`](Self::TooLarge)) — while
/// [`Invalid`](Self::Invalid) is an internal drop: the transfer itself
/// completed, the content is just not a valid bundle.
// The recoverable bundle rides the cold error path by value — the same
// trade recorded by the `result_large_err` allow on `parse_headers`.
#[allow(clippy::large_enum_variant)]
pub enum HeaderFailure {
    /// The producer went away before the bundle completed.
    Cancelled,
    /// The accumulated stream crossed the caller's size bound.
    TooLarge { size: usize, max: usize },
    /// Structural or keyed-validation failure. When the bundle id was
    /// recoverable the caller reports the drop — reception then deletion,
    /// the deletion citing the reason (RFC 9171 §5.6/§5.10) — then drops.
    Invalid(Option<(Bpv7Bundle, ReasonCode)>),
}

/// Drive the structural parser off the segment stream up to the parsed header
/// chain (*without* draining an oversized payload), then run the keyed header
/// verification against the resident bytes — the streaming gate's whole
/// pre-drain stage in one call. The header verification begins incremental
/// verification of the payload-block BIBs ([`HeaderVerify::deferred_verifiers`])
/// for the dispatcher's streaming payload drain to feed and settle.
///
/// `Ok` is the verified headers, the resident header `Bytes` (the whole bundle
/// when it fit, else the `consumed` prefix), the payload `tail` the caller
/// drains — the drain continues the byte count this pass starts against
/// `max_size`, which here bounds hostile unbounded header chains — and the
/// header-region BCB OperationSets for the caller's Ingress gate chain, handed
/// back from the one decode rather than re-derived. `Err` is a
/// [`HeaderFailure`]; see its variants for who handles what.
#[allow(clippy::result_large_err, clippy::type_complexity)]
pub async fn parse_headers<F>(
    stream: &mut dyn Receiver<Segment>,
    max_size: usize,
    key_provider: F,
) -> Result<
    (
        HeaderVerify,
        Bytes,
        Option<parse::PayloadTail>,
        HashMap<u64, bpsec::bcb::OperationSet>,
    ),
    HeaderFailure,
>
where
    F: FnOnce(&Bpv7Bundle, &[u8]) -> Box<dyn bpsec::key::KeySource>,
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
                Err(e) => {
                    debug!("Bundle BPSec structural validation failed: {e}");
                    return Err(HeaderFailure::Invalid(None));
                }
            },
            // A `Partial` after the stream has already ended is a truncated
            // bundle: the declared payload cannot complete (`tail.remaining()`
            // is positive), so reject it exactly like `NeedMore` at end-of-
            // stream instead of handing an exhausted stream to the payload drain.
            Ok(parse::ParserProgress::Partial { .. }) if last => {
                debug!("Truncated bundle (oversized payload, stream ended)");
                return Err(HeaderFailure::Invalid(None));
            }
            Ok(parse::ParserProgress::Partial { consumed, tail }) => {
                match parser.finish(consumed.clone()) {
                    Ok(parsed) => break (parsed, consumed, Some(tail)),
                    Err(e) => {
                        debug!("Bundle BPSec structural validation failed: {e}");
                        return Err(HeaderFailure::Invalid(None));
                    }
                }
            }
            Err(e) => {
                debug!("Bundle structural parse failed: {e}");
                return Err(HeaderFailure::Invalid(None));
            }
        }
    };

    // Header verification (§A–§D) against the resident bytes. On a keyed failure
    // the recoverable `bundle` is returned so the caller can report the drop;
    // on success it moves into the returned `HeaderVerify`.
    let parse::Parsed {
        mut bundle,
        bcbs: bcb_ops,
        bibs: mut bib_ops,
        ..
    } = parsed;
    let key_source = key_provider(&bundle, &headers);
    match verify_headers(&headers, &*key_source, &mut bundle, &bcb_ops, &mut bib_ops) {
        Ok((extracted, to_remove, report, deferred_verifiers)) => Ok((
            HeaderVerify {
                bundle,
                extracted,
                to_remove,
                report,
                deferred_verifiers,
            },
            headers,
            tail,
            // The header-region BCB OperationSets ride back to the caller for
            // the Ingress gate chain, handed back from this one decode rather
            // than re-derived from the prefix later.
            bcb_ops,
        )),
        Err(error) => {
            debug!("Invalid bundle received: {error}");
            Err(HeaderFailure::Invalid(Some((
                bundle,
                status_report_reason_for(&error),
            ))))
        }
    }
}

/// Header verification (§A classify → §B/§C8/§C7 verify → §D extract) against the
/// resident `headers` buffer — the `consumed` prefix for an oversized streamed
/// payload, or the whole bundle otherwise. Mutates `bundle.blocks` (BIB coverage
/// stamps). Returns the extracted extension fields, the blocks to remove, the
/// reception-report reason, and one begun incremental verifier per block-1
/// (payload) op-set the keyed verify deferred, for the dispatcher's payload
/// drain to feed as the payload streams; the §E removals are deferred to the
/// output doors too.
#[allow(clippy::type_complexity)]
fn verify_headers(
    headers: &[u8],
    key_source: &dyn bpsec::key::KeySource,
    bundle: &mut Bpv7Bundle,
    bcb_ops: &HashMap<u64, bpsec::bcb::OperationSet>,
    bib_ops: &mut HashMap<u64, bpsec::bib::OperationSet>,
) -> Result<
    (
        ExtensionFields,
        HashSet<u64>,
        ReceptionReport,
        Vec<(u64, bpsec::bib::Verifier)>,
    ),
    hardy_bpv7::Error,
> {
    // §A — classify; collect deletables; the report_* facts feed the
    // reception-report reason below.
    let classification = checks::classify_unsupported(&bundle.blocks, bcb_ops, bib_ops, &[])?;

    let mut to_remove: HashSet<u64> = HashSet::new();
    to_remove.extend(classification.unrecognised_deletable.iter().copied());
    for n in &classification.bib_deletable {
        to_remove.insert(*n);
        bib_ops.remove(n);
    }

    // §B + §C8 + §C7 — composed keyed verification. NoKey on §C8 is fatal for
    // HopCount and unclocked BundleAge; a §C8/§B decrypt failure is rejected.
    // `verify` drains the deferred block-1 (payload) op-sets out of `bib_ops`,
    // handing them back owned in `facts.deferred_bibs`; `bcb_ops` is only
    // borrowed.
    let mut decrypted = HashMap::new();
    let to_update_seed: HashMap<u64, Vec<u8>> = HashMap::new();
    let facts = checks::verify(
        headers,
        key_source,
        &mut bundle.blocks,
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
    let is_clocked = bundle.primary.id.timestamp.is_clocked();
    for &target in &facts.failed {
        if target == 1
            || bundle
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
    let report = reception_report_for(&classification, !facts.failed.is_empty());

    // Ingress accepts/forwards, so an undecipherable liveness block is fatal; any
    // other undecipherable block is forwarded intact for a downstream acceptor.
    reject_undecryptable_liveness(&facts.nokey_ext, is_clocked)?;

    // §D — decode the well-known extension fields; the caller records them in
    // the bundle's metadata. Decode only: no canonical re-emission is queued
    // (non-canonical CBOR is rejected at parse). Extension blocks only —
    // never the payload, so header-resident.
    let extracted = extract_extension_block_fields(headers, &bundle.blocks, &decrypted)?;

    // Begin incremental verification of the deferred block-1 (payload)
    // targets here, where `key_source` already exists: the source (possibly
    // an expensive provider lookup) is resolved once per bundle and never
    // crosses an `await` — only the returned `Send` verifiers, carrying
    // copied key material (the recorded exception), ride the async drain.
    // Empty deferral (a resident payload) yields an empty vec.
    let deferred_verifiers = checks::begin_payload_verification(
        headers,
        key_source,
        &bundle.blocks,
        &facts.deferred_bibs,
    )?;

    Ok((extracted, to_remove, report, deferred_verifiers))
}

// ---------------------------------------------------------------------------
// §D — extension-block field extraction
//
// Decodes the well-known PreviousNode / BundleAge / HopCount extension blocks
// into typed values the BPA records in metadata. BPA policy — bpv7 keeps only
// the structural parse + per-section BPSec primitives.
// ---------------------------------------------------------------------------

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
        // No plaintext from §C8: `extract` itself returns `Ok(None)` for a
        // BCB-covered block (ciphertext in place) or a non-resident payload.
        None => block.extract(source),
    }
}

/// Decode `PreviousNode` / `BundleAge` / `HopCount` block bodies into an
/// [`ExtensionFields`]. Non-canonical encodings are rejected at decode
/// (RFC 9171 §4.1), not re-emitted — canonicalisation is a configurable mutating
/// filter. Generic over the decrypted-plaintext container so the BPSec
/// `Zeroizing` type never needs naming here.
fn extract_extension_block_fields<V: AsRef<[u8]>>(
    data: &[u8],
    blocks: &HashMap<u64, block::Block>,
    decrypted_data: &HashMap<u64, V>,
) -> Result<ExtensionFields, hardy_bpv7::Error> {
    let mut out = ExtensionFields::default();

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
        tx.send(Segment::Final(Bytes::from(data)))
            .await
            .expect("channel open");

        let Ok((hv, _headers, tail, _)) = parse_headers(&mut rx, 1 << 20, |_, _| keys()).await
        else {
            panic!("headers must verify: only the corrupt BIB target fails");
        };
        assert!(tail.is_none(), "the small bundle is fully resident");

        // §5.1.1 failure-drop is *scheduled* at ingress, not applied: the
        // bundle is stored as received and the corrupt target rides the removal
        // set to the output doors, where the BPSec cascade runs per attempt
        // (the shared BCB survives there, still covering the payload). A small
        // resident bundle defers no payload BIB, so the header pass already
        // holds the complete schedule.
        assert!(
            hv.deferred_verifiers.is_empty(),
            "a resident bundle defers no BIB"
        );
        let mut to_remove: Vec<u64> = hv.to_remove.iter().copied().collect();
        to_remove.sort_unstable();
        assert_eq!(to_remove, vec![3], "only the corrupt target is scheduled");
        assert!(
            hv.bundle.blocks.contains_key(&3),
            "no editing on input: the corrupt block is still present in the stored bundle"
        );
        assert!(
            hv.bundle.blocks.contains_key(&2),
            "shared BCB retained as received"
        );
        assert!(hv.bundle.blocks.contains_key(&1), "payload survives");
    }

    // The NoKey liveness policy through both real keyed pipelines: a
    // BCB-encrypted Hop Count this node has no key for is fatal at ingress
    // (the anti-loop defense cannot be enforced, so the bundle must not be
    // forwarded), while the one-shot validate path returns it as a fact for
    // the call site to adjudicate — `dispatcher::restart` ignores the list
    // (tolerating a since-rotated key), and the accept/forward paths reject
    // through `reject_undecryptable_liveness`.
    #[cfg(feature = "rfc9173")]
    #[tokio::test]
    async fn nokey_hop_count_fatal_at_ingress_a_fact_at_validate() {
        use hardy_bpv7::{
            bpsec::{
                encryptor::{Context, Encryptor},
                key::{EncAlgorithm, Key, Operation, Type},
                no_keys,
            },
            builder::Builder,
            creation_timestamp::CreationTimestamp,
            hop_info::HopInfo,
        };

        let enc_k = Key {
            key_type: Type::OctetSequence {
                key: b"qwertyuiopasdfghqwertyuiopasdfgh".as_slice().into(),
            },
            key_algorithm: None,
            enc_algorithm: Some(EncAlgorithm::A256GCM),
            operations: Some([Operation::Encrypt].into_iter().collect()),
            id: Some("ipn:2.1".into()),
            key_use: None,
        };

        let (built, data) =
            Builder::new("ipn:0.2.1".parse().unwrap(), "ipn:0.3.99".parse().unwrap())
                .with_hop_count(&HopInfo {
                    limit: 64,
                    count: 1,
                })
                .with_payload(b"payload".as_slice().into())
                .build(CreationTimestamp::now())
                .unwrap();
        let hop_block = *built
            .blocks
            .iter()
            .find(|(_, b)| matches!(b.block_type, block::Type::HopCount))
            .expect("builder emitted the Hop Count block")
            .0;

        let encrypted = Bytes::from(
            Encryptor::new(&built, &data)
                .encrypt_block(
                    hop_block,
                    Context::AES_GCM(Default::default()),
                    "ipn:0.2.1".parse().unwrap(),
                    &enc_k,
                )
                .map_err(|(_, e)| e)
                .expect("encrypt the Hop Count block")
                .rebuild()
                .expect("rebuild the encrypted bundle"),
        );

        // Ingress: fatal, with a recoverable bundle for the reception report.
        // (NoKey has no RFC 9172 reason of its own; it maps to the generic
        // BlockUnintelligible.)
        let (tx, mut rx) = hardy_async::channel::bounded(1);
        tx.send(Segment::Final(encrypted.clone()))
            .await
            .expect("channel open");
        match parse_headers(&mut rx, 1 << 20, no_keys).await {
            Err(HeaderFailure::Invalid(Some((_, reason)))) => {
                assert_eq!(reason, ReasonCode::BlockUnintelligible)
            }
            Ok(_) => panic!("an undecryptable Hop Count must be fatal at ingress"),
            Err(_) => panic!("expected Invalid with a recoverable bundle"),
        }

        // Validate: a fact, not a verdict — the Ok is what lets restart
        // tolerate the bundle; the accept/forward call sites then reject it.
        let (_, _, nokey) =
            parse_validate_with_provider(encrypted, no_keys).expect("validate returns the facts");
        assert_eq!(nokey, vec![(hop_block, block::Type::HopCount)]);
        assert!(matches!(
            reject_undecryptable_liveness(&nokey, true),
            Err(hardy_bpv7::Error::InvalidBPSec(bpsec::Error::NoKey))
        ));

        // BundleAge is liveness-critical only on an unclocked node.
        let age_fact = [(9, block::Type::BundleAge)];
        assert!(reject_undecryptable_liveness(&age_fact, true).is_ok());
        assert!(matches!(
            reject_undecryptable_liveness(&age_fact, false),
            Err(hardy_bpv7::Error::InvalidBPSec(bpsec::Error::NoKey))
        ));
    }

    #[test]
    fn reception_report_precedence() {
        let mut c = checks::Classification::default();
        assert_eq!(reception_report_for(&c, false), ReceptionReport::Requested);
        // A failure-drop alone reports, but is not block-demanded.
        assert_eq!(
            reception_report_for(&c, true),
            ReceptionReport::FailureDropped
        );
        c.report_unsupported_block = true;
        assert_eq!(
            reception_report_for(&c, false),
            ReceptionReport::Demanded(ReasonCode::BlockUnsupported)
        );
        c.report_unsupported_security = true;
        assert_eq!(
            reception_report_for(&c, false),
            ReceptionReport::Demanded(ReasonCode::UnknownSecurityOperation)
        );
        // The failure-drop outranks in the reason slot without erasing the
        // block's demand.
        assert_eq!(
            reception_report_for(&c, true),
            ReceptionReport::Demanded(ReasonCode::FailedSecurityOperation)
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
