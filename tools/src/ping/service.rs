use super::*;
use hardy_bpa::async_trait;
use hardy_bpv7::{eid::Eid, status_report::AdministrativeRecord};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

// Statistics for ping session, following IP ping conventions.
#[derive(Default, Clone)]
pub struct Statistics {
    pub sent: u32,
    pub received: u32,
    pub corrupted: u32,
    pub min_rtt: Option<std::time::Duration>,
    pub max_rtt: Option<std::time::Duration>,
    sum_rtt_us: u128,         // Microseconds for precision
    sum_rtt_squared_us: u128, // For stddev calculation
}

impl Statistics {
    pub fn record_rtt(&mut self, rtt: std::time::Duration) {
        self.received += 1;
        let rtt_us = rtt.as_micros();
        self.sum_rtt_us += rtt_us;
        self.sum_rtt_squared_us += rtt_us * rtt_us;
        self.min_rtt = Some(self.min_rtt.map_or(rtt, |min| min.min(rtt)));
        self.max_rtt = Some(self.max_rtt.map_or(rtt, |max| max.max(rtt)));
    }

    pub fn avg_rtt(&self) -> Option<std::time::Duration> {
        if self.received > 0 {
            Some(std::time::Duration::from_micros(
                (self.sum_rtt_us / self.received as u128) as u64,
            ))
        } else {
            None
        }
    }

    pub fn stddev_rtt(&self) -> Option<std::time::Duration> {
        if self.received > 1 {
            let n = self.received as u128;
            let mean = self.sum_rtt_us / n;
            let variance = (self.sum_rtt_squared_us / n).saturating_sub(mean * mean);
            let stddev_us = (variance as f64).sqrt() as u64;
            Some(std::time::Duration::from_micros(stddev_us))
        } else {
            None
        }
    }

    pub fn loss_percent(&self) -> f64 {
        if self.sent > 0 {
            ((self.sent - self.received) as f64 / self.sent as f64) * 100.0
        } else {
            0.0
        }
    }
}

// Entry for tracking a sent request bundle, keyed by its bundle-id, so that
// forward-leg ("Ping") status reports can be matched to a sequence number.
struct SentBundle {
    seqno: u32,
    send_time: time::OffsetDateTime,
}

// A hop on the outbound (Ping) path for one sequence number, from a status
// report; elapsed is measured from when that request was sent.
struct PathHop {
    node: Eid,
    elapsed: std::time::Duration,
    kind: hardy_bpa::services::StatusNotify,
}

// A hop on the return (Pong) path of one echo response bundle. The client never
// sent the response, so there is no send time to measure against — the absolute
// report time is kept and rendered relative to the first report for that bundle.
struct ReturnHop {
    node: Eid,
    kind: hardy_bpa::services::StatusNotify,
    report_time: time::OffsetDateTime,
}

// Shared mutable state protected by a single mutex.
struct SharedState {
    sent_bundles: HashMap<hardy_bpv7::bundle::Id, SentBundle>,
    // Sequence number -> (send time, the payload bytes we sent). The send time
    // drives the RTT calculation; the payload bytes are compared against the
    // reflected payload for round-trip integrity.
    expected_responses: HashMap<u32, (time::OffsetDateTime, Box<[u8]>)>,
    stats: Statistics,
    // Outbound (Ping) path hops, keyed by sequence number.
    path_hops: HashMap<u32, Vec<PathHop>>,
    // Sequence numbers that received a reply (to filter the "lost" display).
    replied: HashSet<u32>,
    // Sequence numbers already counted towards count-based termination. A single
    // ping generates several status reports (delivery is always followed by a
    // deletion, on both legs), so this guards each ping to exactly one permit.
    resolved: HashSet<u32>,
    // Return (Pong) journeys, keyed by the echo response bundle-id (echo source
    // + creation timestamp). A status report carries no echoed sequence number,
    // so return-leg reports can only be grouped per response bundle, not per ping.
    return_journeys: HashMap<hardy_bpv7::bundle::Id, Vec<ReturnHop>>,
}

