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
//!   what to do with it. Its one remaining caller is `dispatcher::restart`'s
//!   reconcile walk, which ignores the list (re-check stored data on startup,
//!   tolerating a since-rotated key); it retires when that walk moves to the
//!   streamed pass. The streamed pass applies the liveness policy itself via
//!   [`reject_undecryptable_liveness`].
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
//!   metadata and are applied per attempt at the output doors, ahead of each
//!   door's filter chain, where the BPSec cascade for a BCB-covered BIB
//!   whose target list shrinks runs. Used by
//!   `dispatcher::ingress`; on a keyed failure returns the recoverable bundle
//!   so the caller can emit a status report.

use core::mem::take;

use bytes::{Bytes, BytesMut};
use hardy_bpv7::{
    Bundle as Bpv7Bundle, block, bpsec, bundle_age, checks, parse, status_report::ReasonCode,
};
use time::OffsetDateTime;
use tracing::debug;

use super::{ExtensionFields, expiry};
use crate::{HashMap, HashSet, cla::Segment, stream::Receiver};

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Extract the well-known extension-block fields from freshly-built `Builder`
/// output — a structural `Bundle` plus its wire bytes — so any PreviousNode /
/// BundleAge / HopCount the builder emitted reaches the bundle's metadata. Used
/// by the locally-originated paths (`dispatcher::local`, `dispatcher::report`)
/// that build a bundle and immediately wrap it; the keyed parse pipelines
/// ([`parse_validate_with_provider`], [`parse_headers`]) would do this same
/// extraction after redundant BPSec validation a freshly-built bundle doesn't
/// need.
pub fn extract_from_built(
    bundle: &Bpv7Bundle,
    data: &[u8],
) -> Result<ExtensionFields, hardy_bpv7::Error> {
    extract_extension_block_fields(data, &bundle.blocks, &HashMap::<u64, &[u8]>::new())
}

/// Map a keyed-validation error to the status-report reason BPA emits with the
/// deletion notice. Used by [`parse_headers`] and the payload drain's
/// `ValidationFailure::reason_code` (crate-internal, in `dispatcher`).
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

// The report for a header-pass rejection that precedes the keyed header
// verification: the parsed bundle, the reception facts its own blocks
// establish (the §A classification's `report_on_failure` flags), and the
// deletion reason `error` maps to.
fn reported_drop(
    parsed: parse::Parsed,
    error: &hardy_bpv7::Error,
) -> (Bpv7Bundle, ReceptionReport, ReasonCode) {
    let (classification, _) = checks::classify_unsupported_and_verdict(
        &parsed.bundle.blocks,
        &parsed.bcbs,
        &parsed.bibs,
        &[],
    );
    (
        parsed.bundle,
        reception_report_for(&classification, false),
        status_report_reason_for(error),
    )
}

// ---------------------------------------------------------------------------
// Validate — one-shot keyed validation of a complete buffer, no rewriting
// ---------------------------------------------------------------------------

/// One-shot keyed validation of a complete in-memory bundle, returning the
/// re-validated structural [`Bpv7Bundle`]. The §D extension-field decode still
/// runs — a corrupt well-known block rejects the bundle — but neither the
/// decoded fields nor the §C8 no-key facts are returned: the sole caller,
/// restart's stored-data re-check, deliberately ignores both, tolerating a
/// key that has since rotated away (soft NoKey).
///
/// No block removal, no rewriting — non-canonical CBOR is rejected at parse
/// (RFC 9171 §4.1), and re-emitting it is a configurable mutating-filter concern
/// (see `docs/streaming_pipeline_design.md` §5.2.2), not standard-parser work.
#[allow(clippy::result_large_err)]
pub fn parse_validate_with_provider<F>(
    data: Bytes,
    key_provider: F,
) -> Result<Bpv7Bundle, hardy_bpv7::Error>
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

    // §D — decode the extension fields purely as validation: a corrupt
    // well-known block rejects the bundle; the decoded values are discarded.
    extract_extension_block_fields(&data, &bundle.blocks, &decrypted).map(|_| bundle)
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
/// `nokey` is `VerifyFacts::nokey_ext`.
/// A node that accepts/forwards applies this; a restart re-check tolerates a
/// key that has since rotated away and skips it.
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
    pub extensions: ExtensionFields,
    /// Unrecognised / unsupported blocks scheduled for removal: the output
    /// doors drop them per attempt (`BundleMetadata::to_remove`).
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
    /// [`hardy_bpv7::bpsec::DecryptingReader`].
    pub deferred_verifiers: Vec<(u64, bpsec::bib::Verifier)>,
}

