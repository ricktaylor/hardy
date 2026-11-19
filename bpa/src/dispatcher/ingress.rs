use alloc::sync::Arc;
use core::{mem::take, ops::ControlFlow};
use trace_err::*;

use hardy_bpv7::{
    Error as Bpv7Error,
    block::{self, BibCoverage},
    bpsec::{Role, Roles, SecurityServices, bcb, bib, key::KeySource},
    builder::PayloadTrailer,
    editor::Chunk,
    eid::NodeId,
    parse::Parsed,
    status_report::ReasonCode,
};
use tracing::{debug, info};

#[cfg(feature = "instrument")]
use tracing::instrument;

use super::{
    Dispatcher, rewrite_received_bundle, rewrite_received_headers,
    validate::{Rewrite, ValidatingReceiver, ValidationFailure},
};
use crate::{
    HashMap,
    bundle::{
        Bundle, BundleMetadata, BundleStatus,
        arrival::{
            HeaderFailure, Headers, HopCountKept, Pass,
            fate::check_announced,
            parse_headers,
            payload::{begin_payload_decryption, begin_payload_verifiers},
            report::ReportFacts,
        },
    },
    cla::{Acceptance, ClaAddress, Segment},
    filter::slots::MetadataDelta,
    otel_metrics::{reason_label, status_label},
    rebuilt_headers::RebuiltHeaders,
    routing::{Decision, DispatchAction},
    stream::{ConcatError, Receiver},
};

/// Count a drop of a received bundle, labelled by reason. Every drop site in
/// the received pipeline increments this counter exactly once (see the drop
/// accounting note on `receive_bundle`); a new drop site calls this rather
/// than respelling the counter name and label vocabulary.
fn count_received_dropped(reason: &ReasonCode) {
    metrics::counter!("bpa.bundle.received.dropped", "reason" => reason_label(reason)).increment(1);
}

// What the gate decisions (Ingress chain + route lookup) settle for an
// arrival they admit. The gate returns `None` for a rejection, which it has
// already counted and reported.
struct Admitted {
    // The routing decision of record, with the table it was made against.
    decision: Decision,
    // The security roles the Ingress chain announced this node takes on
    // the blocks.
    roles: HashMap<u64, Roles>,
}

// What the ingress rewrite makes of an admitted arrival, for the drain: the
// bundle as received; its headers rebuilt, the payload streaming on as
// received; or, where this node accepts the payload, its headers rebuilt
// through the payload block's header, the payload's data framed by the
// trailer.
enum Rewritten {
    Unchanged,
    Headers(hardy_bpv7::Bundle, Vec<Chunk>),
    Payload(hardy_bpv7::Bundle, Vec<Chunk>, PayloadTrailer),
}

// The payload's stages, begun on the resident headers for the drain to
// feed, where this node accepts the payload or verifies its ciphertext: the
// decrypt stage, where a BCB covers the payload; the Verify stage's
// verifiers, over what the decrypt stage hands on; and the length of the
// data the stages hand on, the plaintext's where the payload is decrypted.
struct PayloadStages {
    decryptor: Option<bcb::Decryptor>,
    verifiers: Vec<(u64, bib::Verifier)>,
    len: u64,
}

// The outcome of the shared receive pipeline, for the three in-feeds.
//
// `Dispatched` and `Disposed` are both *acceptance* (the bundle ran the
// Ingress chain and its routing decision was executed or queued for
// dispatch, or it was dropped internally with reports); `Refused` is the one
// non-acceptance outcome — the transfer could not be taken at all
// (truncation, the size cap) and its custodian keeps responsibility.
pub(super) enum Received {
    /// Admitted: the bundle ran the Ingress chain, was written once to the
    /// metadata store, and had the gate's routing decision executed (or
    /// queued for dispatch).
    Dispatched,
    /// Accepted and disposed of internally (invalid, gate-rejected,
    /// chain-dropped, duplicate) — reports emitted where possible; nothing
    /// dispatched.
    Disposed,
    /// Acceptance refused: truncated stream or over the size cap. The
    /// refusal site logs the specifics.
    Refused,
}