pub struct Service {
    sink: std::sync::OnceLock<Box<dyn hardy_bpa::services::ServiceSink>>,
    local_node: NodeId,
    destination: Eid,
    lifetime: std::time::Duration,
    quiet: bool,
    semaphore: Option<Arc<tokio::sync::Semaphore>>,
    count: Option<u32>,
    state: std::sync::Mutex<SharedState>,
}

impl Service {
    pub fn new(args: &Command) -> Self {
        Self {
            sink: std::sync::OnceLock::new(),
            local_node: args.node_id().unwrap(),
            destination: args.destination.clone(),
            lifetime: args.lifetime(),
            quiet: args.quiet,
            count: args.count,
            semaphore: args.count.map(|_| Arc::new(tokio::sync::Semaphore::new(0))),
            state: std::sync::Mutex::new(SharedState {
                sent_bundles: HashMap::new(),
                expected_responses: HashMap::new(),
                stats: Statistics::default(),
                path_hops: HashMap::new(),
                replied: HashSet::new(),
                resolved: HashSet::new(),
                return_journeys: HashMap::new(),
            }),
        }
    }

    // Remove entries older than bundle lifetime to prevent unbounded memory growth.
    fn cleanup_expired(&self, now: time::OffsetDateTime) {
        let cutoff = now - self.lifetime;
        let mut expired = Vec::new();
        {
            let mut state = self.state.lock().trace_expect("Failed to lock state mutex");
            state
                .sent_bundles
                .retain(|_, entry| entry.send_time > cutoff);
            state.expected_responses.retain(|seqno, (send_time, _)| {
                if *send_time > cutoff {
                    true
                } else {
                    expired.push(*seqno);
                    false
                }
            });
            state
                .return_journeys
                .retain(|_, hops| hops.iter().any(|h| h.report_time > cutoff));
        }

        // Once its tracking entries are gone, nothing can resolve an expired
        // ping — a late reply or status report no longer matches anything — so
        // resolve it here, or count-based termination would sit out the full
        // wait timeout.
        for seqno in expired {
            self.resolve(seqno);
        }
    }

    // Print and clear the discovered outbound (Ping) path for a sequence number.
    fn print_path(&self, seqno: u32) {
        let hops = self
            .state
            .lock()
            .trace_expect("Failed to lock state mutex")
            .path_hops
            .remove(&seqno)
            .unwrap_or_default();

        if hops.is_empty() || self.quiet {
            return;
        }

        // Group hops by node, preserving discovery order.
        let mut nodes: Vec<(&Eid, Vec<&PathHop>)> = Vec::new();
        for hop in &hops {
            if let Some((_, node_hops)) = nodes.iter_mut().find(|(n, _)| *n == &hop.node) {
                node_hops.push(hop);
            } else {
                nodes.push((&hop.node, vec![hop]));
            }
        }

        // Sort nodes by their earliest status time.
        nodes.sort_by_key(|(_, node_hops)| node_hops.iter().map(|h| h.elapsed).min());

        let segments: Vec<String> = nodes
            .iter()
            .map(|(node, node_hops)| {
                let mut statuses: Vec<String> = node_hops
                    .iter()
                    .map(|h| {
                        format!(
                            "{} {}",
                            abbrev(h.kind),
                            humantime::format_duration(h.elapsed)
                        )
                    })
                    .collect();
                statuses.sort();
                format!("{} ({})", node, statuses.join(", "))
            })
            .collect();

        println!("  path: {}", segments.join(" -> "));
    }

    // Get a copy of the current statistics.
    pub fn statistics(&self) -> Statistics {
        self.state
            .lock()
            .trace_expect("Failed to lock state mutex")
            .stats
            .clone()
    }