impl HeaderVerify {
    /// Header-only early-reject reason, if any: the bundle is past its lifetime
    /// (the shared [`expiry`] rule, so the gate and the post-store expiry
    /// checkpoints agree), or a Hop Count block has reached its limit. Computed
    /// straight off the parsed primary + extracted extension fields, so the
    /// streaming gate can run it before the payload is drained.
    pub fn gate_reason(&self, received_at: OffsetDateTime) -> Option<ReasonCode> {
        if expiry(&self.bundle.primary, self.extensions.age, received_at)
            <= OffsetDateTime::now_utc()
        {
            Some(ReasonCode::LifetimeExpired)
        } else if self
            .extensions
            .hop_count
            .as_ref()
            .is_some_and(|h| h.count > u64::from(h.limit.get()))
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
    /// The caller's size bound was crossed — by the accumulated stream, or
    /// by the whole-bundle wire size the header chain declares (canonical
    /// CBOR gives the payload a definite-length head, so the full size is
    /// known before the payload arrives). `u64` end to end: a declared
    /// size may exceed a 32-bit target's address space.
    TooLarge { size: u64, max: u64 },
    /// Structural or keyed-validation failure. `error` is the parse or
    /// verification failure itself (the originate door surfaces it to the
    /// caller); `report` carries the recoverable bundle, the reception facts
    /// established before the failure, and the status-report reason when the
    /// bundle id survived — the CLA ingress door reports the drop with it,
    /// reception then deletion, the deletion citing the reason (RFC 9171
    /// §5.6/§5.10).
    Invalid {
        error: hardy_bpv7::Error,
        report: Option<(Bpv7Bundle, ReceptionReport, ReasonCode)>,
    },
}

/// Drive the structural parser off the segment stream up to the parsed header
/// chain (*without* draining a payload still to arrive), then run the keyed header
/// verification against the resident bytes — the streaming gate's whole
/// pre-drain stage in one call. The header verification begins incremental
/// verification of the payload-block BIBs ([`HeaderVerify::deferred_verifiers`])
/// for the dispatcher's streaming payload drain to feed and settle.
///
/// Before verifying, the pass holds the payload peek: it pulls segments until
/// the payload's first `peek` bytes (all of it, if shorter) are resident, each
/// fed through the tail, so the gate's chain can read them. A `peek` of 0, or
/// a payload with no peekable prefix ([`peekable_payload`]: one a BCB covers,
/// or a fragment past the payload's start), pulls nothing past the payload
/// block's header.
///
/// `Ok` is the verified headers, the resident `Bytes` (the whole bundle when
/// it completed in this pass, else the headers and whatever of the payload
/// block has arrived), the payload `tail` the caller drains — the drain
/// continues the byte count this pass starts against `max_size`, which here
/// bounds hostile unbounded header chains and, once the chain parses,
/// refuses a declared whole-bundle size over the bound before a single
/// payload byte drains — and the header-region BCB OperationSets for the
/// caller's Ingress gate chain, handed back from the one decode rather than
/// re-derived. `Err` is a [`HeaderFailure`]; see its variants for who
/// handles what.
#[allow(clippy::result_large_err, clippy::type_complexity)]
pub async fn parse_headers<F>(
    stream: &mut dyn Receiver<Segment>,
    max_size: u64,
    peek: usize,
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
    // (the whole bundle, or the `consumed` prefix of one still arriving);
    // `tail` (if any) drains the rest back in `dispatcher::ingress`.
    let mut arrival = Arrival {
        stream,
        total: 0,
        max: max_size,
    };
    let (parsed, headers, tail) = loop {
        let (bytes, last) = arrival.next().await?;
        match parser.push(bytes) {
            // The stream ended inside the header region: with no parsed
            // headers there is no bundle id to report — the §4.1 discard.
            Ok(parse::ParserProgress::NeedMore(n)) if last => {
                debug!("Truncated bundle: the stream ended inside its headers");
                return Err(HeaderFailure::Invalid {
                    error: hardy_cbor::decode::Error::NeedMoreData(n).into(),
                    report: None,
                });
            }
            Ok(parse::ParserProgress::NeedMore(_)) => {}
            Ok(parse::ParserProgress::Ready(whole)) => match parser.finish(whole.clone()) {
                Ok(parsed) => {
                    break (
                        await_final(&mut *arrival.stream, parsed, last).await?,
                        whole,
                        None,
                    );
                }
                Err(e) => {
                    debug!("Bundle BPSec structural validation failed: {e}");
                    return Err(HeaderFailure::Invalid {
                        error: e,
                        report: None,
                    });
                }
            },
            // A `Partial` from the stream's last segment is never handed to
            // the payload drain, whose stream is exhausted; its headers
            // parsed, so the drop is reported.
            Ok(parse::ParserProgress::Partial { consumed, tail }) if last => {
                let parsed = match parser.finish(consumed) {
                    Ok(parsed) => parsed,
                    Err(e) => {
                        debug!("Bundle BPSec structural validation failed: {e}");
                        return Err(HeaderFailure::Invalid {
                            error: e,
                            report: None,
                        });
                    }
                };
                return Err(tail_rejected_at_final(tail, parsed));
            }
            Ok(parse::ParserProgress::Partial { consumed, tail }) => {
                match parser.finish(consumed.clone()) {
                    Ok(parsed) => break (parsed, consumed, Some(tail)),
                    Err(e) => {
                        debug!("Bundle BPSec structural validation failed: {e}");
                        return Err(HeaderFailure::Invalid {
                            error: e,
                            report: None,
                        });
                    }
                }
            }
            Err(e) => {
                debug!("Bundle structural parse failed: {e}");
                return Err(HeaderFailure::Invalid {
                    error: e,
                    report: None,
                });
            }
        }
    };

    // The header chain declares the whole wire size, so the caller's bound
    // applies to the full bundle here — resident or still on the wire —
    // before a single payload byte is drained, and before any keyed work
    // below is spent on a bundle that will be refused.
    let declared = parsed.bundle.encoded_len();
    if declared > max_size {
        return Err(HeaderFailure::TooLarge {
            size: declared,
            max: max_size,
        });
    }

    // The payload peek is held once the bundle is admissible by size, and
    // before the keyed work, which then verifies over the held bytes too.
    let (parsed, headers, tail) = match tail {
        Some(tail) => hold_peek(&mut arrival, parsed, headers, tail, peek).await?,
        None => (parsed, headers, None),
    };

    // Header verification (§A–§D) against the resident bytes. On a keyed failure
    // the recoverable `bundle` is returned so the caller can report the drop;
    // on success it moves into the returned `HeaderVerify`.
    let parse::Parsed {
        bundle,
        bcbs: bcb_ops,
        bibs: mut bib_ops,
        ..
    } = parsed;
    let key_source = key_provider(&bundle, &headers);
    match verify_headers(&headers, &*key_source, bundle, &bcb_ops, &mut bib_ops) {
        // The header-region BCB OperationSets ride back to the caller for
        // the Ingress gate chain, handed back from this one decode rather
        // than re-derived from the prefix later.
        Ok(hv) => Ok((hv, headers, tail, bcb_ops)),
        Err((bundle, error, reception)) => {
            debug!("Invalid bundle received: {error}");
            let reason = status_report_reason_for(&error);
            Err(HeaderFailure::Invalid {
                error,
                report: Some((bundle, reception, reason)),
            })
        }
    }
}

// A bundle complete before its stream's `Final` commits only once the stream
// confirms the end: until then the producer can still abandon the transfer.
// Empty `Next` segments are padding the `Segment` contract allows; a byte past
// the outer break is reported as the payload drain reports one.
#[allow(clippy::result_large_err)]
async fn await_final(
    stream: &mut dyn Receiver<Segment>,
    parsed: parse::Parsed,
    mut ended: bool,
) -> Result<parse::Parsed, HeaderFailure> {
    while !ended {
        match stream.recv().await {
            Ok(Segment::Next(b)) if b.is_empty() => {}
            Ok(Segment::Final(b)) if b.is_empty() => ended = true,
            Ok(_) => {
                debug!("Bytes follow a complete bundle");
                let error = hardy_bpv7::Error::AdditionalData;
                let report = Some(reported_drop(parsed, &error));
                return Err(HeaderFailure::Invalid { error, report });
            }
            Err(_) => {
                debug!("Bundle stream cancelled after a complete bundle");
                return Err(HeaderFailure::Cancelled);
            }
        }
    }
    Ok(parsed)
}

// The arrival as the header pass pulls it: each segment counts against the
// caller's size bound, a count the payload drain continues. Every return from
// the pass leaves exactly the counted bytes resident (`headers.len() ==
// total`), so the drain counts on from the resident bytes.
struct Arrival<'s> {
    stream: &'s mut dyn Receiver<Segment>,
    total: u64,
    max: u64,
}

