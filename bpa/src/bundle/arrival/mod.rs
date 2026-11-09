//! A bundle's arrival at the BPA's doors: the keyed header pass, composing
//! the [`hardy_bpv7::bpsec::checks`] check over the structural parse, and
//! returning the `Bundle` together with the per-hop fields the BPA records
//! in metadata; the fate ([`fate`]), the reception facts
//! ([`report`]) and the payload's stages ([`payload`]) beside it. It does
//! not canonicalise: non-canonical CBOR is rejected at parse (RFC 9171
//! §4.1), and rewriting it is a configurable mutating-filter concern (see
//! `docs/streaming_pipeline_design.md` §5.2.2), not parser work.
//!
//! [`parse_headers`] is the streaming header pass of both raw-bundle doors,
//! which the gate can early-reject on before the payload is spooled, in the
//! [`Pass`] its door validates the bundle in. As a receiver it classifies and
//! *schedules* the removals — the `delete_block_on_failure`-flagged unknowns
//! and the §5.1.1 failure-drops ([`Headers::to_remove`]) — and, in the
//! same keyed pass, begins incremental verification of the BIB targets
//! deferred to the not-yet-resident payload
//! ([`Headers::deferred_verifiers`], through bpv7's
//! [`bib::Operation::begin_verify`](hardy_bpv7::bpsec::bib::Operation::begin_verify));
//! the dispatcher's payload drain feeds those as the payload streams. CLA ingress applies the
//! removals to the headers once its gate admits the bundle, before the bundle
//! is first stored, with the BPSec cascade for a BCB-covered BIB whose target
//! list shrinks; the bundle is otherwise stored as received. As a source,
//! for the raw service door, it schedules none: an originated bundle leaves
//! as its service built it, or is refused.
//! Used by `dispatcher::ingress`; on a keyed failure returns the recoverable
//! bundle so the caller can emit a status report.

use core::{fmt, mem::take};

use bytes::{Bytes, BytesMut};
use hardy_bpv7::{
    Bundle as Bpv7Bundle, block,
    bpsec::{
        self, Role, Roles, SecurityService, SecurityServices,
        checks::{self, Checked},
    },
    eid::Eid,
    parse::{self, PerHopFields},
    status_report::ReasonCode,
};
use time::OffsetDateTime;
use tracing::debug;

use self::{
    fate::{Deletion, Findings, decide_fate},
    payload::begin_payload_verification,
    report::ReportFacts,
};
use super::expiry;
use crate::{HashMap, HashSet, cla::Segment, keys::KeyProvider, stream::Receiver};

pub(crate) mod fate;
pub(crate) mod payload;
pub(crate) mod report;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

// Whether `target` is a block of `block_type`: compared by encoding, so an
// alias of the type counts.
fn has_type(blocks: &HashMap<u64, block::Block>, target: u64, block_type: block::Type) -> bool {
    blocks
        .get(&target)
        .is_some_and(|b| b.block_type == block_type)
}

/// Extract the well-known extension-block fields from freshly-built `Builder`
/// output — a structural `Bundle` plus its wire bytes — so any PreviousNode /
/// BundleAge / HopCount the builder emitted reaches the bundle's metadata. Used
/// by the locally-originated paths (`dispatcher::originate`,
/// `dispatcher::report`) that build a bundle and immediately wrap it;
/// [`parse_headers`] would do this same extraction after BPSec validation a
/// freshly-built bundle doesn't need.
pub fn extract_from_built(
    bundle: &Bpv7Bundle,
    data: &[u8],
) -> Result<PerHopFields, hardy_bpv7::Error> {
    PerHopFields::from_blocks(&bundle.blocks, data)
}

// The report for a header-pass rejection that precedes the keyed header
// verification: the parsed bundle, the reception facts its own blocks
// establish (the §A classification's `report_on_failure` flags), and the
// deletion reason `error` maps to.
fn reported_drop(
    parsed: parse::Parsed,
    error: &hardy_bpv7::Error,
) -> (Bpv7Bundle, ReportFacts, ReasonCode) {
    let facts = ReportFacts::unrecognised(&parsed.classification);
    (parsed.bundle, facts, error.into())
}

// ---------------------------------------------------------------------------
// Streaming ingress — the pre-drain header pass. The gate early-rejects on it
// before the payload is spooled; the dispatcher's drain then streams and
// verifies the payload.
// ---------------------------------------------------------------------------

/// Result of the pre-drain header pass: everything the streaming gate needs to
/// decide whether to drain, plus the inputs the dispatcher's payload drain
/// needs to finish once the payload streams.
pub struct Headers {
    /// The parse the pass verified: the bundle, with the BIB coverage the
    /// pass established; the resident bytes (`data`: the whole bundle, or
    /// the headers and whatever of the payload has arrived, at least the
    /// peek the pass held); its BCB and BIB operation sets; its per-hop
    /// fields, those the pass decrypted decoded from their plaintext; and
    /// its plaintext cache, what the pass decrypted (each BCB-covered per-hop
    /// block it could, and the encrypted BIBs when it must accept an
    /// operation one may carry) and the blocks whose ciphertext failed to
    /// authenticate, which the Ingress chain's reader is seeded from and
    /// hands back to, so no block is decrypted twice. Every
    /// BIB operation set is the gate's to accept from, those over the
    /// payload included: those over a payload not yet resident, which
    /// `deferred_verifiers` check as received, and those over an encrypted
    /// payload, unchecked, since RFC 9172 §3.9 forbids checking them
    /// against ciphertext and only the payload's acceptor, which decrypts
    /// it, checks them.
    pub parsed: parse::Parsed,
    /// Unrecognised / unsupported blocks scheduled for removal, by a
    /// receiver's pass: CLA ingress drops them from the headers before the
    /// bundle is first stored. A source's pass schedules none.
    pub to_remove: HashSet<u64>,
    /// The per-hop blocks whose security operations a receiver's pass
    /// accepted, both services, each one decrypted or verified: CLA ingress
    /// writes them in plaintext, without those operations, before the
    /// bundle is first stored. A source's pass accepts none.
    pub accepted: HashMap<u64, SecurityServices>,
    /// The Hop Count the pass leaves as received, and why: it travels
    /// unchanged where this node forwards the bundle, the increment being a
    /// SHOULD (RFC 9171 §4.4.3), and is logged there. The gate may yet accept
    /// it: as the bundle's destination, which fails the acceptance of one it
    /// cannot accept, and, for one that shares an encrypted BIB, with that
    /// BIB's other targets where this node accepts them all.
    pub hop_count_kept: Option<(u64, HopCountKept)>,
    /// The §5.6 reception-reporting facts (see [`reception_report_for`]).
    /// Carried on the reception assertion whether the bundle is accepted or
    /// rejected downstream — Step 4's facts precede either outcome — though
    /// a reject's deletion reason takes the report's one reason slot when
    /// both are asserted.
    pub report: ReportFacts,
    /// The targets of the BIB operations the pass checked and found good,
    /// for the gate's verification of the blocks the Ingress chain
    /// designates this node to verify ([`check_announced`]). The
    /// payload's, where it was not resident, is checked by
    /// `deferred_verifiers` instead.
    pub verified: HashSet<u64>,
    /// One incremental verifier per BIB op-set `Parsed::check` left targeting
    /// the not-yet-resident payload (block 1), each paired with its BIB's
    /// block number for failure attribution. Begun by the header pass, each
    /// owning the keyed digest state it needs, so the dispatcher's payload
    /// drain feeds and settles them without the key source. Empty when the
    /// payload was resident. A block-1 *BCB* (payload confidentiality) needs
    /// no deferral — it's decrypted at delivery via
    /// [`hardy_bpv7::bpsec::DecryptingReader`].
    pub deferred_verifiers: Vec<(u64, bpsec::bib::Verifier)>,
    /// The arrival's one key source, derived by the header pass, which every
    /// later stage that needs keys uses instead of asking the key provider
    /// again.
    pub keys: Box<dyn bpsec::key::KeySource>,
}

/// Why a pass leaves a Hop Count block as received
/// ([`Headers::hop_count_kept`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HopCountKept {
    /// An operation on it is in a security context this node does not
    /// support.
    Unsupported {
        /// The security block whose context is unsupported.
        security_block: u64,
    },
    /// This node holds no key for its BCB, so cannot read it.
    NoKey {
        /// The BCB's block number.
        bcb: u64,
        /// The BCB's security source.
        source: Eid,
    },
    /// This node holds no key for the BIB over it, so cannot check it.
    NoKeyForBib {
        /// The BIB's block number.
        bib: u64,
        /// The BIB's security source.
        source: Eid,
    },
    /// An encrypted BIB this node cannot read may cover it.
    PossiblyCovered,
    /// The encrypted BIB over it also covers another block, whose BCB
    /// operation goes with the Hop Count's, all or none (RFC 9172 §3.9): it
    /// stays as received unless this node accepts every block that BIB
    /// covers.
    SharesEncryptedBib {
        /// The encrypted BIB's block number.
        bib: u64,
    },
}

impl fmt::Display for HopCountKept {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported { security_block } => {
                write!(
                    f,
                    "block {security_block}'s security context is unsupported"
                )
            }
            Self::NoKey { bcb, source } => {
                write!(
                    f,
                    "no key for BCB block {bcb} from security source {source}"
                )
            }
            Self::NoKeyForBib { bib, source } => {
                write!(
                    f,
                    "no key for BIB block {bib} from security source {source}"
                )
            }
            Self::PossiblyCovered => {
                f.write_str("an encrypted BIB this node cannot read may cover it")
            }
            Self::SharesEncryptedBib { bib } => write!(
                f,
                "its encrypted BIB, block {bib}, also covers a block this node does not accept"
            ),
        }
    }
}