    // Print summary statistics in IP ping format.
    pub fn print_summary(&self) {
        let state = self.state.lock().trace_expect("Failed to lock state mutex");

        println!();
        println!("--- {} ping statistics ---", self.destination);

        if state.stats.corrupted > 0 {
            println!(
                "{} bundles transmitted, {} received, {} corrupted, {:.0}% loss",
                state.stats.sent,
                state.stats.received,
                state.stats.corrupted,
                state.stats.loss_percent()
            );
        } else {
            println!(
                "{} bundles transmitted, {} received, {:.0}% loss",
                state.stats.sent,
                state.stats.received,
                state.stats.loss_percent()
            );
        }

        if let (Some(min), Some(avg), Some(max)) = (
            state.stats.min_rtt,
            state.stats.avg_rtt(),
            state.stats.max_rtt,
        ) {
            let stddev = state.stats.stddev_rtt().unwrap_or_default();
            println!(
                "rtt min/avg/max/stddev = {}/{}/{}/{}",
                humantime::format_duration(min),
                humantime::format_duration(avg),
                humantime::format_duration(max),
                humantime::format_duration(stddev),
            );
        }

        // Last-seen info for lost bundles — those with outbound status reports
        // but no reply.
        let mut lost_seqnos: Vec<_> = state
            .path_hops
            .keys()
            .filter(|seqno| !state.replied.contains(seqno))
            .copied()
            .collect();
        lost_seqnos.sort();

        if !lost_seqnos.is_empty() {
            println!();
            println!("Lost bundles last seen:");
            for seqno in lost_seqnos {
                if let Some(hops) = state.path_hops.get(&seqno)
                    && let Some(last_hop) = hops.iter().max_by_key(|h| h.elapsed)
                {
                    let status = match last_hop.kind {
                        hardy_bpa::services::StatusNotify::Received => "received by",
                        hardy_bpa::services::StatusNotify::Forwarded => "forwarded by",
                        hardy_bpa::services::StatusNotify::Delivered => "delivered to",
                        hardy_bpa::services::StatusNotify::Deleted => "deleted by",
                    };
                    println!(
                        "  seq={} {} {} after {}",
                        seqno,
                        status,
                        last_hop.node,
                        humantime::format_duration(last_hop.elapsed)
                    );
                }
            }
        }

        // Return (Pong) journeys: status reports about the echo's response
        // bundles. No sequence number is recoverable, so each response bundle is
        // listed in the order first observed, with times relative to its first
        // report.
        if !state.return_journeys.is_empty() {
            println!();
            println!("Return journeys (from {}):", self.destination);
            let mut journeys: Vec<&Vec<ReturnHop>> = state.return_journeys.values().collect();
            journeys.sort_by_key(|hops| hops.iter().map(|h| h.report_time).min());
            for (n, hops) in journeys.iter().enumerate() {
                let t0 = hops.iter().map(|h| h.report_time).min();
                let mut sorted: Vec<&ReturnHop> = hops.iter().collect();
                sorted.sort_by_key(|h| h.report_time);
                let segments: Vec<String> = sorted
                    .iter()
                    .map(
                        |h| match t0.and_then(|t0| (h.report_time - t0).try_into().ok()) {
                            Some(elapsed) => {
                                let elapsed: std::time::Duration = elapsed;
                                format!(
                                    "{} {} ({})",
                                    abbrev(h.kind),
                                    h.node,
                                    humantime::format_duration(elapsed)
                                )
                            }
                            None => format!("{} {}", abbrev(h.kind), h.node),
                        },
                    )
                    .collect();
                println!("  response {}: {}", n + 1, segments.join(" -> "));
            }
        }
    }