impl Arrival<'_> {
    // The next segment's bytes, and whether it is the stream's `Final`. A
    // producer gone away is `Cancelled`, a count past the bound `TooLarge`.
    #[allow(clippy::result_large_err)]
    async fn next(&mut self) -> Result<(Bytes, bool), HeaderFailure> {
        let (bytes, last) = match self.stream.recv().await {
            Ok(Segment::Next(b)) => (b, false),
            Ok(Segment::Final(b)) => (b, true),
            Err(_) => {
                debug!("Bundle stream cancelled");
                return Err(HeaderFailure::Cancelled);
            }
        };
        self.total = self.total.saturating_add(bytes.len() as u64);
        if self.total > self.max {
            return Err(HeaderFailure::TooLarge {
                size: self.total,
                max: self.max,
            });
        }
        Ok((bytes, last))
    }
}

// A complete transfer whose payload block the tail did not accept — the
// stream ended inside it, or a trailer check failed on the bytes that came —
// reported as the payload drain reports the same failure.
fn tail_rejected_at_final(tail: parse::PayloadTail, parsed: parse::Parsed) -> HeaderFailure {
    let error = tail
        .finish()
        .expect_err("a tail incomplete at `Final` is unfinished or failed");
    debug!("Payload block rejected at the stream's Final: {error}");
    let report = Some(reported_drop(parsed, &error));
    HeaderFailure::Invalid { error, report }
}

/// The payload block, when the bytes resident ahead of its body's end are the
/// payload's own prefix: `None` for a payload a BCB covers, which no filter
/// reads, and for a fragment past the payload's start, whose bytes are not
/// the payload's prefix. The rule the header pass's hold and the filters'
/// `payload_peek` share.
pub fn peekable_payload(bundle: &Bpv7Bundle) -> Option<&block::Block> {
    let later_fragment = bundle
        .primary
        .id
        .fragment_info
        .as_ref()
        .is_some_and(|fragment| fragment.offset > 0);
    if later_fragment {
        return None;
    }
    bundle
        .blocks
        .get(&1)
        .filter(|payload| payload.bcb.is_none())
}

// The header pass's result before keyed verification: the parsed bundle, its
// resident bytes, and the payload tail still to drain, if any.
type Resident = (parse::Parsed, Bytes, Option<parse::PayloadTail>);

// Holds the payload peek: pulls segments until the payload's first `peek`
// bytes (all of it, if shorter) are resident, and returns them with
// `consumed` as the resident bytes. Each pulled segment is fed to `tail`,
// which runs on it the checks the drain would run on the same bytes, so a
// trailer failure here is reported at once. A tail that completes leaves the
// whole bundle resident, and it commits on the stream's `Final` as one that
// arrived whole does.
#[allow(clippy::result_large_err)]
async fn hold_peek(
    arrival: &mut Arrival<'_>,
    mut parsed: parse::Parsed,
    consumed: Bytes,
    mut tail: parse::PayloadTail,
    peek: usize,
) -> Result<Resident, HeaderFailure> {
    // A `Partial` has parsed the payload block's header, so its data range
    // is known though its bytes are not all resident. Nothing is held for a
    // payload with no peekable prefix (`peekable_payload`), nor, defensively,
    // for a missing payload block, which a `Partial` never has. Compared in
    // u64: the range is wire-derived.
    let held_to = peekable_payload(&parsed.bundle).map_or(0, |payload| {
        let body = payload.payload_range();
        body.start + (peek as u64).min(body.end - body.start)
    });
    if consumed.len() as u64 >= held_to {
        return Ok((parsed, consumed, Some(tail)));
    }
    // Nothing downstream reads the parse's own view of the buffer. Dropping
    // it leaves `consumed` uniquely held when the parser owned the buffer, so
    // the hold extends it in place; when one segment held the whole header
    // region, `consumed` still shares it, and the hold copies.
    drop(take(&mut parsed.data));
    let mut held = consumed
        .try_into_mut()
        .unwrap_or_else(|consumed| BytesMut::from(&consumed[..]));
    loop {
        let (bytes, last) = arrival.next().await?;
        held.extend_from_slice(&bytes);
        match tail.push(&bytes) {
            Ok(true) => {
                let parsed = await_final(&mut *arrival.stream, parsed, last).await?;
                return Ok((parsed, held.freeze(), None));
            }
            Ok(false) if last => return Err(tail_rejected_at_final(tail, parsed)),
            Ok(false) if held.len() as u64 >= held_to => {
                return Ok((parsed, held.freeze(), Some(tail)));
            }
            Ok(false) => {}
            Err(error) => {
                debug!("Payload block rejected while holding the payload peek: {error}");
                let report = Some(reported_drop(parsed, &error));
                return Err(HeaderFailure::Invalid { error, report });
            }
        }
    }
}