impl Headers {
    /// Header-only early-reject reason, if any: the bundle is past its lifetime
    /// (the shared [`expiry`] rule, so the gate and the post-store expiry
    /// checkpoints agree), or a Hop Count block has reached its limit. Computed
    /// straight off the parsed primary + extracted extension fields, so the
    /// streaming gate can run it before the payload is drained.
    pub fn gate_reason(&self, received_at: OffsetDateTime) -> Option<ReasonCode> {
        if expiry(
            &self.parsed.bundle.primary,
            self.parsed.per_hop.bundle_age,
            received_at,
        ) <= OffsetDateTime::now_utc()
        {
            Some(ReasonCode::LifetimeExpired)
        } else if self
            .parsed
            .per_hop
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

// The key source for one keyed pass. A bundle with no BCB or BIB consults no
// keys, so it skips the provider round-trip, as the filter engine's passes
// do.
fn keys_for(
    bcb_ops: &HashMap<u64, bpsec::bcb::OperationSet>,
    bib_ops: &HashMap<u64, bpsec::bib::OperationSet>,
    key_provider: &dyn KeyProvider,
    bundle: &Bpv7Bundle,
    data: &[u8],
) -> Box<dyn bpsec::key::KeySource> {
    if bcb_ops.is_empty() && bib_ops.is_empty() {
        Box::new(bpsec::key::KeySet::EMPTY)
    } else {
        key_provider.key_source(bundle, data)
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
        report: Option<(Bpv7Bundle, ReportFacts, ReasonCode)>,
    },
    /// [`Pass::Source`] only: the bundle is valid but breaks a rule a bundle
    /// this node originates must keep.
    SourceRule(SourceRule),
}

/// A rule a bundle this node originates must keep beyond its validity, so
/// that this node's egress can forward it as its service built it.
#[derive(Debug, PartialEq, Eq)]
pub enum SourceRule {
    /// It carries a Previous Node block: an originated bundle has no previous
    /// node (RFC 9171 §4.4.1), and this node's egress inserts its own.
    PreviousNode,
    /// A BIB or BCB targets the per-hop block numbered here: this node's
    /// egress rewrites the block at once, so the operation could never reach
    /// its acceptor.
    ProtectedPerHopBlock(u64),
}

/// What this node's header pass validates a bundle as: its receiver or its
/// source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pass {
    /// A bundle received from a peer, or reassembled: reception processing
    /// (RFC 9171 §5.6). An unprocessable block's flags decide its fate, a
    /// block failing its security check is discarded unless its flags say
    /// delete the bundle, and the removals are scheduled for CLA ingress.
    Receiver,
    /// A bundle this node's service built, which it originates: the header
    /// pass validates it as its source. It keeps the [`SourceRule`]s, so it
    /// runs none of the next hop's processing; an unprocessable block is
    /// carried whatever its flags, which are for the nodes that receive it;
    /// and any failed security check refuses it, as nothing is scheduled.
    Source,
}

/// Drive the structural parser off the segment stream up to the parsed header
/// chain (*without* draining a payload still to arrive), then run the keyed header
/// verification against the resident bytes — the streaming gate's whole
/// pre-drain stage in one call. The header verification begins incremental
/// verification of the payload-block BIBs ([`Headers::deferred_verifiers`])
/// for the dispatcher's streaming payload drain to feed and settle.
///
/// Before verifying, the pass holds the payload peek: it pulls segments until
/// the payload's first `peek` bytes (all of it, if shorter) are resident, each
/// fed through the tail, so the gate's chain can read them. A `peek` of 0, or
/// a payload with no peekable prefix ([`peekable_payload`]: one a BCB covers,
/// or a fragment past the payload's start), pulls nothing past the payload
/// block's header.
///
/// `Ok` is the verified headers, holding the parse whole (the resident
/// bytes among it: the whole bundle when it completed in this pass, else
/// the headers and whatever of the payload block has arrived), and the
/// payload `tail` the caller drains — the drain continues the byte count
/// this pass starts against `max_size`, which here bounds hostile unbounded
/// header chains and, once the chain parses, refuses a declared
/// whole-bundle size over the bound before a single payload byte drains.
/// `Err` is a [`HeaderFailure`]; see its variants for who handles what.
/// `pass` is what it validates the bundle as: see [`Pass`].
#[allow(clippy::result_large_err)]
pub async fn parse_headers(
    stream: &mut dyn Receiver<Segment>,
    pass: Pass,
    max_size: u64,
    peek: usize,
    key_provider: &dyn KeyProvider,
) -> Result<(Headers, Option<parse::PayloadTail>), HeaderFailure> {
    let mut parser = parse::BundleParser::default();
    // Drive the parser up to the header chain. The parse's `data` is the
    // resident bytes (the whole bundle, or the `consumed` prefix of one
    // still arriving); `tail` (if any) drains the rest back in
    // `dispatcher::ingress`.
    let mut arrival = Arrival {
        stream,
        total: 0,
        max: max_size,
    };
    let (parsed, tail) = loop {
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
            Ok(parse::ParserProgress::Ready(whole)) => match parser.finish(whole) {
                Ok(parsed) => {
                    break (await_final(&mut *arrival.stream, parsed, last).await?, None);
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
                match parser.finish(consumed) {
                    Ok(parsed) => break (parsed, Some(tail)),
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
    let (parsed, tail) = match tail {
        Some(tail) => hold_peek(&mut arrival, parsed, tail, peek).await?,
        None => (parsed, None),
    };

    // A source's rules come before the keyed work, so that a bundle this
    // node originates gets none of the next hop's processing.
    if pass == Pass::Source {
        check_source_rules(&parsed.bundle).map_err(HeaderFailure::SourceRule)?;
    }

    // Header verification (§A–§D) against the resident bytes. On a keyed failure
    // the recoverable `bundle` is returned so the caller can report the drop;
    // on success the parse moves whole into the returned `Headers`.
    let key_source = keys_for(
        &parsed.bcbs,
        &parsed.bibs,
        key_provider,
        &parsed.bundle,
        &parsed.data,
    );
    match verify_headers(parsed, pass, key_source) {
        Ok(hv) => Ok((hv, tail)),
        Err((bundle, error, facts)) => {
            debug!("Invalid bundle received: {error}");
            let reason = ReasonCode::from(&error);
            Err(HeaderFailure::Invalid {
                error,
                report: Some((bundle, facts, reason)),
            })
        }
    }
}

// The rules a bundle this node originates keeps (see [`SourceRule`]): no
// Previous Node block, and no BIB or BCB on a Bundle Age or Hop Count block.
// `BibCoverage::Maybe` marks only BCB-covered blocks, so `bcb` covers it.
fn check_source_rules(bundle: &Bpv7Bundle) -> Result<(), SourceRule> {
    for (&number, block) in &bundle.blocks {
        // Compared by encoding, so an alias of a type counts.
        if block.block_type == block::Type::PreviousNode {
            return Err(SourceRule::PreviousNode);
        }
        if (block.block_type == block::Type::BundleAge || block.block_type == block::Type::HopCount)
            && (block.bcb.is_some() || block.bib != block::BibCoverage::None)
        {
            return Err(SourceRule::ProtectedPerHopBlock(number));
        }
    }
    Ok(())
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

// The header pass's result before keyed verification: the parse, its `data`
// the resident bytes, and the payload tail still to drain, if any.
type Resident = (parse::Parsed, Option<parse::PayloadTail>);

// Holds the payload peek: pulls segments until the payload's first `peek`
// bytes (all of it, if shorter) are resident, and returns them with the
// parse's `data` as the resident bytes. Each pulled segment is fed to `tail`,
// which runs on it the checks the drain would run on the same bytes, so a
// trailer failure here is reported at once. A tail that completes leaves the
// whole bundle resident, and it commits on the stream's `Final` as one that
// arrived whole does.
#[allow(clippy::result_large_err)]
async fn hold_peek(
    arrival: &mut Arrival<'_>,
    mut parsed: parse::Parsed,
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
    if parsed.data.len() as u64 >= held_to {
        return Ok((parsed, Some(tail)));
    }
    // The resident bytes are uniquely held when the parser owned the buffer,
    // so the hold extends them in place; when one segment held the whole
    // header region, the producer may still share it, and the hold copies.
    let mut held = take(&mut parsed.data)
        .try_into_mut()
        .unwrap_or_else(|consumed| BytesMut::from(&consumed[..]));
    loop {
        let (bytes, last) = arrival.next().await?;
        held.extend_from_slice(&bytes);
        match tail.push(&bytes) {
            Ok(true) => {
                parsed.data = held.freeze();
                let parsed = await_final(&mut *arrival.stream, parsed, last).await?;
                return Ok((parsed, None));
            }
            Ok(false) if last => {
                parsed.data = held.freeze();
                return Err(tail_rejected_at_final(tail, parsed));
            }
            Ok(false) if held.len() as u64 >= held_to => {
                parsed.data = held.freeze();
                return Ok((parsed, Some(tail)));
            }
            Ok(false) => {}
            Err(error) => {
                debug!("Payload block rejected while holding the payload peek: {error}");
                parsed.data = held.freeze();
                let report = Some(reported_drop(parsed, &error));
                return Err(HeaderFailure::Invalid { error, report });
            }
        }
    }
}

/// Header verification (§A classify → §B/§C8/§C7 verify → §D extract) against the
/// parse's resident bytes — the `consumed` prefix of a bundle still
/// arriving, or the whole bundle otherwise. Takes the parse by value and
/// returns it inside the assembled [`Headers`] (BIB coverage stamps
/// applied, and one begun incremental verifier per block-1 (payload) op-set
/// the keyed verify deferred, for the dispatcher's payload drain to feed as
/// the payload streams, and the §E removals scheduled for CLA ingress's
/// rewrite). On a keyed failure the recoverable bundle rides the error, with
/// the reception facts established before it.
#[allow(clippy::result_large_err)]
fn verify_headers(
    parsed: parse::Parsed,
    pass: Pass,
    key_source: Box<dyn bpsec::key::KeySource>,
) -> Result<Headers, (Bpv7Bundle, hardy_bpv7::Error, ReportFacts)> {
    // Assembled up front with empty facts; the verification closure fills
    // them in place (keeping `?` ergonomics — its borrow ends at the call),
    // and the recoverable bundle rides whichever arm results.
    let mut hv = Headers {
        parsed,
        to_remove: HashSet::new(),
        accepted: HashMap::new(),
        hop_count_kept: None,
        report: ReportFacts::default(),
        verified: HashSet::new(),
        deferred_verifiers: Vec::new(),
        keys: key_source,
    };

    let verified = (|hv: &mut Headers| {
        // §A — the parser's classification; collect deletables. The report_*
        // facts stand from here, so every rejection below — a block demanding
        // the bundle's deletion (RFC 9171 §5.6 Step 4 reports, then deletes)
        // or a keyed failure — still honours a block's report demand.
        let classification = &hv.parsed.classification;
        let unrecognised = ReportFacts::unrecognised(classification);
        hv.report = unrecognised;
        // Only an unrecognised block (A1) is every receiver's to act on: an
        // unsupported security operation's flags act where this node accepts
        // the operation, below for the per-hop blocks and at the gate for the
        // rest, and elsewhere it travels untouched. A source carries an
        // unprocessable block whatever its flags, which are for the nodes
        // that receive it.
        if pass == Pass::Receiver {
            if let Some(hardy_bpv7::Error::Unsupported(block)) = classification.delete_bundle {
                return Err(hardy_bpv7::Error::Unsupported(block));
            }
            hv.to_remove
                .extend(classification.unrecognised_deletable.iter().copied());
        }

        // The per-hop blocks this node accepts (RFC 9171 §5.4): every
        // Previous Node and Bundle Age block, which it replaces, and a Hop
        // Count whose BCB operation it can process, the increment being a
        // SHOULD (§4.4.3). A Hop Count it cannot read, for want of a key or
        // under an operation in a security context it does not support, is
        // kept as received, with the reason.
        let mut acceptors: HashSet<u64> = HashSet::new();
        for (&number, block) in &hv.parsed.bundle.blocks {
            // Compared by encoding, so an alias of a type counts.
            if block.block_type == block::Type::PreviousNode
                || block.block_type == block::Type::BundleAge
            {
                acceptors.insert(number);
            } else if block.block_type == block::Type::HopCount {
                let Some(bcb) = block.bcb else {
                    acceptors.insert(number);
                    continue;
                };
                let ops = hv.parsed.bcbs.get(&bcb).ok_or(hardy_bpv7::Error::Altered)?;
                let op = ops
                    .operations()
                    .get(&number)
                    .ok_or(hardy_bpv7::Error::Altered)?;
                if op.is_unsupported() {
                    hv.hop_count_kept = Some((
                        number,
                        HopCountKept::Unsupported {
                            security_block: bcb,
                        },
                    ));
                    continue;
                }
                match op.require_key(&*hv.keys, ops.source()) {
                    Ok(()) => {
                        acceptors.insert(number);
                    }
                    Err(bpsec::Error::NoKey) => {
                        hv.hop_count_kept = Some((
                            number,
                            HopCountKept::NoKey {
                                bcb,
                                source: ops.source().clone(),
                            },
                        ));
                    }
                    Err(e) => return Err(e.into()),
                }
            }
        }

        // The acceptor's processing of those blocks and the verifier's
        // integrity checks over everything readable, one pass, one outcome
        // per operation met; the fate below is decided for what fails.
        // What is decrypted, and what failed to, seeds the arrival's
        // plaintext cache; the BIB operation sets stay whole. A Hop Count
        // whose acceptance cannot be completed here, coupled through an
        // encrypted BIB to a block this node does not accept, the check
        // reads all the same, as a verifier reads it, so the hop limit
        // holds; the gate may accept it with the rest.
        let roles: HashMap<u64, Roles> = acceptors
            .iter()
            .map(|&n| (n, Roles::each(Role::Acceptor, SecurityServices::BOTH)))
            .collect();
        let facts = hv.parsed.check(
            &*hv.keys,
            &roles,
            Roles::each(Role::Verifier, SecurityServices::INTEGRITY),
        )?;

        // A failure's fate (RFC 9172 §5.1.1, §5.1.2). A security operation
        // that fails leaves its target unprocessable: a BCB target whose
        // ciphertext did not authenticate, or the target of a BIB operation
        // whose integrity check failed (`Checked::Failed`), whether this node
        // is the operation's acceptor or only a verifier.
        // A target the bundle cannot lose (the primary block, the payload,
        // the Bundle Age of a bundle without a creation time) deletes the
        // bundle. Any other target deletes the bundle when its
        // "delete bundle if block can't be processed" flag says so, and
        // otherwise is discarded with its security operations, never kept: it
        // is queued in `to_remove`, and the editor cascade strips it from the
        // operation sets that name it, dropping a security block only once it
        // empties. A shared BCB with a surviving co-target must stay, since
        // naming the BCB itself would strand its ciphertext
        // (`StrandsCiphertext`), which the output doors' removal cascade
        // refuses on every attempt. A source's every failure is fatal: the
        // service's bundle fails its own check, and is refused rather than
        // altered.
        //
        // This node is the acceptor of every operation on a Previous Node or
        // Bundle Age block, which it must replace (RFC 9171 §5.4). A Previous
        // Node it cannot accept, for want of a key, a BCB's or a BIB's, for
        // an operation in a security context it does not support, or for an
        // encrypted BIB it cannot read that may cover the block, is a failed
        // acceptance: the block is one it cannot process, an RFC 9171 §5.6
        // Step 4 block ("Block unsupported"), whose flags demand a report
        // and decide between deleting the bundle and discarding the block,
        // never kept. A Bundle Age it cannot accept is no failure on a bundle
        // with a creation time, since this node writes the one it owes from
        // the clock: the received block is discarded, its flags unconsulted
        // and nothing reported, and egress writes a fresh one. The check
        // has already failed the bundle for the Bundle Age of a bundle
        // without a creation time. An integrity operation on either block inside an
        // encrypted BIB this node decrypted is one no valid bundle carries,
        // which it cannot accept: a conflicting security operation (RFC 9172
        // §7), whose target's flags decide as a failed acceptance's do, with
        // the report "Conflicting security operation". A Hop Count it holds
        // no key for is not its to accept, since the increment is a SHOULD
        // (RFC 9171 §4.4.3): the block travels unchanged, for a downstream
        // acceptor, as does any other block a BIB this node holds no key for
        // covers.
        let blocks = &hv.parsed.bundle.blocks;
        // A receiver replaces the Bundle Age it cannot accept from the clock;
        // a source alters nothing of its service's bundle.
        let clocked = hv.parsed.bundle.primary.id.timestamp.is_clocked();
        let (soft, unaccepted): (HashSet<u64>, HashSet<u64>) = facts
            .targets(Checked::NoKey)
            .chain(facts.targets(Checked::Unsupported))
            .chain(facts.targets(Checked::PossiblyConflicting))
            .filter(|&target| {
                has_type(blocks, target, block::Type::PreviousNode)
                    || has_type(blocks, target, block::Type::BundleAge)
            })
            .partition(|&target| {
                pass == Pass::Receiver
                    && clocked
                    && has_type(blocks, target, block::Type::BundleAge)
            });
        // The security block of an operation in an unsupported context on a
        // block whose acceptance failed is itself one this node cannot
        // process (RFC 9171 §5.6 Step 4).
        let unsupported: HashSet<u64> = facts
            .outcomes
            .iter()
            .filter(|o| o.checked == Checked::Unsupported && unaccepted.contains(&o.target))
            .map(|o| o.security_block)
            .collect();
        let findings = Findings {
            bcb_failed: facts
                .targets_of(SecurityService::Confidentiality, Checked::Failed)
                .collect(),
            bib_failed: facts
                .targets_of(SecurityService::Integrity, Checked::Failed)
                .collect(),
            conflicting: facts.targets(Checked::Conflicting).collect(),
            unaccepted,
            unsupported,
            soft,
        };
        let fate = match decide_fate(&hv.parsed.bundle, findings, pass == Pass::Source) {
            Ok(fate) => fate,
            Err(Deletion { error, report }) => {
                // A report demand stands before the deletion, as the
                // classification's do.
                hv.report = unrecognised | report;
                return Err(error);
            }
        };
        hv.to_remove.extend(fate.discarded.iter().copied());
        // An encrypted BIB over a Previous Node or Bundle Age block the fate
        // discards goes with it, whole: it carries an operation no valid
        // bundle does, and this node encrypts nothing again.
        hv.to_remove.extend(
            facts
                .outcomes
                .iter()
                .filter(|o| {
                    matches!(
                        o.checked,
                        Checked::Conflicting | Checked::PossiblyConflicting
                    ) && fate.discarded.contains(&o.target)
                })
                .map(|o| o.security_block),
        );
        // A target queued for failure-drop surfaces in the reception report.
        hv.report = unrecognised | fate.report;

        // A Hop Count with an operation in a security context this node does
        // not support is not its to accept, and one whose BCB it holds no key
        // for it cannot read, so enforces no hop limit on the bundle: either
        // is left as received.
        // A Hop Count under a BIB operation in a security context this node
        // does not support is not its to accept: it is left as received.
        for o in facts.outcomes.iter().filter(|o| {
            o.checked == Checked::Unsupported && has_type(blocks, o.target, block::Type::HopCount)
        }) {
            hv.hop_count_kept = Some((
                o.target,
                HopCountKept::Unsupported {
                    security_block: o.security_block,
                },
            ));
        }

        // §D — the per-hop fields, which the caller records in the bundle's
        // metadata: the parse decoded the plaintext blocks, and the blocks
        // decrypted here are decoded from the cache.
        decode_decrypted(
            &mut hv.parsed.per_hop,
            &hv.parsed.bundle.blocks,
            &hv.parsed.decrypted.plaintext,
        )?;

        // A per-hop block the fate discards is not read: this node cannot
        // trust its value.
        for &target in &fate.discarded {
            let Some(block) = hv.parsed.bundle.blocks.get(&target) else {
                continue;
            };
            // Compared by encoding, so an alias of the type counts.
            if block.block_type == block::Type::PreviousNode {
                hv.parsed.per_hop.previous_node = None;
            } else if block.block_type == block::Type::BundleAge {
                hv.parsed.per_hop.bundle_age = None;
            } else if block.block_type == block::Type::HopCount {
                hv.parsed.per_hop.hop_count = None;
            }
        }

        // Begin incremental verification of the payload's deferred
        // operations, for the drain to feed. A resident payload defers none,
        // and yields an empty vec.
        let deferred: HashSet<u64> = facts
            .outcomes
            .iter()
            .filter(|o| o.checked == Checked::Deferred && o.target == 1)
            .map(|o| o.security_block)
            .collect();
        hv.deferred_verifiers = match hv.parsed.bundle.blocks.get(&1) {
            // An encrypted payload's BIBs are its acceptor's to check.
            Some(payload) if payload.bcb.is_none() => begin_payload_verification(
                &hv.parsed.data,
                &*hv.keys,
                &hv.parsed.bundle.blocks,
                hv.parsed
                    .bibs
                    .iter()
                    .filter(|(bib, _)| deferred.contains(bib))
                    .map(|(&bib, ops)| (bib, ops)),
                payload.data.end - payload.data.start,
                false,
            )?,
            _ => Vec::new(),
        };

        if pass == Pass::Receiver {
            let (accepted, kept) = accepted_per_hop(
                &hv.parsed.bundle.blocks,
                &hv.to_remove,
                &hv.parsed.decrypted.plaintext,
                &hv.parsed.bibs,
                &facts,
            );
            hv.accepted = accepted
                .into_iter()
                .map(|n| (n, SecurityServices::BOTH))
                .collect();
            if kept.is_some() {
                hv.hop_count_kept = kept;
            }
        }

        // What the pass checked, for the gate's verification of the blocks
        // it is designated to verify.
        hv.verified = facts
            .targets_of(SecurityService::Integrity, Checked::Verified)
            .collect();

        Ok::<_, hardy_bpv7::Error>(())
    })(&mut hv);

    match verified {
        Ok(()) => Ok(hv),
        Err(e) => Err((hv.parsed.bundle, e, hv.report)),
    }
}

// The per-hop blocks a receiver accepts, every security operation on each
// decrypted or verified by its header pass: each Previous Node or Bundle Age
// block it keeps, since it must replace them (RFC 9171 §5.4), and a Hop
// Count it could read and check. The Hop Count's increment is a SHOULD
// (§4.4.3), so one this node cannot accept here is stored as received, and
// one it could read is returned beside the accepted set, kept, with the
// reason: one under a BIB it holds no key for, which travels with that BIB;
// one with an operation in a security context it does not support, which
// the caller records; and one it could check whose BCB operation would take
// others with it, one an encrypted BIB this node cannot read may cover, and
// one under an encrypted BIB that also covers another block, since removing
// the BCB operation on the Hop Count takes those on that BIB and its other
// targets with it, all or none (RFC 9172 §3.9), and no other block under an
// encrypted BIB is accepted here. The gate accepts the last with that BIB's
// other targets, where it accepts them all.
fn accepted_per_hop<V>(
    blocks: &HashMap<u64, block::Block>,
    to_remove: &HashSet<u64>,
    decrypted: &HashMap<u64, V>,
    bib_ops: &HashMap<u64, bpsec::bib::OperationSet>,
    facts: &checks::Facts,
) -> (HashSet<u64>, Option<(u64, HopCountKept)>) {
    let mut accepted = HashSet::new();
    let mut kept = None;
    for (&number, block) in blocks {
        let protected = block.bcb.is_some() || block.bib != block::BibCoverage::None;
        if !protected || to_remove.contains(&number) {
            continue;
        }
        // Compared by encoding, so an alias of a type counts.
        if block.block_type == block::Type::PreviousNode
            || block.block_type == block::Type::BundleAge
        {
            accepted.insert(number);
        } else if block.block_type == block::Type::HopCount {
            if block.bcb.is_some() && !decrypted.contains_key(&number) {
                continue;
            }
            let no_key_for_bib = facts
                .outcomes
                .iter()
                .find(|o| {
                    o.target == number
                        && o.service == SecurityService::Integrity
                        && o.checked == Checked::NoKey
                })
                .map(|o| o.security_block);
            if let Some(bib) = no_key_for_bib {
                if let Some(ops) = bib_ops.get(&bib) {
                    kept = Some((
                        number,
                        HopCountKept::NoKeyForBib {
                            bib,
                            source: ops.source().clone(),
                        },
                    ));
                }
                continue;
            }
            if facts
                .outcomes
                .iter()
                .any(|o| o.target == number && o.checked == Checked::Unsupported)
            {
                continue;
            }
            let coupled = match block.bib {
                block::BibCoverage::Maybe => Some(HopCountKept::PossiblyCovered),
                block::BibCoverage::Some(bib)
                    if blocks.get(&bib).is_some_and(|b| b.bcb.is_some())
                        && blocks.iter().any(|(&n, b)| {
                            n != number && b.bib == block::BibCoverage::Some(bib)
                        }) =>
                {
                    Some(HopCountKept::SharesEncryptedBib { bib })
                }
                _ => None,
            };
            match coupled {
                Some(reason) => kept = Some((number, reason)),
                None => {
                    accepted.insert(number);
                }
            }
        }
    }
    (accepted, kept)
}

// ---------------------------------------------------------------------------
// §D — the per-hop fields
// ---------------------------------------------------------------------------

// The per-hop fields once §C8 has decrypted: the parse decoded the blocks no
// BCB covers, and each this pass decrypted is decoded from its plaintext.
fn decode_decrypted<V: AsRef<[u8]>>(
    per_hop: &mut PerHopFields,
    blocks: &HashMap<u64, block::Block>,
    decrypted: &HashMap<u64, V>,
) -> Result<(), hardy_bpv7::Error> {
    for (number, plaintext) in decrypted {
        let block = blocks
            .get(number)
            .expect("a decrypted block is in the bundle");
        per_hop.decode(block.block_type, plaintext.as_ref())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "rfc9173")]
    use core::num::NonZeroU8;
    use core::sync::atomic::{AtomicUsize, Ordering};

    // `signer::Context` is named by its module: `Context` is the
    // encryptor's.
    #[cfg(feature = "rfc9173")]
    use hardy_bpv7::{
        bpsec::{
            encryptor::{Context, Encryptor},
            key::{EncAlgorithm, Key, KeyAlgorithm, KeySet, Operation, Type},
            signer::{self, Signer},
        },
        crc,
        hop_info::HopInfo,
    };
    use hardy_bpv7::{builder::Builder, creation_timestamp::CreationTimestamp, eid::Eid};
    use hardy_cbor::encode::{Encoder, emit};
    use hex_literal::hex;
    #[cfg(feature = "rfc9173")]
    use rand::{TryRng, rngs::SysRng};

    use super::*;
    use crate::{
        bundle::arrival::report::{ReceptionReport, reception_report_for},
        keys::{NullKeyProvider, tests::Keys},
    };

    // A peer's bundle whose Hop Count block is BCB-encrypted under a
    // generated key: (bytes, the Hop Count's block number, the key).
    #[cfg(feature = "rfc9173")]
    fn encrypted_hop_count_bundle() -> (Bytes, u64, Key) {
        // Immaterial key value: generated per the no-literal-keys rule.
        let mut key_bytes = vec![0u8; 32];
        SysRng.try_fill_bytes(&mut key_bytes).unwrap();
        let key = Key {
            key_type: Type::octet_sequence(key_bytes),
            key_algorithm: None,
            enc_algorithm: Some(EncAlgorithm::A256GCM),
            operations: Some(
                [Operation::Encrypt, Operation::Decrypt]
                    .into_iter()
                    .collect(),
            ),
            id: Some("ipn:2.1".into()),
            key_use: None,
        };

        let (built, data) =
            Builder::new("ipn:0.2.1".parse().unwrap(), "ipn:0.3.99".parse().unwrap())
                // The Hop Count carries no CRC, as RFC 9173 §4.8.1 leaves a
                // target, so that an alteration of its ciphertext reaches the
                // decryption rather than the CRC.
                .add_extension_block(block::Type::HopCount)
                .unwrap()
                .with_flags(block::Flags {
                    report_on_failure: true,
                    must_replicate: true,
                    ..Default::default()
                })
                .with_crc_type(crc::CrcType::None)
                .build(
                    emit(&HopInfo {
                        limit: NonZeroU8::new(64).unwrap(),
                        count: 1,
                    })
                    .0
                    .into(),
                )
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
                    &key,
                )
                .map_err(|(_, e)| e)
                .expect("encrypt the Hop Count block")
                .rebuild()
                .expect("rebuild the encrypted bundle"),
        );
        (encrypted, hop_block, key)
    }

    // The NoKey liveness policy through the keyed header pass: a
    // BCB-encrypted Hop Count this node has no key for passes unread. Its
    // increment is a SHOULD (RFC 9171 §4.4.3), so the node enforces no hop
    // limit and forwards the block intact.
    #[cfg(feature = "rfc9173")]
    #[tokio::test]
    async fn nokey_hop_count_passes_ingress_unread() {
        let (encrypted, hop_block, _) = encrypted_hop_count_bundle();

        let (tx, mut rx) = hardy_async::channel::bounded(1);
        tx.send(Segment::Final(encrypted.clone()))
            .await
            .expect("channel open");
        let Ok((hv, ..)) =
            parse_headers(&mut rx, Pass::Receiver, 1 << 20, 0, &NullKeyProvider).await
        else {
            panic!("an undecryptable Hop Count must pass the header pass");
        };
        assert!(hv.parsed.per_hop.hop_count.is_none(), "the count is unread");
        assert!(
            !hv.to_remove.contains(&hop_block),
            "the block is forwarded intact"
        );
        assert!(
            matches!(hv.hop_count_kept, Some((n, HopCountKept::NoKey { .. })) if n == hop_block),
            "the block is kept as received, for a forwarding node to log"
        );
    }

    // How `protected_per_hop_block` protects its block.
    #[cfg(feature = "rfc9173")]
    #[derive(Clone, Copy, Debug)]
    enum Protection {
        Signed,
        Encrypted,
    }

    // A clocked peer's bundle whose `block_type` block, a Previous Node,
    // Bundle Age or Hop Count carrying `flags`, is signed or encrypted under a
    // key no test supplies: the bytes and the block's number.
    #[cfg(feature = "rfc9173")]
    fn protected_per_hop_block(
        block_type: block::Type,
        flags: block::Flags,
        protection: Protection,
    ) -> (Bytes, u64) {
        let body = if block_type == block::Type::PreviousNode {
            hardy_cbor::encode::emit(&"ipn:0.4.0".parse::<Eid>().unwrap()).0
        } else if block_type == block::Type::BundleAge {
            hardy_cbor::encode::emit(&5_000u64).0
        } else {
            hardy_cbor::encode::emit(&HopInfo {
                limit: NonZeroU8::new(64).unwrap(),
                count: 1,
            })
            .0
        };
        let (built, data) =
            Builder::new("ipn:0.2.1".parse().unwrap(), "ipn:0.3.99".parse().unwrap())
                .add_extension_block(block_type)
                .unwrap()
                .with_flags(flags)
                .build(body.into())
                .with_payload(b"payload".as_slice().into())
                .build(CreationTimestamp::now())
                .unwrap();
        let number = *built
            .blocks
            .iter()
            .find(|(_, b)| b.block_type == block_type)
            .expect("the block is present")
            .0;

        // Immaterial key value: generated per the no-literal-keys rule.
        let mut k = vec![0u8; 32];
        SysRng.try_fill_bytes(&mut k).unwrap();
        let source: Eid = "ipn:0.2.1".parse().unwrap();
        let protected = match protection {
            Protection::Signed => Signer::new(&built, &data)
                .sign_block(
                    number,
                    signer::Context::HMAC_SHA2(Default::default()),
                    source,
                    &Key {
                        key_type: Type::octet_sequence(k),
                        key_algorithm: Some(KeyAlgorithm::HS256),
                        enc_algorithm: None,
                        operations: Some(
                            [Operation::Sign, Operation::Verify].into_iter().collect(),
                        ),
                        id: None,
                        key_use: None,
                    },
                )
                .map_err(|(_, e)| e)
                .expect("sign the block")
                .rebuild()
                .expect("rebuild the signed bundle"),
            Protection::Encrypted => Encryptor::new(&built, &data)
                .encrypt_block(
                    number,
                    Context::AES_GCM(Default::default()),
                    source,
                    &Key {
                        key_type: Type::octet_sequence(k),
                        key_algorithm: None,
                        enc_algorithm: Some(EncAlgorithm::A256GCM),
                        operations: Some(
                            [Operation::Encrypt, Operation::Decrypt]
                                .into_iter()
                                .collect(),
                        ),
                        id: None,
                        key_use: None,
                    },
                )
                .map_err(|(_, e)| e)
                .expect("encrypt the block")
                .rebuild()
                .expect("rebuild the encrypted bundle"),
        };
        (Bytes::from(protected), number)
    }

    // This node accepts every operation on a Previous Node or Bundle Age
    // block, so one it holds no key for is a failed acceptance: a block it
    // cannot process (RFC 9171 §5.6 Step 4), discarded unread when its flags
    // ask nothing more. A Hop Count it holds no key for is not its to accept:
    // it travels, read where it is plaintext.
    #[cfg(feature = "rfc9173")]
    #[tokio::test]
    async fn an_unaccepted_per_hop_block_is_discarded_unread() {
        for protection in [Protection::Signed, Protection::Encrypted] {
            for block_type in [
                block::Type::PreviousNode,
                block::Type::BundleAge,
                block::Type::HopCount,
            ] {
                let (data, number) =
                    protected_per_hop_block(block_type, block::Flags::default(), protection);
                let Ok(hv) = header_pass(data, Pass::Receiver, &NullKeyProvider).await else {
                    panic!("a {protection:?} {block_type:?} without its key keeps the bundle");
                };
                let accepted = block_type != block::Type::HopCount;
                assert_eq!(
                    hv.to_remove.contains(&number),
                    accepted,
                    "a {protection:?} {block_type:?} is discarded only where this node accepts it"
                );
                assert!(
                    hv.accepted.is_empty(),
                    "a {protection:?} {block_type:?}'s operations go unaccepted without its key"
                );
                assert_eq!(reception_report_for(hv.report), ReceptionReport::Requested);
                let read = if block_type == block::Type::PreviousNode {
                    hv.parsed.per_hop.previous_node.is_some()
                } else if block_type == block::Type::BundleAge {
                    hv.parsed.per_hop.bundle_age.is_some()
                } else {
                    hv.parsed.per_hop.hop_count.is_some()
                };
                assert_eq!(
                    read,
                    !accepted && matches!(protection, Protection::Signed),
                    "a {protection:?} {block_type:?}'s value is read only where it travels in plaintext"
                );
            }
        }
    }

    // A Hop Count under a BIB this node holds no key for is read, since it
    // is plaintext, but not accepted: it is kept as received with that BIB,
    // and the pass says why.
    #[cfg(feature = "rfc9173")]
    #[tokio::test]
    async fn a_hop_count_under_a_bib_without_its_key_is_kept() {
        let (data, number) = protected_per_hop_block(
            block::Type::HopCount,
            block::Flags::default(),
            Protection::Signed,
        );
        let Ok(hv) = header_pass(data, Pass::Receiver, &NullKeyProvider).await else {
            panic!("a signed Hop Count without its key keeps the bundle");
        };
        assert!(hv.accepted.is_empty() && hv.to_remove.is_empty());
        assert!(
            hv.parsed.per_hop.hop_count.is_some(),
            "the plaintext value is read"
        );
        let source: Eid = "ipn:0.2.1".parse().unwrap();
        assert!(
            matches!(
                &hv.hop_count_kept,
                Some((n, HopCountKept::NoKeyForBib { source: s, .. })) if *n == number && *s == source
            ),
            "{:?}",
            hv.hop_count_kept
        );
    }

    // A failed acceptance's block is one this node cannot process, so its
    // flags decide the rest as Step 4's do: one asking for a report demands
    // it, as "Block unsupported", and one flagged to delete the bundle
    // deletes it for that reason, after the report.
    #[cfg(feature = "rfc9173")]
    #[tokio::test]
    async fn an_unaccepted_previous_node_reports_and_deletes_as_flagged() {
        let report = block::Flags {
            report_on_failure: true,
            ..Default::default()
        };
        let delete = block::Flags {
            delete_bundle_on_failure: true,
            ..report.clone()
        };
        for protection in [Protection::Signed, Protection::Encrypted] {
            let (data, number) =
                protected_per_hop_block(block::Type::PreviousNode, report.clone(), protection);
            let Ok(hv) = header_pass(data, Pass::Receiver, &NullKeyProvider).await else {
                panic!("a {protection:?} Previous Node asking for a report keeps the bundle");
            };
            assert!(hv.to_remove.contains(&number));
            assert_eq!(
                reception_report_for(hv.report),
                ReceptionReport::Demanded(ReasonCode::BlockUnsupported)
            );

            let (data, _) =
                protected_per_hop_block(block::Type::PreviousNode, delete.clone(), protection);
            let Err(HeaderFailure::Invalid {
                report: Some((_, facts, reason)),
                ..
            }) = header_pass(data, Pass::Receiver, &NullKeyProvider).await
            else {
                panic!("a {protection:?} Previous Node flagged to delete the bundle deletes it");
            };
            assert_eq!(
                reception_report_for(facts),
                ReceptionReport::Demanded(ReasonCode::BlockUnsupported)
            );
            assert_eq!(reason, ReasonCode::BlockUnsupported);
        }
    }

    // A Bundle Age this node cannot accept is no failure on a bundle with a
    // creation time: this node writes the one it owes from the clock, so the
    // received block is discarded unread whatever its flags, which are not
    // consulted, nothing is reported, and the bundle is kept.
    #[cfg(feature = "rfc9173")]
    #[tokio::test]
    async fn a_clocked_bundle_age_this_node_cannot_accept_is_replaced_whatever_its_flags() {
        let report = block::Flags {
            report_on_failure: true,
            ..Default::default()
        };
        let delete = block::Flags {
            delete_bundle_on_failure: true,
            ..report.clone()
        };
        for protection in [Protection::Signed, Protection::Encrypted] {
            for flags in [report.clone(), delete.clone()] {
                let (data, number) =
                    protected_per_hop_block(block::Type::BundleAge, flags.clone(), protection);
                let Ok(hv) = header_pass(data, Pass::Receiver, &NullKeyProvider).await else {
                    panic!("{flags:?}: a {protection:?} Bundle Age keeps the bundle");
                };
                assert!(
                    hv.to_remove.contains(&number),
                    "{flags:?}: the {protection:?} Bundle Age is discarded"
                );
                assert_eq!(hv.parsed.per_hop.bundle_age, None, "{flags:?}: unread");
                assert_eq!(
                    reception_report_for(hv.report),
                    ReceptionReport::Requested,
                    "{flags:?}"
                );
            }
        }
    }

    // A keyed verification failure keeps the facts the §A classification
    // established: an unrecognised block flagged `report_on_failure` still
    // demands its reception report (RFC 9171 §5.6 Step 4) when a BIB over
    // another block, one flagged to delete the bundle, fails integrity, and
    // the deletion cites the failed security operation.
    #[cfg(feature = "rfc9173")]
    #[tokio::test]
    async fn a_keyed_failure_keeps_the_block_facts() {
        let (tampered, key, _) = tampered_signed_block(true, block::Type::HopCount);
        let (tx, mut rx) = hardy_async::channel::bounded(1);
        tx.send(Segment::Final(Bytes::from(tampered)))
            .await
            .expect("channel open");
        let keys = Keys(vec![key]);
        let Err(HeaderFailure::Invalid {
            report: Some((_, facts, reason)),
            ..
        }) = parse_headers(&mut rx, Pass::Receiver, 1 << 20, 0, &keys).await
        else {
            panic!("a failed BIB over a delete-flagged block must reject the bundle");
        };
        assert_eq!(
            reception_report_for(facts),
            ReceptionReport::Demanded(ReasonCode::BlockUnsupported)
        );
        assert_eq!(reason, ReasonCode::FailedSecurityOperation);
    }

    // The fate of a BIB operation whose integrity check fails over a
    // block with no "delete bundle" flag: the target is discarded with its
    // security operations, and the bundle survives, reporting the drop.
    #[cfg(feature = "rfc9173")]
    #[tokio::test]
    async fn a_failed_bib_discards_its_target() {
        let (tampered, key, signed_block) = tampered_signed_block(false, block::Type::HopCount);
        let (tx, mut rx) = hardy_async::channel::bounded(1);
        tx.send(Segment::Final(Bytes::from(tampered)))
            .await
            .expect("channel open");
        let keys = Keys(vec![key]);
        let Ok((hv, ..)) = parse_headers(&mut rx, Pass::Receiver, 1 << 20, 0, &keys).await else {
            panic!("a failed BIB over an unflagged block keeps the bundle");
        };
        assert!(
            hv.to_remove.contains(&signed_block),
            "the target is scheduled for removal"
        );
        assert_eq!(
            hv.parsed.per_hop.hop_count, None,
            "a discarded block's value is not read"
        );
        assert_eq!(
            reception_report_for(hv.report),
            ReceptionReport::Demanded(ReasonCode::FailedSecurityOperation)
        );
    }

    // The header pass over `data`, complete in one segment, as `pass`.
    #[allow(clippy::result_large_err)]
    async fn header_pass(
        data: Bytes,
        pass: Pass,
        keys: &dyn KeyProvider,
    ) -> Result<Headers, HeaderFailure> {
        let mut rx = stream_of(vec![Segment::Final(data)]).await;
        parse_headers(&mut rx, pass, 1 << 20, 0, keys)
            .await
            .map(|(hv, _)| hv)
    }

    // A bundle with an unrecognised block (type 999) carrying `flags`, and
    // that block's number.
    fn with_unrecognised_block(flags: block::Flags) -> (Bytes, u64) {
        let (bundle, data) = Builder::new("ipn:1.2".parse().unwrap(), "ipn:2.1".parse().unwrap())
            .add_extension_block(block::Type::Unrecognised(999))
            .unwrap()
            .with_flags(flags)
            .build(b"unknown".as_slice().into())
            .with_payload(b"payload".as_slice().into())
            .build(CreationTimestamp::now())
            .unwrap();
        let unknown = *bundle
            .blocks
            .iter()
            .find(|(_, b)| b.block_type == block::Type::Unrecognised(999))
            .expect("the unrecognised block is present")
            .0;
        (Bytes::from(data), unknown)
    }

    // A source carries an unprocessable block whatever its flags, which are
    // for the nodes that receive it: a receiver refuses the bundle a block
    // flagged to delete it, and schedules the removal of one flagged to
    // delete itself.
    #[tokio::test]
    async fn a_source_carries_an_unprocessable_block_whatever_its_flags() {
        let (delete_bundle, _) = with_unrecognised_block(block::Flags {
            delete_bundle_on_failure: true,
            ..Default::default()
        });
        assert!(matches!(
            header_pass(delete_bundle.clone(), Pass::Receiver, &NullKeyProvider).await,
            Err(HeaderFailure::Invalid { .. })
        ));
        let Ok(hv) = header_pass(delete_bundle, Pass::Source, &NullKeyProvider).await else {
            panic!("a source carries a block flagged to delete the bundle");
        };
        assert!(hv.to_remove.is_empty());

        let (delete_block, unknown) = with_unrecognised_block(block::Flags {
            delete_block_on_failure: true,
            ..Default::default()
        });
        let Ok(hv) = header_pass(delete_block.clone(), Pass::Receiver, &NullKeyProvider).await
        else {
            panic!("a receiver keeps the bundle");
        };
        assert_eq!(hv.to_remove, HashSet::from([unknown]));
        let Ok(hv) = header_pass(delete_block, Pass::Source, &NullKeyProvider).await else {
            panic!("a source carries a block flagged to delete itself");
        };
        assert!(hv.to_remove.is_empty(), "a source schedules nothing");
    }

    // A failed security check refuses a source's bundle, where a receiver
    // discards the block that failed it.
    #[cfg(feature = "rfc9173")]
    #[tokio::test]
    async fn a_source_refuses_a_failed_check() {
        let (tampered, key, signed_block) =
            tampered_signed_block(false, block::Type::Unrecognised(999));
        let tampered = Bytes::from(tampered);
        let keys = |key: bpsec::key::Key| Keys(vec![key]);

        let Ok(hv) = header_pass(tampered.clone(), Pass::Receiver, &keys(key.clone())).await else {
            panic!("a receiver keeps the bundle");
        };
        assert!(hv.to_remove.contains(&signed_block));
        assert!(matches!(
            header_pass(tampered, Pass::Source, &keys(key)).await,
            Err(HeaderFailure::Invalid {
                error: hardy_bpv7::Error::InvalidBPSec(bpsec::Error::IntegrityCheckFailed),
                ..
            })
        ));
    }

    // A source's per-hop blocks are unprotected: a BCB or a BIB on a Hop
    // Count refuses its bundle, which a receiver keeps.
    #[cfg(feature = "rfc9173")]
    #[tokio::test]
    async fn a_source_refuses_a_protected_per_hop_block() {
        let (encrypted, hop_block, _) = encrypted_hop_count_bundle();
        assert!(
            header_pass(encrypted.clone(), Pass::Receiver, &NullKeyProvider)
                .await
                .is_ok()
        );
        assert!(matches!(
            header_pass(encrypted, Pass::Source, &NullKeyProvider).await,
            Err(HeaderFailure::SourceRule(SourceRule::ProtectedPerHopBlock(n))) if n == hop_block
        ));

        let (signed, _, signed_block) = tampered_signed_block(false, block::Type::HopCount);
        assert!(matches!(
            header_pass(Bytes::from(signed), Pass::Source, &NullKeyProvider).await,
            Err(HeaderFailure::SourceRule(SourceRule::ProtectedPerHopBlock(n))) if n == signed_block
        ));
    }

    // A source's bundle carries no Previous Node block, which a receiver's
    // may.
    #[tokio::test]
    async fn a_source_refuses_a_previous_node() {
        let (_, data) = Builder::new("ipn:1.2".parse().unwrap(), "ipn:2.1".parse().unwrap())
            .add_extension_block(block::Type::PreviousNode)
            .unwrap()
            .build(
                hardy_cbor::encode::emit(&"ipn:9.0".parse::<Eid>().unwrap())
                    .0
                    .into(),
            )
            .with_payload(b"payload".as_slice().into())
            .build(CreationTimestamp::now())
            .unwrap();
        let data = Bytes::from(data);
        assert!(
            header_pass(data.clone(), Pass::Receiver, &NullKeyProvider)
                .await
                .is_ok()
        );
        assert!(matches!(
            header_pass(data, Pass::Source, &NullKeyProvider).await,
            Err(HeaderFailure::SourceRule(SourceRule::PreviousNode))
        ));
    }

    // A bundle with a Hop Count (flagged to delete the bundle if it cannot be
    // processed when `delete_bundle` is set) and a block of type 999 that asks
    // for a report on failure, neither carrying a CRC, as RFC 9173 §3.8.1
    // leaves a target, whose `signed` block is signed and then altered, the
    // alteration reaching the security check rather than a CRC: the tampered
    // bytes, the signing key and the signed block's number.
    #[cfg(feature = "rfc9173")]
    fn tampered_signed_block(
        delete_bundle: bool,
        signed: block::Type,
    ) -> (Vec<u8>, bpsec::key::Key, u64) {
        use hardy_bpv7::{
            bpsec::{
                key::{Key, KeyAlgorithm, Operation, Type},
                signer::{Context, Signer},
            },
            builder::Builder,
            creation_timestamp::CreationTimestamp,
        };
        use rand::{TryRng, rngs::SysRng};

        // Immaterial key value: generated, and handed back for the verifying
        // key source.
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

        let (_, data) = Builder::new("ipn:1.2".parse().unwrap(), "ipn:2.1".parse().unwrap())
            .add_extension_block(block::Type::HopCount)
            .unwrap()
            .with_flags(block::Flags {
                delete_bundle_on_failure: delete_bundle,
                ..Default::default()
            })
            .with_crc_type(crc::CrcType::None)
            .build(
                emit(&HopInfo {
                    limit: NonZeroU8::new(64).unwrap(),
                    count: 1,
                })
                .0
                .into(),
            )
            .add_extension_block(block::Type::Unrecognised(999))
            .unwrap()
            .with_flags(block::Flags {
                report_on_failure: true,
                ..Default::default()
            })
            .with_crc_type(crc::CrcType::None)
            .build(b"unknown".as_slice().into())
            .with_payload(b"payload".as_slice().into())
            .build(CreationTimestamp::now())
            .unwrap();
        let built = parse::parse(Bytes::from(data)).unwrap();
        let signed_block = *built
            .bundle
            .blocks
            .iter()
            .find(|(_, b)| b.block_type == signed)
            .expect("the block to sign is present")
            .0;
        let signed = Signer::new(&built.bundle, &built.data)
            .sign_block(
                signed_block,
                Context::HMAC_SHA2(Default::default()),
                "ipn:1.2".parse().unwrap(),
                &key,
            )
            .map_err(|(_, e)| e)
            .expect("sign the block")
            .rebuild()
            .expect("rebuild the signed bundle");

        // Raise the data's last byte by one, a count of 1 or a character:
        // still valid CBOR, no longer what was signed.
        let at = parse::parse(Bytes::copy_from_slice(&signed))
            .unwrap()
            .bundle
            .blocks
            .get(&signed_block)
            .expect("the signed block is present")
            .payload_range()
            .end as usize
            - 1;
        let mut tampered = signed.to_vec();
        tampered[at] += 1;
        (tampered, key, signed_block)
    }

    // RFC 9172 §5.1.1 for a Hop Count whose ciphertext fails authentication:
    // not liveness-critical, so the header pass schedules it for removal with
    // its security blocks, and the bundle survives, reporting the drop.
    #[cfg(feature = "rfc9173")]
    #[tokio::test]
    async fn corrupt_hop_count_is_scheduled_for_removal() {
        let (encrypted, hop_block, key) = encrypted_hop_count_bundle();
        let target = parse::parse(encrypted.clone()).unwrap().bundle.blocks[&hop_block].clone();
        // Flip the last ciphertext byte: the GCM check fails.
        let last = usize::try_from(target.payload_range().end).unwrap() - 1;
        let mut data = encrypted.to_vec();
        data[last] ^= 0x01;

        let (tx, mut rx) = hardy_async::channel::bounded(1);
        tx.send(Segment::Final(Bytes::from(data)))
            .await
            .expect("channel open");
        let Ok((hv, ..)) = parse_headers(
            &mut rx,
            Pass::Receiver,
            1 << 20,
            0,
            &Keys(vec![key.clone()]),
        )
        .await
        else {
            panic!("a corrupt Hop Count must not be fatal");
        };
        assert!(
            hv.to_remove.contains(&hop_block),
            "the corrupt Hop Count is scheduled for the failure-drop"
        );
        assert_eq!(
            reception_report_for(hv.report),
            ReceptionReport::FailureDropped
        );
    }

    // A bundle carrying a plaintext BIB, with `flags`, in an unrecognised
    // security context (99), over its payload or, with `over_bundle_age`,
    // over a Bundle Age block: the bytes, the BIB's number and its target's.
    fn with_unsupported_bib(flags: block::Flags, over: Option<block::Type>) -> (Bytes, u64, u64) {
        let target = if over.is_some() { 2 } else { 1 };
        let mut asb = Encoder::new();
        asb.emit_array(Some(1), |a| a.emit(&target));
        asb.emit(&99u64);
        asb.emit(&0u64);
        asb.emit(&"ipn:1.2".parse::<Eid>().unwrap());
        asb.emit_array(Some(1), |a| a.emit_array(Some(0), |_| {}));
        let asb = asb.build();

        let mut builder = Builder::new("ipn:1.2".parse().unwrap(), "ipn:2.1".parse().unwrap());
        if let Some(block_type) = over {
            let body = if block_type == block::Type::PreviousNode {
                hardy_cbor::encode::emit(&"ipn:0.4.0".parse::<Eid>().unwrap()).0
            } else {
                hardy_cbor::encode::emit(&5_000u64).0
            };
            builder = builder
                .add_extension_block(block_type)
                .unwrap()
                .build(body.into());
        }
        let (built, data) = builder
            .add_extension_block(block::Type::BlockIntegrity)
            .unwrap()
            .with_flags(flags)
            .build(asb.as_slice().into())
            .with_payload(b"payload".as_slice().into())
            .build(CreationTimestamp::now())
            .unwrap();
        let (&bib, _) = built
            .blocks
            .iter()
            .find(|(_, b)| b.block_type == block::Type::BlockIntegrity)
            .expect("the BIB is present");
        assert_eq!(
            built.blocks[&target].block_type,
            over.unwrap_or(block::Type::Payload)
        );
        (Bytes::from(data), bib, target)
    }

    // An operation in a security context this node does not support is its
    // acceptor's to act on: over a block the header pass does not accept,
    // here the payload, the BIB is left untouched whatever its flags, and
    // demands nothing.
    #[tokio::test]
    async fn an_unsupported_bib_is_left_untouched_where_unaccepted() {
        for flags in [
            block::Flags::default(),
            block::Flags {
                delete_block_on_failure: true,
                ..Default::default()
            },
            block::Flags {
                delete_bundle_on_failure: true,
                report_on_failure: true,
                ..Default::default()
            },
        ] {
            let (data, _, _) = with_unsupported_bib(flags.clone(), None);
            let Ok(hv) = header_pass(data, Pass::Receiver, &NullKeyProvider).await else {
                panic!("{flags:?}: the bundle passes the header pass");
            };
            assert!(hv.to_remove.is_empty(), "{flags:?}: nothing is removed");
            assert_eq!(
                reception_report_for(hv.report),
                ReceptionReport::Requested,
                "{flags:?}"
            );
        }
    }

    // Over a Previous Node, whose operations the next node accepts, an
    // unsupported operation is a failed acceptance: the block is discarded,
    // its value unread, and the BIB is itself a block this node cannot
    // process (RFC 9171 §5.6 Step 4), whose flags discard it, demand an
    // "Unknown security operation" report, or delete the bundle for it.
    #[tokio::test]
    async fn an_unsupported_bib_on_a_previous_node_meets_its_fate() {
        let over = Some(block::Type::PreviousNode);
        let (data, bib, node) = with_unsupported_bib(block::Flags::default(), over);
        let Ok(hv) = header_pass(data, Pass::Receiver, &NullKeyProvider).await else {
            panic!("an unflagged BIB keeps the bundle");
        };
        assert!(hv.to_remove.contains(&node) && !hv.to_remove.contains(&bib));
        assert_eq!(
            hv.parsed.per_hop.previous_node, None,
            "the value is not read"
        );
        assert_eq!(reception_report_for(hv.report), ReceptionReport::Requested);

        let discard = block::Flags {
            delete_block_on_failure: true,
            ..Default::default()
        };
        let (data, bib, node) = with_unsupported_bib(discard, over);
        let Ok(hv) = header_pass(data, Pass::Receiver, &NullKeyProvider).await else {
            panic!("a BIB flagged to discard itself keeps the bundle");
        };
        assert!(hv.to_remove.contains(&node) && hv.to_remove.contains(&bib));

        let report = block::Flags {
            report_on_failure: true,
            ..Default::default()
        };
        let (data, _, _) = with_unsupported_bib(report, over);
        let Ok(hv) = header_pass(data, Pass::Receiver, &NullKeyProvider).await else {
            panic!("a BIB asking for a report keeps the bundle");
        };
        assert_eq!(
            reception_report_for(hv.report),
            ReceptionReport::Demanded(ReasonCode::UnknownSecurityOperation)
        );

        let delete = block::Flags {
            delete_bundle_on_failure: true,
            ..Default::default()
        };
        let (data, _, _) = with_unsupported_bib(delete, over);
        let Err(HeaderFailure::Invalid {
            report: Some((_, _, reason)),
            ..
        }) = header_pass(data, Pass::Receiver, &NullKeyProvider).await
        else {
            panic!("a BIB flagged to delete the bundle deletes it");
        };
        assert_eq!(reason, ReasonCode::UnknownSecurityOperation);
    }

    // Over the Bundle Age of a bundle with a creation time, an unsupported
    // operation is no failure: this node replaces the block from the clock,
    // so it is discarded unread, the BIB's flags are not consulted, and
    // nothing is reported.
    #[tokio::test]
    async fn an_unsupported_bib_on_a_clocked_bundle_age_is_no_failure() {
        for flags in [
            block::Flags::default(),
            block::Flags {
                delete_block_on_failure: true,
                ..Default::default()
            },
            block::Flags {
                delete_bundle_on_failure: true,
                report_on_failure: true,
                ..Default::default()
            },
        ] {
            let (data, bib, age) =
                with_unsupported_bib(flags.clone(), Some(block::Type::BundleAge));
            let Ok(hv) = header_pass(data, Pass::Receiver, &NullKeyProvider).await else {
                panic!("{flags:?}: the bundle passes the header pass");
            };
            assert!(
                hv.to_remove.contains(&age) && !hv.to_remove.contains(&bib),
                "{flags:?}: the Bundle Age is discarded, and the BIB left to the cascade"
            );
            assert_eq!(hv.parsed.per_hop.bundle_age, None, "{flags:?}: unread");
            assert_eq!(
                reception_report_for(hv.report),
                ReceptionReport::Requested,
                "{flags:?}"
            );
        }
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

    // A bundle with no BCB or BIB never consults the key provider: the
    // header pass needs no keys for it.
    #[tokio::test]
    async fn header_pass_skips_keys_without_bpsec() {
        let calls = AtomicUsize::new(0);
        struct Counting<'a>(&'a AtomicUsize);
        impl KeyProvider for Counting<'_> {
            fn key_source(
                &self,
                _bundle: &Bpv7Bundle,
                _data: &[u8],
            ) -> Box<dyn bpsec::key::KeySource> {
                self.0.fetch_add(1, Ordering::Relaxed);
                Box::new(bpsec::key::KeySet::EMPTY)
            }
        }
        let provider = Counting(&calls);
        let (_, data) = small_bundle();
        let mut rx = stream_of(vec![Segment::Final(data)]).await;
        let Ok(_) = parse_headers(&mut rx, Pass::Receiver, 1 << 20, 0, &provider).await else {
            panic!("a plain bundle passes the header pass");
        };
        assert_eq!(calls.load(Ordering::Relaxed), 0);
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

        let Ok((hv, tail)) =
            parse_headers(&mut rx, Pass::Receiver, 1 << 20, 0, &NullKeyProvider).await
        else {
            panic!("a bundle confirmed by `Final` must pass the header pass");
        };
        let headers = hv.parsed.data;
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
            parse_headers(&mut rx, Pass::Receiver, 1 << 20, 0, &NullKeyProvider).await,
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
        }) = parse_headers(&mut rx, Pass::Receiver, 1 << 20, 0, &NullKeyProvider).await
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
            report: Some((_, facts, reason)),
            ..
        }) = parse_headers(&mut rx, Pass::Receiver, 1 << 20, 0, &NullKeyProvider).await
        else {
            panic!("trailing bytes must reject the bundle with a report");
        };
        assert_eq!(
            reception_report_for(facts),
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
            report: Some((_, facts, reason)),
            ..
        }) = parse_headers(&mut rx, Pass::Receiver, 1 << 20, 0, &NullKeyProvider).await
        else {
            panic!("a short Final must reject the bundle with a report");
        };
        assert_eq!(
            reception_report_for(facts),
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

        let Ok((hv, tail)) =
            parse_headers(&mut rx, Pass::Receiver, 1 << 20, 150, &NullKeyProvider).await
        else {
            panic!("a peeked bundle must pass the header pass");
        };
        let headers = hv.parsed.data;
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

        let Ok((hv, tail)) =
            parse_headers(&mut rx, Pass::Receiver, 1 << 20, 0, &NullKeyProvider).await
        else {
            panic!("the header pass must pass");
        };
        let headers = hv.parsed.data;
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

        let Ok((hv, tail)) =
            parse_headers(&mut rx, Pass::Receiver, 1 << 20, 4096, &NullKeyProvider).await
        else {
            panic!("a peeked bundle must pass the header pass");
        };
        let headers = hv.parsed.data;
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

            let Ok((hv, tail)) =
                parse_headers(&mut rx, Pass::Receiver, 1 << 20, 4096, &NullKeyProvider).await
            else {
                panic!("{shape}: a complete bundle confirmed by `Final` must pass");
            };
            let headers = hv.parsed.data;
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
            parse_headers(&mut rx, Pass::Receiver, 1 << 20, 4096, &NullKeyProvider).await,
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

        // Immaterial key value: the pass runs with `&NullKeyProvider`, so the key
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

        let Ok((hv, tail)) =
            parse_headers(&mut rx, Pass::Receiver, 1 << 20, 4096, &NullKeyProvider).await
        else {
            panic!("an encrypted payload must pass the header pass");
        };
        let headers = hv.parsed.data;
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

            let Ok((hv, tail)) =
                parse_headers(&mut rx, Pass::Receiver, 1 << 20, 150, &NullKeyProvider).await
            else {
                panic!("a fragment must pass the header pass");
            };
            let headers = hv.parsed.data;
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
        }) = parse_headers(&mut rx, Pass::Receiver, 1 << 20, 4096, &NullKeyProvider).await
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
        }) = parse_headers(&mut rx, Pass::Receiver, 1 << 20, 4096, &NullKeyProvider).await
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
            parse_headers(&mut rx, Pass::Receiver, bound, 4096, &NullKeyProvider).await,
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
            parse_headers(&mut rx, Pass::Receiver, 1 << 20, 4096, &NullKeyProvider).await,
            Err(HeaderFailure::Cancelled)
        ));
    }

    // The size bound applies to the declared whole-bundle size the moment
    // the header chain parses, carried in u64: a payload head declaring
    // 8 GiB refuses with the full declaration, from the head alone, before
    // a payload byte arrives.
    // A bundle whose payload `sign_key` signed, and, with `enc_key`, then
    // encrypted, which encrypts the BIB over it too (RFC 9172 §3.9): the
    // parse, with its op-sets decoded.
    #[cfg(feature = "rfc9173")]
    fn signed_payload(sign_key: &Key, enc_key: Option<&Key>) -> parse::Parsed {
        let (built, data) = Builder::new("ipn:1.2".parse().unwrap(), "ipn:2.1".parse().unwrap())
            .with_payload(vec![0xAB; 2_000].as_slice().into())
            .build(CreationTimestamp::now())
            .unwrap();
        let signed = Signer::new(&built, &data)
            .sign_block(
                1,
                signer::Context::HMAC_SHA2(Default::default()),
                "ipn:1.2".parse().unwrap(),
                sign_key,
            )
            .map_err(|(_, e)| e)
            .unwrap()
            .rebuild()
            .unwrap();
        let Some(enc_key) = enc_key else {
            return parse::parse(Bytes::from(signed)).unwrap();
        };
        let signed = parse::parse(Bytes::from(signed)).unwrap();
        let encrypted = Encryptor::new(&signed.bundle, &signed.data)
            .encrypt_block(
                1,
                Context::AES_GCM(Default::default()),
                "ipn:1.2".parse().unwrap(),
                enc_key,
            )
            .map_err(|(_, e)| e)
            .unwrap()
            .rebuild()
            .unwrap();
        parse::parse(Bytes::from(encrypted)).unwrap()
    }

    // A generated key for `operations`, its value immaterial.
    #[cfg(feature = "rfc9173")]
    fn generated_key(
        algorithm: Option<KeyAlgorithm>,
        enc: Option<EncAlgorithm>,
        operations: [Operation; 2],
    ) -> Key {
        let mut k = vec![0u8; 32];
        SysRng.try_fill_bytes(&mut k).unwrap();
        Key {
            key_type: Type::octet_sequence(k),
            key_algorithm: algorithm,
            enc_algorithm: enc,
            operations: Some(operations.into_iter().collect()),
            id: None,
            key_use: None,
        }
    }

    // The payload's verify stage checks the data as received: it begins a
    // verifier for a BIB over a plaintext payload, which verifies, and none
    // over an encrypted payload's ciphertext (RFC 9172 §3.9), nor where this
    // node holds no key, nor for a security context it does not support.
    #[cfg(feature = "rfc9173")]
    #[tokio::test]
    async fn the_payload_verify_stage_checks_only_plaintext_it_can() {
        let sign_key = generated_key(
            Some(KeyAlgorithm::HS256),
            None,
            [Operation::Sign, Operation::Verify],
        );
        let enc_key = generated_key(
            None,
            Some(EncAlgorithm::A256GCM),
            [Operation::Encrypt, Operation::Decrypt],
        );
        let keys = KeySet::new(vec![sign_key.clone(), enc_key.clone()]);

        let plain = signed_payload(&sign_key, None);
        let payload_len = {
            let payload = &plain.bundle.blocks[&1];
            payload.data.end - payload.data.start
        };
        let mut verifiers = begin_payload_verification(
            &plain.data,
            &keys,
            &plain.bundle.blocks,
            plain.bibs.iter().map(|(&bib, ops)| (bib, ops)),
            payload_len,
            false,
        )
        .unwrap();
        assert_eq!(verifiers.len(), 1, "a plaintext payload's BIB is begun");
        let (_, mut verifier) = verifiers.pop().unwrap();
        verifier.update(plain.bundle.blocks[&1].payload(&plain.data).unwrap());
        verifier.finish().expect("the payload's BIB verifies");

        // A verifier passes over an operation it holds no key for; an
        // acceptor's stage, strict, refuses to begin.
        assert!(
            begin_payload_verification(
                &plain.data,
                &KeySet::EMPTY,
                &plain.bundle.blocks,
                plain.bibs.iter().map(|(&bib, ops)| (bib, ops)),
                payload_len,
                false
            )
            .unwrap()
            .is_empty(),
            "no key, no verifier"
        );
        assert!(
            matches!(
                begin_payload_verification(
                    &plain.data,
                    &KeySet::EMPTY,
                    &plain.bundle.blocks,
                    plain.bibs.iter().map(|(&bib, ops)| (bib, ops)),
                    payload_len,
                    true
                ),
                Err(hardy_bpv7::Error::InvalidBPSec(bpsec::Error::NoKey))
            ),
            "no key, no acceptance"
        );

        // An encrypted payload's BIBs are its acceptor's to check against
        // the plaintext: the header pass defers none.
        let encrypted = signed_payload(&sign_key, Some(&enc_key));
        let (sign_key, enc_key) = (sign_key.clone(), enc_key.clone());
        let hv = header_pass(
            encrypted.data.clone(),
            Pass::Receiver,
            &Keys(vec![sign_key, enc_key]),
        )
        .await
        .map_err(|_| ())
        .expect("the header pass verifies");
        assert!(
            hv.deferred_verifiers.is_empty(),
            "the ciphertext is never checked"
        );

        let (data, bib, _) = with_unsupported_bib(block::Flags::default(), None);
        let unsupported = parse::parse(data).unwrap();
        assert!(unsupported.bibs.contains_key(&bib));
        let payload_len = {
            let payload = &unsupported.bundle.blocks[&1];
            payload.data.end - payload.data.start
        };
        assert!(
            begin_payload_verification(
                &unsupported.data,
                &keys,
                &unsupported.bundle.blocks,
                unsupported.bibs.iter().map(|(&bib, ops)| (bib, ops)),
                payload_len,
                false
            )
            .expect("an unsupported context is the classification's")
            .is_empty(),
        );
    }

    #[tokio::test]
    async fn a_declaration_beyond_32_bits_refuses_at_the_header_pass() {
        let (data, start) = peek_bundle();

        // Swap the payload's 1000-byte definite head for an 8 GiB one, and
        // send nothing past it.
        assert_eq!(data[start - 3..start], hex!("59 03 E8"));
        let mut head = data[..start - 3].to_vec();
        head.extend_from_slice(&hex!("5B 00 00 00 02 00 00 00 00"));
        let mut rx = stream_of(vec![Segment::Next(Bytes::from(head))]).await;

        let Err(HeaderFailure::TooLarge { size, max }) =
            parse_headers(&mut rx, Pass::Receiver, 1 << 20, 0, &NullKeyProvider).await
        else {
            panic!("an over-declared bundle must refuse");
        };
        // The bundle as built, less its 1000 payload bytes, with the head
        // six bytes longer, plus the 8 GiB declared.
        let declared = u64::try_from(data.len() - 1000 + 6).unwrap() + (8 << 30);
        assert_eq!(size, declared, "the declared wire size is reported");
        assert!(size > u64::from(u32::MAX));
        assert_eq!(max, 1 << 20);
    }
}