    pub async fn send(&self, args: &Command, seq_no: u32) -> anyhow::Result<()> {
        let (bundle_bytes, payload_bytes) = ping::payload::build_payload(args, seq_no)?;

        if !self.quiet {
            eprintln!("Sending ping {seq_no}...");
        }

        // Get sink first (may block on OnceLock initialization)
        let sink = self.sink.wait();

        // Record send time immediately before the actual send for accurate RTT
        let send_time = time::OffsetDateTime::now_utc();

        let id = sink
            .send(bundle_bytes.into())
            .await
            .map_err(|e| anyhow::anyhow!("Failed to send bundle: {e}"))?;

        {
            let mut state = self.state.lock().trace_expect("Failed to lock state mutex");
            state.sent_bundles.insert(
                id,
                SentBundle {
                    seqno: seq_no,
                    send_time,
                },
            );
            state
                .expected_responses
                .insert(seq_no, (send_time, payload_bytes));
            state.stats.sent += 1;
        }

        // Periodically clean up expired entries to bound memory usage
        self.cleanup_expired(send_time);

        Ok(())
    }

    pub async fn wait_for_responses(&self, cancel_token: &tokio_util::sync::CancellationToken) {
        if let Some(semaphore) = &self.semaphore
            && let Some(count) = self.count
        {
            tokio::select! {
                _ = cancel_token.cancelled() => {},
                r = semaphore.acquire_many(count) => {
                    _ = r.trace_expect("Failed to acquire semaphore permits");
                },
            };
        }
    }

    // Mark a ping resolved for count-based termination, releasing its single
    // permit the first time. A ping is resolved by its reply, a payload
    // mismatch, or an outbound-leg deletion; every subsequent status report for
    // the same sequence number (notably the deletion that always follows local
    // delivery) is a no-op. Does nothing outside count mode, where the set is
    // bounded by the ping count.
    fn resolve(&self, seqno: u32) {
        let Some(semaphore) = &self.semaphore else {
            return;
        };
        let newly_resolved = self
            .state
            .lock()
            .trace_expect("Failed to lock state mutex")
            .resolved
            .insert(seqno);
        if newly_resolved {
            semaphore.add_permits(1);
        }
    }

    // Whether a status report was sourced by this client's own node.
    fn is_local_report(&self, report_source: &Eid) -> bool {
        report_source
            .to_node_id()
            .is_ok_and(|node| node == self.local_node)
    }