/// Header verification (§A classify → §B/§C8/§C7 verify → §D extract) against the
/// resident `headers` buffer — the `consumed` prefix of a bundle still
/// arriving, or the whole bundle otherwise. Takes the structural bundle by value
/// and returns it inside the assembled [`HeaderVerify`] (BIB coverage stamps
/// applied, and one begun incremental verifier per block-1 (payload) op-set
/// the keyed verify deferred, for the dispatcher's payload drain to feed as
/// the payload streams; the §E removals are deferred to the output doors
/// too). On a keyed failure the recoverable bundle rides the error, with the
/// reception facts established before it.
#[allow(clippy::result_large_err)]
fn verify_headers(
    headers: &[u8],
    key_source: &dyn bpsec::key::KeySource,
    bundle: Bpv7Bundle,
    bcb_ops: &HashMap<u64, bpsec::bcb::OperationSet>,
    bib_ops: &mut HashMap<u64, bpsec::bib::OperationSet>,
) -> Result<HeaderVerify, (Bpv7Bundle, hardy_bpv7::Error, ReceptionReport)> {
    // Assembled up front with empty facts; the verification closure fills
    // them in place (keeping `?` ergonomics — its borrow ends at the call),
    // and the recoverable bundle rides whichever arm results.
    let mut hv = HeaderVerify {
        bundle,
        extensions: ExtensionFields::default(),
        to_remove: HashSet::new(),
        report: ReceptionReport::Requested,
        deferred_verifiers: Vec::new(),
    };

    let verified = (|hv: &mut HeaderVerify| {
        // §A — classify; collect deletables. The report_* facts stand from
        // here, so every rejection below — a block demanding the bundle's
        // deletion (RFC 9171 §5.6 Step 4 reports, then deletes) or a keyed
        // failure — still honours a block's report demand.
        let (classification, delete_bundle) =
            checks::classify_unsupported_and_verdict(&hv.bundle.blocks, bcb_ops, bib_ops, &[]);
        hv.report = reception_report_for(&classification, false);
        if let Some(error) = delete_bundle {
            return Err(error);
        }

        hv.to_remove
            .extend(classification.unrecognised_deletable.iter().copied());
        for n in &classification.bib_deletable {
            hv.to_remove.insert(*n);
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
            &mut hv.bundle.blocks,
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
        let is_clocked = hv.bundle.primary.id.timestamp.is_clocked();
        for &target in &facts.failed {
            if target == 1
                || hv
                    .bundle
                    .blocks
                    .get(&target)
                    .is_some_and(|b| is_liveness_critical(b.block_type, is_clocked))
            {
                return Err(bpsec::Error::DecryptionFailed.into());
            }
            hv.to_remove.insert(target);
        }
        // Anything still in `facts.failed` here was queued for failure-drop (the
        // fatal cases returned above) — surface that in the reception report.
        hv.report = reception_report_for(&classification, !facts.failed.is_empty());

        // Ingress accepts/forwards, so an undecipherable liveness block is fatal; any
        // other undecipherable block is forwarded intact for a downstream acceptor.
        reject_undecryptable_liveness(&facts.nokey_ext, is_clocked)?;

        // §D — decode the well-known extension fields; the caller records them in
        // the bundle's metadata. Decode only: no canonical re-emission is queued
        // (non-canonical CBOR is rejected at parse). Extension blocks only —
        // never the payload, so header-resident.
        hv.extensions = extract_extension_block_fields(headers, &hv.bundle.blocks, &decrypted)?;

        // Begin incremental verification of the deferred block-1 (payload)
        // targets here, where `key_source` already exists: the source
        // (possibly an expensive provider lookup) is resolved once per
        // bundle and never crosses an `await` — only the returned `Send`
        // verifiers, carrying copied key material (the recorded exception),
        // ride the async drain. Empty deferral (a resident payload) yields
        // an empty vec.
        hv.deferred_verifiers = checks::begin_payload_verification(
            headers,
            key_source,
            &hv.bundle.blocks,
            &facts.deferred_bibs,
        )?;

        Ok::<_, hardy_bpv7::Error>(())
    })(&mut hv);

    match verified {
        Ok(()) => Ok(hv),
        Err(e) => Err((hv.bundle, e, hv.report)),
    }
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
    use hardy_bpv7::{builder::Builder, creation_timestamp::CreationTimestamp};
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
                    key_type: Type::octet_sequence(hex!("1a2b1a2b1a2b1a2b1a2b1a2b1a2b1a2b")),
                    key_algorithm: Some(KeyAlgorithm::HS384),
                    enc_algorithm: None,
                    operations: Some([Operation::Verify].into_iter().collect()),
                    id: Some("ipn:2.1".into()),
                    key_use: None,
                },
                Key {
                    key_type: Type::octet_sequence(b"qwertyuiopasdfghqwertyuiopasdfgh".as_slice()),
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

        let Ok((hv, _headers, tail, _)) = parse_headers(&mut rx, 1 << 20, 0, |_, _| keys()).await
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
        use core::num::NonZeroU8;

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
        use rand::{TryRng, rngs::SysRng};

        // Immaterial key value: the test parses with `no_keys`, so the
        // key only has to encrypt; generated per the no-literal-keys rule.
        let mut enc_k_bytes = vec![0u8; 32];
        SysRng.try_fill_bytes(&mut enc_k_bytes).unwrap();
        let enc_k = Key {
            key_type: Type::octet_sequence(enc_k_bytes),
            key_algorithm: None,
            enc_algorithm: Some(EncAlgorithm::A256GCM),
            operations: Some([Operation::Encrypt].into_iter().collect()),
            id: Some("ipn:2.1".into()),
            key_use: None,
        };

        let (built, data) =
            Builder::new("ipn:0.2.1".parse().unwrap(), "ipn:0.3.99".parse().unwrap())
                .with_hop_count(&HopInfo {
                    limit: NonZeroU8::new(64).unwrap(),
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
        match parse_headers(&mut rx, 1 << 20, 0, no_keys).await {
            Err(HeaderFailure::Invalid {
                report: Some((_, _, reason)),
                ..
            }) => {
                assert_eq!(reason, ReasonCode::BlockUnintelligible)
            }
            Ok(_) => panic!("an undecryptable Hop Count must be fatal at ingress"),
            Err(_) => panic!("expected Invalid with a recoverable bundle"),
        }

        // Validate: tolerant by construction — the Ok is what lets restart
        // re-admit stored data whose key has rotated away; the ingress door
        // above is the one that adjudicates.
        parse_validate_with_provider(encrypted, no_keys)
            .expect("validate tolerates an undecryptable Hop Count");
        assert!(matches!(
            reject_undecryptable_liveness(&[(hop_block, block::Type::HopCount)], true),
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

    // A keyed verification failure keeps the facts the §A classification
    // established: an unrecognised block flagged `report_on_failure` still
    // demands its reception report (RFC 9171 §5.6 Step 4) when a BIB over
    // another block fails integrity, and the deletion cites the failed
    // security operation.
    #[cfg(feature = "rfc9173")]
    #[tokio::test]
    async fn a_keyed_failure_keeps_the_block_facts() {
        use hardy_bpv7::{
            bpsec::{
                key::{Key, KeyAlgorithm, KeySet, Operation, Type},
                signer::{Context, Signer},
            },
            builder::Builder,
            creation_timestamp::CreationTimestamp,
        };
        use rand::{TryRng, rngs::SysRng};

        // Immaterial key value: generated, and bound once for the signer and
        // the verifying key source.
        let mut k = vec![0u8; 32];
        SysRng.try_fill_bytes(&mut k).unwrap();
        let key = Key {
            key_type: Type::octet_sequence(k),
            key_algorithm: Some(KeyAlgorithm::HS256),
            enc_algorithm: None,
            operations: Some([Operation::Sign, Operation::Verify].into_iter().collect()),
            id: None,
            key_use: None,
        };

        // Block type 777 is signed and carries no flags; block type 999 asks
        // for a report on failure.
        let (_, data) = Builder::new("ipn:1.2".parse().unwrap(), "ipn:2.1".parse().unwrap())
            .add_extension_block(block::Type::Unrecognised(777))
            .unwrap()
            .build(b"signed".as_slice().into())
            .add_extension_block(block::Type::Unrecognised(999))
            .unwrap()
            .with_flags(block::Flags {
                report_on_failure: true,
                ..Default::default()
            })
            .build(b"unknown".as_slice().into())
            .with_payload(b"payload".as_slice().into())
            .build(CreationTimestamp::now())
            .unwrap();
        let built = parse::parse(Bytes::from(data)).unwrap();
        let signed_block = *built
            .bundle
            .blocks
            .iter()
            .find(|(_, b)| matches!(b.block_type, block::Type::Unrecognised(777)))
            .expect("the 777 block is present")
            .0;
        let signed = Signer::new(&built.bundle, &built.data)
            .sign_block(
                signed_block,
                Context::HMAC_SHA2(Default::default()),
                "ipn:1.2".parse().unwrap(),
                &key,
            )
            .map_err(|(_, e)| e)
            .expect("sign the 777 block")
            .rebuild()
            .expect("rebuild the signed bundle");

        // Flip the signed block's first data byte.
        let at = parse::parse(Bytes::copy_from_slice(&signed))
            .unwrap()
            .bundle
            .blocks
            .get(&signed_block)
            .expect("the signed block is present")
            .payload_range()
            .start as usize;
        let mut tampered = signed.to_vec();
        tampered[at] ^= 0xFF;

        let (tx, mut rx) = hardy_async::channel::bounded(1);
        tx.send(Segment::Final(Bytes::from(tampered)))
            .await
            .expect("channel open");
        let keys = move |_: &Bpv7Bundle, _: &[u8]| -> Box<dyn bpsec::key::KeySource> {
            Box::new(KeySet::new(vec![key]))
        };
        let Err(HeaderFailure::Invalid {
            report: Some((_, reception, reason)),
            ..
        }) = parse_headers(&mut rx, 1 << 20, 0, keys).await
        else {
            panic!("a failed BIB must reject the bundle with a report");
        };
        assert_eq!(
            reception,
            ReceptionReport::Demanded(ReasonCode::BlockUnsupported)
        );
        assert_eq!(reason, ReasonCode::FailedSecurityOperation);
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

    // A small bundle the header pass completes from one segment, with the
    // structural bundle its id is read from.
    fn small_bundle() -> (Bpv7Bundle, Bytes) {
        let (bundle, data) = Builder::new("ipn:1.2".parse().unwrap(), "ipn:2.1".parse().unwrap())
            .with_payload(b"complete at head".as_slice().into())
            .build(CreationTimestamp::now())
            .unwrap();
        (bundle, Bytes::from(data))
    }

    // A stream carrying `segments`, whose producer has gone away once they
    // are drained.
    async fn stream_of(segments: Vec<Segment>) -> hardy_async::channel::Receiver<Segment> {
        let (tx, rx) = hardy_async::channel::bounded(segments.len());
        for segment in segments {
            tx.send(segment).await.expect("channel open");
        }
        rx
    }

    // A bundle complete on a non-final segment commits once `Final` follows,
    // past any empty `Next` padding.
    #[tokio::test]
    async fn complete_at_head_commits_at_final() {
        let (_, data) = small_bundle();
        let mut rx = stream_of(vec![
            Segment::Next(data.clone()),
            Segment::Next(Bytes::new()),
            Segment::Final(Bytes::new()),
        ])
        .await;

        let Ok((_, headers, tail, _)) = parse_headers(&mut rx, 1 << 20, 0, bpsec::no_keys).await
        else {
            panic!("a bundle confirmed by `Final` must pass the header pass");
        };
        assert_eq!(headers, data, "the whole bundle is resident");
        assert!(tail.is_none(), "nothing is left to drain");
    }

    // A producer that goes away after a complete bundle but before `Final`
    // has abandoned the transfer: cancelled, so the CLA refuses it and
    // withholds its acknowledgement.
    #[tokio::test]
    async fn complete_at_head_without_final_is_cancelled() {
        let (_, data) = small_bundle();
        let mut rx = stream_of(vec![Segment::Next(data)]).await;

        assert!(matches!(
            parse_headers(&mut rx, 1 << 20, 0, bpsec::no_keys).await,
            Err(HeaderFailure::Cancelled)
        ));
    }

    // Bytes after a complete bundle are trailing data: the bundle is
    // dropped and reported, as the payload drain treats bytes past the
    // outer break.
    #[tokio::test]
    async fn bytes_after_a_complete_bundle_are_invalid() {
        let (bundle, data) = small_bundle();
        let mut rx = stream_of(vec![
            Segment::Next(data),
            Segment::Final(Bytes::from_static(&[0x00])),
        ])
        .await;

        let Err(HeaderFailure::Invalid {
            report: Some((reported, _, reason)),
            ..
        }) = parse_headers(&mut rx, 1 << 20, 0, bpsec::no_keys).await
        else {
            panic!("trailing bytes must reject the bundle with a report");
        };
        assert_eq!(
            reported.primary.id, bundle.primary.id,
            "the report names the bundle"
        );
        assert_eq!(reason, ReasonCode::BlockUnintelligible);
    }

    // A small bundle carrying an unrecognised block flagged
    // `report_on_failure`: a reception report it demands (§5.6 Step 4).
    fn report_flagged_bundle() -> Bytes {
        let (_, data) = Builder::new("ipn:1.2".parse().unwrap(), "ipn:2.1".parse().unwrap())
            .add_extension_block(block::Type::Unrecognised(999))
            .unwrap()
            .with_flags(block::Flags {
                report_on_failure: true,
                ..Default::default()
            })
            .build(b"unknown".as_slice().into())
            .with_payload(b"complete at head".as_slice().into())
            .build(CreationTimestamp::now())
            .unwrap();
        Bytes::from(data)
    }

    // Trailing bytes reject the bundle before any keyed verification runs,
    // yet the report keeps the facts the bundle's own blocks establish.
    #[tokio::test]
    async fn bytes_after_a_complete_bundle_keep_the_block_facts() {
        let mut rx = stream_of(vec![
            Segment::Next(report_flagged_bundle()),
            Segment::Final(Bytes::from_static(&[0x00])),
        ])
        .await;

        let Err(HeaderFailure::Invalid {
            report: Some((_, reception, reason)),
            ..
        }) = parse_headers(&mut rx, 1 << 20, 0, bpsec::no_keys).await
        else {
            panic!("trailing bytes must reject the bundle with a report");
        };
        assert_eq!(
            reception,
            ReceptionReport::Demanded(ReasonCode::BlockUnsupported)
        );
        assert_eq!(reason, ReasonCode::BlockUnintelligible);
    }

    // A `Final` that ends the stream inside the payload block: the bundle is
    // reported from its parsed headers, with the facts its blocks establish,
    // as unintelligible.
    #[tokio::test]
    async fn a_short_final_is_reported_with_the_block_facts() {
        let data = report_flagged_bundle();
        // Short by four bytes: inside the CRC value, as the payload's CRC-32
        // trailer is `44 c0 c1 c2 c3 FF`.
        let mut rx = stream_of(vec![Segment::Final(data.slice(..data.len() - 4))]).await;

        let Err(HeaderFailure::Invalid {
            report: Some((_, reception, reason)),
            ..
        }) = parse_headers(&mut rx, 1 << 20, 0, bpsec::no_keys).await
        else {
            panic!("a short Final must reject the bundle with a report");
        };
        assert_eq!(
            reception,
            ReceptionReport::Demanded(ReasonCode::BlockUnsupported)
        );
        assert_eq!(reason, ReasonCode::BlockUnintelligible);
    }

    // A bundle whose 1000-byte payload counts up from 0, for the peek tests,
    // with the offset its payload's data starts at.
    fn peek_bundle() -> (Bytes, usize) {
        let payload: Vec<u8> = (0..1000_u32).map(|i| i as u8).collect();
        let (bundle, data) = Builder::new("ipn:1.2".parse().unwrap(), "ipn:2.1".parse().unwrap())
            .with_payload(payload.as_slice().into())
            .build(CreationTimestamp::now())
            .unwrap();
        let start = bundle.blocks[&1].payload_range().start as usize;
        (Bytes::from(data), start)
    }

    // `data` as a first segment ending where the payload's data starts, then
    // `size`-byte segments, the last of them `Final`.
    fn segmented(data: &Bytes, start: usize, size: usize) -> Vec<Segment> {
        let mut segments = vec![Segment::Next(data.slice(..start))];
        let mut at = start;
        while at < data.len() {
            let end = (at + size).min(data.len());
            segments.push(Segment::Next(data.slice(at..end)));
            at = end;
        }
        if let Some(Segment::Next(last)) = segments.pop() {
            segments.push(Segment::Final(last));
        }
        segments
    }

    // `segments` with the bytes of its `Final` moved to a `Next`, closed by an
    // empty `Final`.
    fn final_apart(mut segments: Vec<Segment>) -> Vec<Segment> {
        let Some(Segment::Final(last)) = segments.pop() else {
            unreachable!("segmented ends in a Final");
        };
        segments.push(Segment::Next(last));
        segments.push(Segment::Final(Bytes::new()));
        segments
    }

    // The segments a header pass left for the drain.
    async fn left(rx: &mut hardy_async::channel::Receiver<Segment>) -> usize {
        let mut n = 0;
        while rx.recv().await.is_ok() {
            n += 1;
        }
        n
    }

    // A declared peek pulls segments until the payload's first `peek` bytes
    // are resident, and no further: the rest is the drain's.
    #[tokio::test]
    async fn the_header_pass_holds_the_payload_peek() {
        let (data, start) = peek_bundle();
        let segments = segmented(&data, start, 100);
        let count = segments.len();
        let mut rx = stream_of(segments).await;

        let Ok((_, headers, tail, _)) = parse_headers(&mut rx, 1 << 20, 150, bpsec::no_keys).await
        else {
            panic!("a peeked bundle must pass the header pass");
        };
        assert!(tail.is_some(), "the payload is still arriving");
        assert_eq!(headers.len(), start + 200, "two 100-byte segments hold 150");
        assert_eq!(
            headers[..],
            data[..start + 200],
            "the held bytes are the bundle's"
        );
        // Pulled: the header segment and the two body segments the peek
        // needs.
        assert_eq!(
            left(&mut rx).await,
            count - 3,
            "no segment past the peek is pulled"
        );
    }

    // With no peek the header pass stops at the payload block's header.
    #[tokio::test]
    async fn no_peek_pulls_nothing_past_the_payload_header() {
        let (data, start) = peek_bundle();
        let segments = segmented(&data, start, 100);
        let count = segments.len();
        let mut rx = stream_of(segments).await;

        let Ok((_, headers, tail, _)) = parse_headers(&mut rx, 1 << 20, 0, bpsec::no_keys).await
        else {
            panic!("the header pass must pass");
        };
        assert!(tail.is_some());
        assert_eq!(headers.len(), start);
        assert_eq!(left(&mut rx).await, count - 1);
    }

    // A peek covering the whole payload holds its body and stops there: the
    // trailer is the drain's. The 1000-byte body is ten 100-byte segments, so
    // the last one held ends exactly at the body's end.
    #[tokio::test]
    async fn a_peek_covering_the_payload_holds_its_body() {
        let (data, start) = peek_bundle();
        let mut rx = stream_of(segmented(&data, start, 100)).await;

        let Ok((_, headers, tail, _)) = parse_headers(&mut rx, 1 << 20, 4096, bpsec::no_keys).await
        else {
            panic!("a peeked bundle must pass the header pass");
        };
        assert!(tail.is_some(), "the trailer is still to come");
        assert_eq!(headers[..], data[..start + 1000], "the body is resident");
    }

    // A segment that completes the peek and the bundle with it leaves the
    // whole bundle resident: the tail completes, and the bundle commits on
    // the stream's `Final`, as one that arrived whole does — whether the
    // completing segment is the `Final` itself or a `Next` an empty `Final`
    // follows.
    #[tokio::test]
    async fn a_peek_completing_the_bundle_holds_it_whole() {
        let (data, start) = peek_bundle();
        for (shape, segments) in [
            ("completing Final", segmented(&data, start, 300)),
            (
                "Next then empty Final",
                final_apart(segmented(&data, start, 300)),
            ),
        ] {
            let mut rx = stream_of(segments).await;

            let Ok((_, headers, tail, _)) =
                parse_headers(&mut rx, 1 << 20, 4096, bpsec::no_keys).await
            else {
                panic!("{shape}: a complete bundle confirmed by `Final` must pass");
            };
            assert!(tail.is_none(), "{shape}: nothing is left to drain");
            assert_eq!(headers, data, "{shape}");
            assert_eq!(
                left(&mut rx).await,
                0,
                "{shape}: the pass pulls the `Final`"
            );
        }
    }

    // A peeked bundle complete before `Final` whose producer goes away has
    // abandoned the transfer.
    #[tokio::test]
    async fn a_peek_completing_the_bundle_still_waits_for_final() {
        let (data, start) = peek_bundle();
        let mut rx = stream_of(vec![
            Segment::Next(data.slice(..start)),
            Segment::Next(data.slice(start..)),
        ])
        .await;

        assert!(matches!(
            parse_headers(&mut rx, 1 << 20, 4096, bpsec::no_keys).await,
            Err(HeaderFailure::Cancelled)
        ));
    }

    // A payload a BCB covers has no peek, so the header pass holds nothing
    // for it, however large the declared peek.
    #[cfg(feature = "rfc9173")]
    #[tokio::test]
    async fn a_bcb_covered_payload_holds_no_peek() {
        use hardy_bpv7::bpsec::{
            encryptor::{Context, Encryptor},
            key::{EncAlgorithm, Key, Operation, Type},
        };
        use rand::{TryRng, rngs::SysRng};

        // Immaterial key value: the pass runs with `no_keys`, so the key
        // only has to encrypt; generated per the no-literal-keys rule.
        let mut key_bytes = vec![0u8; 32];
        SysRng.try_fill_bytes(&mut key_bytes).unwrap();
        let key = Key {
            key_type: Type::octet_sequence(key_bytes),
            key_algorithm: None,
            enc_algorithm: Some(EncAlgorithm::A256GCM),
            operations: Some([Operation::Encrypt].into_iter().collect()),
            id: None,
            key_use: None,
        };
        let (built, data) = Builder::new("ipn:1.2".parse().unwrap(), "ipn:2.1".parse().unwrap())
            .with_payload(vec![0xA5_u8; 1000].as_slice().into())
            .build(CreationTimestamp::now())
            .unwrap();
        let encrypted = Bytes::from(
            Encryptor::new(&built, &data)
                .encrypt_block(
                    1,
                    Context::AES_GCM(Default::default()),
                    "ipn:1.2".parse().unwrap(),
                    &key,
                )
                .map_err(|(_, e)| e)
                .expect("encrypt the payload")
                .rebuild()
                .expect("rebuild the encrypted bundle"),
        );
        let start = parse::parse(encrypted.clone()).unwrap().bundle.blocks[&1]
            .payload_range()
            .start as usize;
        let segments = segmented(&encrypted, start, 100);
        let count = segments.len();
        let mut rx = stream_of(segments).await;

        let Ok((_, headers, tail, _)) = parse_headers(&mut rx, 1 << 20, 4096, bpsec::no_keys).await
        else {
            panic!("an encrypted payload must pass the header pass");
        };
        assert!(tail.is_some());
        assert_eq!(
            headers.len(),
            start,
            "nothing is held past the payload header"
        );
        assert_eq!(left(&mut rx).await, count - 1);
    }

    // `data` rebuilt as a fragment whose payload starts at `offset` of a
    // 4000-byte ADU.
    fn as_fragment(data: &Bytes, offset: u64) -> Bytes {
        use hardy_bpv7::{
            bundle::FragmentInfo,
            editor::{Chunk, Editor},
        };

        let parsed = parse::parse(data.clone()).unwrap();
        let chunks = Editor::new(&parsed.bundle, data)
            .with_fragment_info(Some(FragmentInfo {
                offset,
                total_adu_length: 4000,
            }))
            .map_err(|(_, e)| e)
            .expect("set the fragment info")
            .rebuild()
            .expect("rebuild the fragment");
        Bytes::from(Chunk::flatten(chunks, data).into_vec())
    }

    // A fragment past the payload's start has no peek, so the header pass
    // holds nothing for it; the first fragment's bytes are the payload's
    // prefix and are held.
    #[tokio::test]
    async fn only_a_first_fragment_holds_the_peek() {
        let (data, _) = peek_bundle();
        for (offset, holds) in [(0, true), (2000, false)] {
            let fragment = as_fragment(&data, offset);
            let start = parse::parse(fragment.clone()).unwrap().bundle.blocks[&1]
                .payload_range()
                .start as usize;
            let mut rx = stream_of(segmented(&fragment, start, 100)).await;

            let Ok((_, headers, tail, _)) =
                parse_headers(&mut rx, 1 << 20, 150, bpsec::no_keys).await
            else {
                panic!("a fragment must pass the header pass");
            };
            assert!(tail.is_some());
            assert_eq!(
                headers.len() > start,
                holds,
                "offset {offset}: the peek is held only for the first fragment"
            );
        }
    }

    // A `Final` inside the peek, the payload short, is reported as a short
    // `Final` after the header pass is.
    #[tokio::test]
    async fn a_final_inside_the_peek_is_reported() {
        let (data, start) = peek_bundle();
        let short = data.slice(..start + 500);
        let mut rx = stream_of(segmented(&short, start, 100)).await;

        let Err(HeaderFailure::Invalid {
            report: Some((_, _, reason)),
            ..
        }) = parse_headers(&mut rx, 1 << 20, 4096, bpsec::no_keys).await
        else {
            panic!("a short Final inside the peek must reject the bundle with a report");
        };
        assert_eq!(reason, ReasonCode::BlockUnintelligible);
    }

    // A trailer the tail rejects while the peek is held is reported at once,
    // before the stream's `Final`: a payload byte flipped fails the CRC when
    // the segment completing the body brings the trailer too.
    #[tokio::test]
    async fn a_crc_mismatch_inside_the_peek_is_reported() {
        let (data, start) = peek_bundle();
        let mut bad = data.to_vec();
        bad[start + 10] ^= 0x01;
        let bad = Bytes::from(bad);
        let mut rx = stream_of(final_apart(segmented(&bad, start, 300))).await;

        let Err(HeaderFailure::Invalid {
            report: Some((_, _, reason)),
            ..
        }) = parse_headers(&mut rx, 1 << 20, 4096, bpsec::no_keys).await
        else {
            panic!("a CRC mismatch inside the peek must reject the bundle with a report");
        };
        assert_eq!(reason, ReasonCode::BlockUnintelligible);
        assert_eq!(left(&mut rx).await, 1, "the empty `Final` is left unpulled");
    }

    // A bundle declaring more than the size bound is refused before the
    // peek pulls a byte of its payload.
    #[tokio::test]
    async fn an_oversize_declaration_refuses_before_the_peek() {
        let (data, start) = peek_bundle();
        let segments = segmented(&data, start, 100);
        let count = segments.len();
        let mut rx = stream_of(segments).await;
        let bound = (start + 150) as u64;

        assert!(matches!(
            parse_headers(&mut rx, bound, 4096, bpsec::no_keys).await,
            Err(HeaderFailure::TooLarge { size, max }) if size == data.len() as u64 && max == bound
        ));
        assert_eq!(
            left(&mut rx).await,
            count - 1,
            "nothing is pulled past the payload header"
        );
    }

    // A producer that goes away while the peek is held has abandoned the
    // transfer.
    #[tokio::test]
    async fn a_stream_cancelled_inside_the_peek_is_cancelled() {
        let (data, start) = peek_bundle();
        let mut rx = stream_of(vec![
            Segment::Next(data.slice(..start)),
            Segment::Next(data.slice(start..start + 100)),
        ])
        .await;

        assert!(matches!(
            parse_headers(&mut rx, 1 << 20, 4096, bpsec::no_keys).await,
            Err(HeaderFailure::Cancelled)
        ));
    }

    // The caller's size bound applies to the declared whole-bundle size
    // the moment the header chain parses, before a single payload byte is
    // drained — and it is carried in u64 end to end, so a declaration
    // beyond a 32-bit address space still refuses cleanly.
    #[tokio::test]
    async fn declared_oversize_refuses_at_the_header_pass() {
        use hardy_bpv7::{bpsec::no_keys, builder::Builder, creation_timestamp::CreationTimestamp};

        let (built, data) =
            Builder::new("ipn:0.2.1".parse().unwrap(), "ipn:0.3.99".parse().unwrap())
                .with_payload(vec![0x5A_u8; 50_000].as_slice().into())
                .build(CreationTimestamp::now())
                .unwrap();
        let declared = built.encoded_len();

        // Small segments keep the *accumulated* bytes under the bound while
        // the header chain parses, so the declaration is what trips it: the
        // reported size is the full declared wire size, not a byte count.
        let (tx, mut rx) = hardy_async::channel::bounded(2);
        for chunk in data.chunks(200).take(2) {
            tx.send(Segment::Next(Bytes::copy_from_slice(chunk)))
                .await
                .expect("channel open");
        }
        match parse_headers(&mut rx, 1000, 0, no_keys).await {
            Err(HeaderFailure::TooLarge { size, max }) => {
                assert_eq!(size, declared, "the declared wire size is reported");
                assert_eq!(max, 1000);
            }
            Ok(_) => panic!("an over-declared bundle must refuse"),
            Err(_) => panic!("expected TooLarge"),
        }
    }
}