impl Dispatcher {
    // Entry point for bundles received from CLAs.
    //
    // The return value is the acceptance verdict (the `cla::Sink::dispatch`
    // contract): `Accepted` covers every internally-disposed case (invalid,
    // gate-rejected, duplicate) too — refusing an invalid bundle would
    // invite the previous node's custody/retransmission machinery to resend
    // identical bytes forever, so the first BPA to detect a content problem
    // accepts and terminates the bundle. `Refused` is reserved for transfers
    // this node could not take at all (truncation, the size cap). Returning
    // without draining `stream` is deliberately meaningless on its own: the
    // producer learns the verdict from this return, never from the stream
    // closing.
    //
    // # Bundle State
    //
    // - `process_received_bundle()` runs the Ingress filter and the gate
    //   route lookup, writes the record once at `Dispatching`, and executes
    //   the routing decision through `execute_admitted` — fresh arrivals do
    //   not transit the dispatch queue (`DispatchPending` belongs to the
    //   re-dispatch paths), except the admin-endpoint and fragment arms,
    //   which queue for the processing pool.
    //
    // See [Bundle State Machine Design](../../docs/bundle_state_machine_design.md)
    // for the complete state transition diagram.
    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    pub async fn receive_bundle(
        &self,
        ingress_cla: Arc<str>,
        ingress_peer_node: Option<&NodeId>,
        ingress_peer_addr: Option<&ClaAddress>,
        stream: &mut dyn Receiver<Segment>,
    ) -> Acceptance {
        let metadata = BundleMetadata::ingress(
            ingress_cla,
            ingress_peer_node.cloned(),
            ingress_peer_addr.cloned(),
        );

        // Truncation and the size bound refuse the transfer — the CLA must
        // not acknowledge it, so the peer retransmits — while
        // invalid-but-complete bundles are accepted and handled internally:
        // the CLA cannot fix invalid content, and the transfer itself
        // succeeded.
        //
        // Drop sites inside `process_received_bundle` count themselves under
        // `bpa.bundle.received.dropped` with a `reason` label. Nothing was
        // stored on the CLA path before a drop, so there's no data to clean up.
        match self.process_received_bundle(stream, metadata).await {
            Received::Dispatched | Received::Disposed => Acceptance::Accepted,
            Received::Refused => Acceptance::Refused,
        }
    }