    // Handle an incoming status report (administrative record).
    fn handle_status_report(&self, payload_data: &[u8], report_source: &Eid) {
        use hardy_bpv7::status_report::ReasonCode;

        let report = match hardy_cbor::decode::parse_exact::<AdministrativeRecord>(payload_data) {
            Ok(AdministrativeRecord::BundleStatusReport(r)) => r,
            _ => return,
        };

        // A status report identifies its subject only by bundle-id (source +
        // creation timestamp) — there is no echoed sequence number. Classify by
        // subject: a forward-leg ("Ping") report is about one of our own request
        // bundles (known id -> seqno); a return-leg ("Pong") report is about an
        // echo response bundle (source == the echo endpoint) and cannot be tied
        // to a sequence number, so it is grouped per response bundle-id.
        enum Leg {
            Forward {
                seqno: u32,
                send_time: time::OffsetDateTime,
            },
            Return,
        }
        let leg = {
            let state = self.state.lock().trace_expect("Failed to lock state mutex");
            if let Some(entry) = state.sent_bundles.get(&report.bundle_id) {
                Leg::Forward {
                    seqno: entry.seqno,
                    send_time: entry.send_time,
                }
            } else if same_endpoint(&report.bundle_id.source, &self.destination) {
                Leg::Return
            } else {
                if !self.quiet {
                    eprintln!("Spurious status report received!");
                }
                return;
            }
        };

        for (kind, assertion) in [
            (
                hardy_bpa::services::StatusNotify::Received,
                &report.received,
            ),
            (
                hardy_bpa::services::StatusNotify::Forwarded,
                &report.forwarded,
            ),
            (
                hardy_bpa::services::StatusNotify::Delivered,
                &report.delivered,
            ),
            (hardy_bpa::services::StatusNotify::Deleted, &report.deleted),
        ] {
            let Some(assertion) = assertion else { continue };

            let report_time = assertion.0.unwrap_or_else(time::OffsetDateTime::now_utc);
            let location = if !self.is_local_report(report_source) {
                format!(" by {report_source}")
            } else {
                " locally".to_string()
            };
            let reason = if matches!(report.reason, ReasonCode::NoAdditionalInformation) {
                String::new()
            } else {
                format!(", {:?},", report.reason)
            };

            match &leg {
                Leg::Forward { seqno, send_time } => {
                    let elapsed: Option<std::time::Duration> =
                        (report_time - *send_time).try_into().ok();
                    let after = elapsed
                        .map(|e| format!(" after {}", humantime::format_duration(e)))
                        .unwrap_or_default();
                    if !self.quiet {
                        println!("Ping {seqno} {}{location}{reason}{after}", verb(kind));
                    }
                    // Track non-local hops for the per-seqno outbound path.
                    if let Some(elapsed) = elapsed
                        && !self.is_local_report(report_source)
                    {
                        self.state
                            .lock()
                            .trace_expect("Failed to lock state mutex")
                            .path_hops
                            .entry(*seqno)
                            .or_default()
                            .push(PathHop {
                                node: report_source.clone(),
                                elapsed,
                                kind,
                            });
                    }
                    // A deletion on the outbound leg ends this ping: it either
                    // never reached the echo, or was delivered and cleaned up.
                    // Either way no further reply is expected for this seqno.
                    if matches!(kind, hardy_bpa::services::StatusNotify::Deleted) {
                        self.resolve(*seqno);
                    }
                }
                Leg::Return => {
                    // Return-leg reports carry no echoed sequence number, so they
                    // never resolve a ping for count-based termination; the
                    // outbound leg always does that. They are recorded only for
                    // the return-journey display.
                    if !self.quiet {
                        println!(
                            "Pong (from {}) {}{location}{reason}",
                            self.destination,
                            verb(kind)
                        );
                    }
                    self.state
                        .lock()
                        .trace_expect("Failed to lock state mutex")
                        .return_journeys
                        .entry(report.bundle_id.clone())
                        .or_default()
                        .push(ReturnHop {
                            node: report_source.clone(),
                            kind,
                            report_time,
                        });
                }
            }
        }
    }
}

// Endpoint-identity comparison for parsed EIDs. Structural `Eid` equality
// keeps the legacy two-element and three-element `ipn` encodings of the same
// endpoint (RFC 9758) as distinct variants; comparing (node-id, service)
// collapses that. Anything else that differs — including dtn demux strings —
// is a genuinely different endpoint and does not match.
fn same_endpoint(a: &Eid, b: &Eid) -> bool {
    a == b
        || match (a.to_node_id(), b.to_node_id()) {
            (Ok(a_node), Ok(b_node)) => a_node == b_node && a.service() == b.service(),
            _ => false,
        }
}

// Short label for a status kind, used in path displays.
fn abbrev(kind: hardy_bpa::services::StatusNotify) -> &'static str {
    match kind {
        hardy_bpa::services::StatusNotify::Received => "rcv",
        hardy_bpa::services::StatusNotify::Forwarded => "fwd",
        hardy_bpa::services::StatusNotify::Delivered => "dlv",
        hardy_bpa::services::StatusNotify::Deleted => "del",
    }
}

// Verb for a status kind, used in per-report lines.
fn verb(kind: hardy_bpa::services::StatusNotify) -> &'static str {
    match kind {
        hardy_bpa::services::StatusNotify::Received => "received",
        hardy_bpa::services::StatusNotify::Forwarded => "forwarded",
        hardy_bpa::services::StatusNotify::Delivered => "delivered",
        hardy_bpa::services::StatusNotify::Deleted => "deleted",
    }
}

#[async_trait]
impl hardy_bpa::services::Service for Service {
    async fn on_register(&self, _endpoint: &Eid, sink: Box<dyn hardy_bpa::services::ServiceSink>) {
        // Ensure single initialization
        self.sink.get_or_init(|| sink);
    }

    async fn on_unregister(&self) {
        // Nothing to do
    }

    async fn on_receive(
        &self,
        data: hardy_bpa::Bytes,
        _expiry: time::OffsetDateTime,
    ) -> hardy_bpa::services::Result<()> {
        // Record receive time immediately for accurate RTT
        let receive_time = time::OffsetDateTime::now_utc();

        // Parse the bundle structurally (the client does not use BPSec).
        let hardy_bpv7::parse::Parsed { data, bundle, .. } = match hardy_bpv7::parse::parse(data) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("Failed to parse bundle: {e}");
                return Ok(());
            }
        };

        // Extract the payload block contents.
        let Some(payload_data) = bundle.blocks.get(&1).and_then(|b| b.payload(&data)) else {
            eprintln!("Bundle has no payload");
            return Ok(());
        };

        // Status reports arrive as administrative records; handle before the
        // source check, since they originate at intermediate nodes.
        if bundle.primary.flags.is_admin_record {
            self.handle_status_report(payload_data, &bundle.primary.id.source);
            return Ok(());
        }

        // A ping response must be sourced by the echo endpoint we pinged.
        if !same_endpoint(&bundle.primary.id.source, &self.destination) {
            eprintln!(
                "Ignoring bundle from unexpected source EID '{}'",
                bundle.primary.id.source
            );
            return Ok(());
        }

        // Parse our sequence number out of the reflected payload.
        let payload = match payload::Payload::parse(payload_data) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("Failed to parse ping payload: {e}");
                return Ok(());
            }
        };

        // Look up the request we sent for this sequence number: the send time
        // (for RTT) and the payload we sent (for the integrity check).
        let expected = self
            .state
            .lock()
            .trace_expect("Failed to lock state mutex")
            .expected_responses
            .remove(&payload.seqno);
        let Some((sent_time, sent_payload)) = expected else {
            eprintln!(
                "Ignoring unexpected ping response with sequence number {}",
                payload.seqno
            );
            return Ok(());
        };

        // Integrity: the echo reflects the payload byte-for-byte, so the
        // returned payload must equal what we sent. No BPSec required, and this
        // works against any conformant echo.
        if payload_data != sent_payload.as_ref() {
            eprintln!(
                "WARNING: Ping {} payload mismatch — response was modified in transit!",
                payload.seqno
            );
            self.state
                .lock()
                .trace_expect("Failed to lock state mutex")
                .stats
                .corrupted += 1;
            self.resolve(payload.seqno);
            return Ok(());
        }

        // Calculate RTT using LOCAL timestamps (not payload timestamps) to avoid
        // clock synchronization issues between nodes.
        if let Ok(rtt) = (receive_time - sent_time).try_into() {
            {
                let mut state = self.state.lock().trace_expect("Failed to lock state mutex");
                state.stats.record_rtt(rtt);
                state.replied.insert(payload.seqno);
            }

            if !self.quiet {
                println!(
                    "Reply from {}: seq={} rtt={}",
                    bundle.primary.id.source,
                    payload.seqno,
                    humantime::format_duration(rtt)
                );
            }
            self.print_path(payload.seqno);
        } else {
            eprintln!(
                "Failed to compute round-trip time for ping {}",
                payload.seqno
            );
        }

        // This ping is resolved; release its permit for count-based termination.
        self.resolve(payload.seqno);
        Ok(())
    }

    async fn on_status_notify(
        &self,
        _bundle_id: &hardy_bpv7::bundle::Id,
        _from: &hardy_bpv7::eid::Eid,
        _kind: hardy_bpa::services::StatusNotify,
        _reason: hardy_bpv7::status_report::ReasonCode,
        _timestamp: Option<time::OffsetDateTime>,
    ) {
        // Status reports arrive via on_receive when report_to = service EID
    }
}