    // Shared bundle processing: parse and validate the headers, gate them
    // (the Ingress chain and the route lookup among the gate's checks), drain
    // and store the payload, report, and execute the routing decision.
    //
    // Called from the CLA ingress path (`receive_bundle`), the ADU
    // reassembly path (`reassemble`), and restart orphan recovery. Handles
    // all bundle validation internally — invalid bundles are logged,
    // counted, and dropped with status reports where possible (`Disposed`);
    // only truncation and the size cap refuse (`Refused`). An admitted
    // bundle runs the Ingress chain, is written once to the metadata store
    // (the P1 checkpoint), and has its gate routing decision executed or
    // queued for dispatch (`Dispatched`).
    //
    // Every caller hands the bundle as a segment stream — a caller holding
    // whole bytes passes them directly (`Bytes` is a `Receiver<Segment>`) —
    // and the spool saves an admitted bundle fresh: `metadata.storage_name`
    // must be unset, and a caller replaying pre-stored bytes (reassembly,
    // restart orphans) owns its own copy's cleanup after this returns.
    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    pub(super) async fn process_received_bundle(
        &self,
        stream: &mut dyn Receiver<Segment>,
        mut metadata: BundleMetadata,
    ) -> Received {
        // Pre-drain header pass: parse the header chain off the stream and run
        // keyed header verification — both in `bundle::arrival`, before a payload
        // still arriving is spooled. `Err` carries an optional recoverable bundle to
        // report before dropping (reporting stays here — we own the machinery);
        // a structural failure, or a stream that ended inside the headers,
        // carries none. The pass holds the Ingress chain's payload peek: none
        // for an empty chain.
        let (hv, tail) = match parse_headers(
            stream,
            Pass::Receiver,
            self.max_bundle_size_wire(),
            self.filters.ingress.peek,
            &*self.key_provider,
        )
        .await
        {
            Ok(parts) => parts,
            Err(HeaderFailure::Cancelled) => {
                debug!("Bundle stream cancelled mid-header; refused");
                return Received::Refused;
            }
            Err(HeaderFailure::TooLarge { size, max }) => {
                // Covers both bounds `parse_headers` enforces: the
                // accumulated header bytes and the declared whole-bundle
                // size — an over-cap bundle, resident or still on the
                // wire, refuses before a single payload byte drains.
                debug!("Bundle exceeds max_bundle_size: {size} > {max}; refused");
                return Received::Refused;
            }
            Err(HeaderFailure::Invalid { report, .. }) => {
                let reason = match report {
                    Some((bundle, facts, reason)) => {
                        // Complete but invalid, with a recoverable id: the
                        // drop is reported like the sibling gate and drain
                        // drops (RFC 9171 §5.6/§5.10). A structural failure,
                        // or a stream that ended inside the headers (`None`),
                        // has no id to report: the §4.1 discard, outside the
                        // reception state machine.
                        self.report_bundle_reception(
                            &bundle,
                            metadata.received_at(),
                            facts,
                            Some(reason),
                        )
                        .await;
                        reason
                    }
                    None => ReasonCode::BlockUnintelligible,
                };
                count_received_dropped(&reason);
                return Received::Disposed;
            }
            Err(HeaderFailure::SourceRule(_)) => {
                unreachable!("a receiver's header pass checks no source rules")
            }
        };

        // Reserved primary-block flag bits have no protocol effect, but they are
        // the only sign of a peer setting them: say so.
        let u = hv.parsed.bundle.primary.flags.unrecognised;
        if u != 0 {
            debug!(
                bundle_id = %hv.parsed.bundle.primary.id,
                "Bundle primary block has unrecognised flag bits set: {u:#x}"
            );
        }

        // Early-reject gate (lifetime / hop) before the payload is drained, so a
        // dead bundle is dropped having spooled nothing. (The dispatch expiry
        // checkpoint re-checks lifetime after the store — a cheap, harmless
        // overlap.)
        if let Some(reason) = hv.gate_reason(metadata.received_at()) {
            count_received_dropped(&reason);
            if let ReasonCode::LifetimeExpired = reason {
                // A bundle that arrives already expired is treated as if it
                // never arrived, not amplified into report traffic — §5.10
                // deletion reports are for bundles that expire in custody (the
                // dispatch, forward, and deliver expiry checkpoints and the
                // reaper). Dropping before anything
                // is stored also keeps expired traffic from churning the
                // metadata store's dedup LRU.
                debug!("Bundle arrived already expired; dropped");
                return Received::Disposed;
            }
            self.report_bundle_reception(
                &hv.parsed.bundle,
                metadata.received_at(),
                hv.report,
                Some(reason),
            )
            .await;
            return Received::Disposed;
        }

        // Config-gated RFC 9171 validity checks, at the same pre-drain seat
        // as the lifetime/hop gate: policy rejections that go beyond
        // structural validity (deployments may relax them), reported like
        // any other gated drop (§5.6/§5.10).
        if let Some(reason) = self.rfc9171_gate_reason(&hv) {
            count_received_dropped(&reason);
            self.report_bundle_reception(
                &hv.parsed.bundle,
                metadata.received_at(),
                hv.report,
                Some(reason),
            )
            .await;
            return Received::Disposed;
        }

        // Destructure the verified headers once, here at the gate: the parse
        // stays whole through the gate's decisions, the acceptance and the
        // rewrite, its decoded per-hop fields go into the metadata (a
        // Classifier may read them), and the begun payload-BIB verifiers,
        // the scheduled §E removals and the reception reason serve the
        // rewrite, drain and store below.
        let Headers {
            mut parsed,
            mut to_remove,
            mut accepted,
            hop_count_kept,
            mut report,
            mut verified,
            mut deferred_verifiers,
            keys,
        } = hv;
        metadata.per_hop = parsed.per_hop.clone();

        // A bundle whose Bundle Age the header pass discards still wants one:
        // a node upstream added it, so egress writes a fresh one from the
        // clock, for downstream nodes without one (RFC 9171 §5.5). The mark is
        // written as a Classifier's would be, ahead of the Ingress chain,
        // which may clear it. A bundle without a creation time never gets
        // here without its Bundle Age: the header pass rejects it.
        if to_remove.iter().any(|n| {
            parsed
                .bundle
                .blocks
                .get(n)
                .is_some_and(|b| b.block_type == block::Type::BundleAge)
        }) {
            metadata.apply(MetadataDelta {
                add_bundle_age: Some(true),
                ..Default::default()
            });
        }

        // Every arrival spools through the store's streaming seam — the
        // resident head may include payload bytes, or the whole bundle, so
        // there is one store path, not a resident/streamed fork. The door
        // owns the validation: a `ValidatingReceiver` decorates the arrival
        // and `Store::save_stream` is the validation-blind drain. For a
        // complete-at-head arrival the decorator yields the head as the
        // stream's `Final` and settles from it alone: the header pass already
        // took the arrival's `Final`, so the inner stream is not pulled again.
        debug_assert!(metadata.storage_name.is_none());

        // The gate decisions settle before the payload drains, so a rejected
        // arrival returns here having spooled nothing; the producer learns
        // the verdict from the door's return, never from the stream.
        let Some(Admitted { decision, roles }) = self
            .decide_at_gate(&mut parsed, &mut metadata, &*keys, report)
            .await
        else {
            return Received::Disposed;
        };

        // The bundle's destination is the acceptor of every security
        // operation in it (RFC 9172 §1.4): where the gate decided to deliver
        // it here, now or once its service registers, every block an
        // operation targets joins the acceptors, the payload among them. A
        // Hop Count the header pass kept for the encrypted BIB it shares
        // joins them too, accepted where this node accepts every block that
        // BIB covers.
        let local = self.delivers_locally(&parsed.bundle, &metadata, &decision);

        // A fragment's own security operations are the reassembled bundle's
        // to process (RFC 9172 §5.2): every role the chain announced on it
        // is passed over, and the destination's acceptance waits for the
        // whole, which crosses this gate once reassembled. One addressed
        // here that a Classifier
        // declined to reassemble (`MetadataDelta::reassemble`) is dropped,
        // reported: a raw fragment is never delivered, and cannot usefully
        // forward.
        let fragment = parsed.bundle.primary.id.fragment_info.is_some();
        if fragment && local && metadata.reassemble() == Some(false) {
            let reason = ReasonCode::NoAdditionalInformation;
            debug!("A Classifier declined to reassemble a fragment addressed here; dropped");
            count_received_dropped(&reason);
            self.report_bundle_reception(
                &parsed.bundle,
                metadata.received_at(),
                report,
                Some(reason),
            )
            .await;
            return Received::Disposed;
        }

        // This node's roles on the blocks the gate checks, the acceptor's
        // outranking the verifier's on one operation: the chain's
        // announcements, a per-hop block's acceptance among them but not
        // its verification, since the header pass accepts the per-hop
        // blocks' operations before the chain runs (RFC 9171 §5.4), kept or
        // discarded; the destination's acceptance of every operation (RFC
        // 9172 §1.4), which outranks any verification announced; and a Hop
        // Count the header pass kept for the encrypted BIB it shares,
        // accepted where this node accepts every block that BIB covers. The
        // services policy announced on a block, to accept or to verify, it
        // expects an operation of (`expected`); the destination's own
        // acceptance expects nothing. A block the header pass accepted or
        // the fate discards is outside the pass, and a fragment's every
        // block is.
        let mut gate_roles: HashMap<u64, Roles> = HashMap::new();
        let mut expected: HashMap<u64, SecurityServices> = HashMap::new();
        if !fragment {
            for (&n, r) in &roles {
                // Compared by encoding, so an alias of the type counts.
                let per_hop = parsed.bundle.blocks.get(&n).is_some_and(|b| {
                    b.block_type == block::Type::PreviousNode
                        || b.block_type == block::Type::BundleAge
                        || b.block_type == block::Type::HopCount
                });
                let announced = if per_hop {
                    Roles::each(Role::Acceptor, r.accepts())
                } else {
                    *r
                };
                let services = announced.accepts() | announced.verifies();
                if !services.any() {
                    continue;
                }
                gate_roles.insert(n, announced);
                expected.insert(n, services);
            }
            if local {
                for (&n, b) in &parsed.bundle.blocks {
                    if b.bcb.is_some() || b.bib != BibCoverage::None {
                        *gate_roles.entry(n).or_default() |=
                            Roles::each(Role::Acceptor, SecurityServices::BOTH);
                    }
                }
                *gate_roles.entry(1).or_default() |=
                    Roles::each(Role::Acceptor, SecurityServices::BOTH);
            }
            if let Some((number, HopCountKept::SharesEncryptedBib { .. })) = hop_count_kept {
                *gate_roles.entry(number).or_default() |=
                    Roles::each(Role::Acceptor, SecurityServices::BOTH);
            }
            gate_roles.retain(|n, _| !accepted.contains_key(n) && !to_remove.contains(n));
            expected.retain(|n, _| gate_roles.contains_key(n));
        }

        // Their operations are processed now, before anything is spooled,
        // for the rewrite below to remove what is accepted; a designated
        // block must carry an operation of the service this arrival checked,
        // and one that does not meets its fate. At the destination a Hop
        // Count this node cannot accept meets its fate too, there being no
        // node downstream to pass it to.
        let mut verify_payload_ciphertext = false;
        if !gate_roles.is_empty() {
            match check_announced(
                &gate_roles,
                &expected,
                &mut parsed,
                &*keys,
                &verified,
                !deferred_verifiers.is_empty(),
                local,
            ) {
                Ok(outcome) => {
                    report |= outcome.fate.report;
                    accepted.extend(outcome.accepted);
                    verified.extend(outcome.verified);
                    to_remove.extend(outcome.fate.discarded);
                    verify_payload_ciphertext = outcome.verify_payload_ciphertext;
                }
                Err(rejected) => {
                    let reason = rejected.reason();
                    debug!(
                        "The gate's check of an announced block rejected the bundle: {rejected}"
                    );
                    count_received_dropped(&reason);
                    self.report_bundle_reception(
                        &parsed.bundle,
                        metadata.received_at(),
                        report | rejected.report(),
                        Some(reason),
                    )
                    .await;
                    return Received::Disposed;
                }
            }
        }

        // Where this node forwards the bundle, a Hop Count the header pass
        // left as received and the gate did not accept travels unchanged,
        // logged.
        if !local
            && let Some((number, reason)) = &hop_count_kept
            && !accepted.contains_key(number)
            && !to_remove.contains(number)
        {
            info!("Hop Count block {number} travels unchanged: {reason}");
        }

        // The deferred-BIB verifiers were begun by the header pass, in the
        // same keyed scope as the header verify; the decorator absorbs the
        // resident payload prefix at construction, the streamed remainder
        // as it arrives — feeding the payload CRC, the block+outer framing,
        // and each deferred BIB digest as the bytes stream past. It
        // validates the bytes as received.
        let payload_data = parsed.bundle.blocks[&1].payload_range();

        // The accepted per-hop operations and the scheduled removals leave
        // the bundle before it is first stored: its headers are rebuilt
        // without them, and the record takes the rebuilt block map. A BIB
        // the removal cascade decrypts and finds invalid rejects the bundle.
        // Where this node accepts the payload, its operations are processed
        // as it streams: the decrypt stage decrypts it where a BCB covers
        // it, the Verify stage checks a BIB over it against what the decrypt
        // stage hands on (RFC 9172 §3.9: the BCB first, the BIB second), and
        // the payload is stored rewritten, in plaintext without those
        // operations. The keys were found at the acceptance above, so each
        // stage begins.
        let payload = accepted.get(&1).copied().unwrap_or_default();
        let payload_stages = if payload.any() || verify_payload_ciphertext {
            let decryptor = if payload.confidentiality || verify_payload_ciphertext {
                begin_payload_decryption(&parsed, &*keys)
            } else {
                Ok(None)
            };
            let stages = decryptor.and_then(|decryptor| {
                let len = decryptor
                    .as_ref()
                    .map_or(payload_data.end - payload_data.start, |d| d.plaintext_len());
                // The BIB over the payload. Over an accepted payload's
                // plaintext every operation begins afresh: its acceptor's
                // where the payload is accepted for integrity, and a
                // verifier's, where it holds the key, over a payload
                // accepted for confidentiality alone. Over a payload
                // received in plaintext the verifiers the header pass began
                // are those: one per BIB it could check, over the data as
                // received, and none where the payload was resident and the
                // check verified it there; an operation it holds no key for
                // or cannot process has already failed the acceptance.
                let verifiers = if !payload.any() {
                    Ok(Vec::new())
                } else if decryptor.is_some() {
                    begin_payload_verifiers(&parsed, &*keys, len, payload.integrity)
                } else {
                    Ok(take(&mut deferred_verifiers))
                };
                verifiers.map(|verifiers| PayloadStages {
                    decryptor,
                    verifiers,
                    len,
                })
            });
            match stages {
                Ok(stages) => Some(stages),
                Err(e) => {
                    let reason = ReasonCode::from(&e);
                    debug!("The payload's acceptance rejected the bundle: {e}");
                    count_received_dropped(&reason);
                    self.report_bundle_reception(
                        &parsed.bundle,
                        metadata.received_at(),
                        report,
                        Some(reason),
                    )
                    .await;
                    return Received::Disposed;
                }
            }
        } else {
            None
        };

        let rewrite: Result<Rewritten, Bpv7Error> = match &payload_stages {
            Some(stages) if payload.any() => {
                rewrite_received_bundle(&parsed, &accepted, to_remove, &*keys, stages.len)
                    .map(|(rebuilt, chunks, trailer)| Rewritten::Payload(rebuilt, chunks, trailer))
            }
            _ => rewrite_received_headers(&parsed, &accepted, to_remove, &*keys).map(|rewrite| {
                match rewrite {
                    Some((rebuilt, chunks)) => Rewritten::Headers(rebuilt, chunks),
                    None => Rewritten::Unchanged,
                }
            }),
        };
        let rewrite = match rewrite {
            Ok(rewrite) => rewrite,
            Err(e) => {
                let reason = ReasonCode::from(&e);
                debug!("The ingress rewrite rejected the bundle: {e}");
                count_received_dropped(&reason);
                self.report_bundle_reception(
                    &parsed.bundle,
                    metadata.received_at(),
                    report,
                    Some(reason),
                )
                .await;
                return Received::Disposed;
            }
        };
        // Drain the admitted arrival into the store, bounded by
        // `max_bundle_size` as the defensive backstop (the declared size was
        // already enforced at parse; a producer exceeding its declaration
        // trips the framing checks first). The record is built here, once,
        // from the bundle the rewrite leaves, its headers rebuilt or the
        // bundle as received, and the metadata the gate wrote: born at
        // `Dispatching` — the status it is persisted at — because nothing
        // observes the record before the single insert below; only the
        // finished, classified record ever reaches storage.
        let max_size = self.max_bundle_size_mem();
        let Parsed {
            data: headers,
            bundle: received,
            ..
        } = parsed;
        let resident = headers.clone();
        let (bpv7, outcome, tail_rx) = match rewrite {
            Rewritten::Payload(rebuilt, chunks, trailer) => {
                let stages =
                    payload_stages.trace_expect("a payload rewrite follows the payload's stages");
                let mut tail_rx = ValidatingReceiver::new(
                    stream,
                    tail,
                    stages.decryptor,
                    stages.verifiers,
                    headers,
                    payload_data,
                    Some(Rewrite::new(
                        Chunk::flatten_head(chunks, &resident).into(),
                        trailer,
                    )),
                );
                let outcome = self.store.save_stream(&mut tail_rx, max_size).await;
                (rebuilt, outcome, tail_rx)
            }
            Rewritten::Headers(rebuilt, chunks) => {
                let mut tail_rx = ValidatingReceiver::new(
                    stream,
                    tail,
                    payload_stages.and_then(|stages| stages.decryptor),
                    deferred_verifiers,
                    headers,
                    payload_data,
                    None,
                );
                let outcome = {
                    let mut rebuilt_rx = RebuiltHeaders::new(chunks, &resident, &mut tail_rx);
                    self.store.save_stream(&mut rebuilt_rx, max_size).await
                };
                (rebuilt, outcome, tail_rx)
            }
            Rewritten::Unchanged => {
                let mut tail_rx = ValidatingReceiver::new(
                    stream,
                    tail,
                    payload_stages.and_then(|stages| stages.decryptor),
                    deferred_verifiers,
                    headers,
                    payload_data,
                    None,
                );
                let outcome = self.store.save_stream(&mut tail_rx, max_size).await;
                (received, outcome, tail_rx)
            }
        };
        let mut bundle = Bundle {
            bpv7,
            metadata,
            status: BundleStatus::Dispatching,
        };

        // Settle the decorator's verdict against the save. A save the
        // verdict then rejects was staged before the verdict settled — the
        // discard half of the streaming contract, owed by this door now
        // that the drain is validation-blind.
        let outcome = match outcome {
            Ok((storage_name, len)) => match tail_rx.finish() {
                Ok(()) => Ok((storage_name, len)),
                Err(failure) => {
                    self.store.delete_data(&storage_name).await;
                    Err(failure)
                }
            },
            // A failed drain persisted nothing, and settles through the
            // decorator's verdict. A cancelled drain is the decorator failing
            // a pull, its verdict latched first (a producer that went away
            // latches `Truncated`). The defensive bound cannot trip: the tail
            // rejects any byte past the outer break before the bound counts
            // it, and the header pass refused a declaration over the same
            // bound.
            Err(ConcatError::Cancelled | ConcatError::TooLarge { .. }) => Err(tail_rx
                .finish()
                .err()
                .unwrap_or(ValidationFailure::Truncated)),
        };

        // Settle the store from the spool's outcome. The bundle is stored as
        // received, less its scheduled removals.
        let (storage_name, data_len) = match outcome {
            Ok(saved) => saved,
            Err(failure) => {
                let Some(reason) = failure.reason_code() else {
                    // Truncated: the transfer never completed, so it is
                    // refused — the peer retains custody and may resend.
                    // A refusal is never reported.
                    debug!("Truncated payload; refused");
                    return Received::Refused;
                };
                // Complete but unacceptable: the transfer was accepted,
                // so this node owns the bundle and terminates it —
                // reported like the sibling gate drops (RFC 9171
                // §5.6/§5.10). Nothing remains staged: the settle above
                // discarded any save the verdict rejected.
                debug!("Streamed payload rejected: {failure}");
                count_received_dropped(&reason);
                self.report_bundle_reception(
                    &bundle.bpv7,
                    bundle.metadata.received_at(),
                    report,
                    Some(reason),
                )
                .await;
                return Received::Disposed;
            }
        };
        bundle.metadata.storage_name = Some(storage_name.clone());

        // Only a completely assembled bundle counts as received.
        metrics::counter!("bpa.bundle.received").increment(1);
        metrics::counter!("bpa.bundle.received.bytes").increment(data_len as u64);

        // Reception happened, so report it (when requested) before the duplicate
        // check: RFC 9171 §5.6 reports on reception, so a replayed/duplicate
        // bundle is still reported as received. (The Ingress chain already ran
        // at the pre-drain gate; a chain drop reported itself there.)
        self.report_bundle_reception(&bundle.bpv7, bundle.metadata.received_at(), report, None)
            .await;

        // `insert_metadata` is the authoritative atomic dup check: the gate's
        // advisory `seen_recently` probe only sees committed records, so duplicate
        // copies racing in concurrently both spool and settle here — the
        // loser is dropped and never dispatched.
        //
        // A duplicate *invalid* bundle (rejected before reaching here) isn't
        // deduplicated — a replay re-parses and may re-report. Accepted, not fixed:
        // RFC 9171 status reports are off-by-default debugging aids, not acks, so a
        // duplicate is harmless. Tombstone-on-reject suppression is deferred — the
        // future compressed-status-report / custody work inverts the requirement (a
        // resend then means "report lost, please re-report"), so that design must
        // own the semantics. Ledgered in bpa/docs/TODO.md ("Tombstone-on-reject
        // suppression").
        if !self.store.insert_metadata(&bundle).await {
            // Bundle with matching id already exists in the metadata store.
            metrics::counter!("bpa.bundle.received.duplicate").increment(1);
            // The spool saved this copy's data; the duplicate loses it.
            self.store.delete_data(&storage_name).await;
            return Received::Disposed;
        }

        // Account the admitted bundle in the status gauge.
        metrics::gauge!("bpa.bundle.status", "state" => status_label(&bundle.status))
            .increment(1.0);

        // Execute the gate's routing decision of record, made again if the
        // table changed during the drain. Fresh arrivals do not transit the
        // dispatch queue (`DispatchPending` belongs to the re-dispatch paths:
        // parks, polls, sweeps, restart, transfer outcomes), except the
        // admin-endpoint and fragment arms (see `execute_admitted`). A crash
        // between the insert above and this execution recovers through
        // restart's `Dispatching` arm, which re-dispatches with a fresh
        // lookup.
        self.execute_admitted(bundle, decision).await;
        Received::Dispatched
    }

    // The gate decisions for an admitted header chain: the Ingress filter
    // chain, then the route lookup — the decision of record. These settle
    // before the payload drains, so a rejection (`None`) returns with
    // nothing spooled or persisted. All rejection counting and reporting
    // happens here.
    async fn decide_at_gate(
        &self,
        parsed: &mut Parsed,
        metadata: &mut BundleMetadata,
        keys: &dyn KeySource,
        report: ReportFacts,
    ) -> Option<Admitted> {
        // Early duplicate probe, first — before any filter or route work: a
        // replay is DoS-shaped traffic, so a recently-committed id settles
        // it for the price of a cache lookup, without touching storage.
        // Advisory only: copies racing in concurrently, ids past the
        // cache's horizon, and a cold cache after restart all settle at
        // insert_metadata's atomic refusal. Reception is still reported
        // (RFC 9171 §5.6 reports on reception; dedup is a dispatch
        // concern), exactly as the post-spool duplicate path reports it.
        if self.store.seen_recently(&parsed.bundle.primary.id) {
            debug!("Duplicate bundle detected at the ingress gate");
            metrics::counter!("bpa.bundle.received.duplicate").increment(1);
            self.report_bundle_reception(&parsed.bundle, metadata.received_at(), report, None)
                .await;
            return None;
        }

        // Ingress chain at the pre-drain gate, on the resident prefix: the
        // headers and whatever of the payload has arrived, at least the held
        // peek. It runs synchronously on the parse and writes the metadata
        // in place, so a Classifier's deltas survive. A filter reading a
        // payload that is not all resident gets the reader's `NotResident`.
        let mut roles = HashMap::new();
        if self.filters.has_ingress()
            && let ControlFlow::Break(reason) =
                self.filters.run_ingress(parsed, metadata, keys, &mut roles)
        {
            let label = reason.unwrap_or(ReasonCode::NoAdditionalInformation);
            count_received_dropped(&label);
            self.report_bundle_reception(&parsed.bundle, metadata.received_at(), report, reason)
                .await;
            return None;
        }

        // Route at the gate — the decision of record for this arrival,
        // carrying the table it was made against: the commit decides again
        // if the table changed during the drain. An explicit Drop route
        // rejects the bundle here — doomed traffic is never persisted, and
        // the caller's cancel stops its drain mid-stream. Placement after
        // the chain is deliberate: a filter Drop keeps precedence, and the
        // Classifier-supplied routing inputs must precede the lookup.
        let decision = self.rib.decide(&parsed.bundle, metadata);
        if let Some(DispatchAction::Drop(reason)) = decision.action {
            let label = reason.unwrap_or(ReasonCode::NoAdditionalInformation);
            count_received_dropped(&label);
            // Drop-with-reason reports like the sibling gate drops;
            // Drop-without-reason is silent — not even the reception
            // report, which a bundle dropped by dispatch's delete_bundle
            // path has already had.
            match reason {
                Some(reason) => {
                    debug!("Route lookup drops the bundle at the ingress gate: {reason:?}");
                    self.report_bundle_reception(
                        &parsed.bundle,
                        metadata.received_at(),
                        report,
                        Some(reason),
                    )
                    .await;
                }
                None => debug!("Route lookup silently drops the bundle at the ingress gate"),
            }
            return None;
        }

        Some(Admitted { decision, roles })
    }

    // The config-gated RFC 9171 validity checks: policy requirements beyond
    // structural validity, checked pre-drain — everything they read is
    // header material the keyed verify pass has already established. Both
    // input doors apply them (the originate raw door at its own gate).
    pub(super) fn rfc9171_gate_reason(&self, hv: &Headers) -> Option<ReasonCode> {
        // RFC 9171 §4.3.1: "A CRC SHALL be present in the primary block
        // unless the bundle includes a BPSec Block Integrity Block whose
        // target is the primary block".
        if self.primary_block_integrity && !hv.parsed.bundle.primary_has_integrity() {
            debug!("Rejecting bundle: primary block has no integrity protection (no CRC, no BIB)");
            return Some(ReasonCode::BlockUnintelligible);
        }

        None
    }
}
