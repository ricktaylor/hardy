use super::*;
use hardy_bpa::async_trait;
use hardy_bpv7::{
    bpsec, bundle::ParsedBundle, creation_timestamp::CreationTimestamp, eid::Eid,
    status_report::AdministrativeRecord,
};
use hardy_cbor::decode::FromCbor;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

/// Statistics for ping session, following IP ping conventions.
#[derive(Default)]
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

/// Entry for tracking a sent bundle, with timestamp for aging out old entries.
struct SentBundle {
    seqno: u32,
    send_time: time::OffsetDateTime,
}

/// A hop in the bundle's path, discovered via status reports.
struct PathHop {
    node: Eid,
    elapsed: std::time::Duration,
    kind: hardy_bpa::services::StatusNotify,
    /// Whether this hop is from the return-trip (pong) path vs outbound (ping) path.
    is_return_trip: bool,
}

/// Shared mutable state protected by a single mutex.
struct SharedState {
    sent_bundles: HashMap<hardy_bpv7::bundle::Id, SentBundle>,
    expected_responses: HashMap<u32, time::OffsetDateTime>,
    stats: Statistics,
    /// Path hops discovered via status reports, keyed by sequence number.
    path_hops: HashMap<u32, Vec<PathHop>>,
    /// Reverse lookup: creation timestamp -> (seqno, send_time).
    /// Used to match return-trip status reports where bundle_id.source is the echo service.
    creation_to_seqno: HashMap<CreationTimestamp, (u32, time::OffsetDateTime)>,
}

pub struct Service {
    sink: std::sync::OnceLock<Box<dyn hardy_bpa::services::ServiceSink>>,
    node_id: String,
    destination: Eid,
    lifetime: std::time::Duration,
    quiet: bool,
    /// Random ephemeral key for BIB signing/verification (None if --no-sign)
    signing_key: Option<bpsec::key::Key>,
    semaphore: Option<Arc<tokio::sync::Semaphore>>,
    count: Option<u32>,
    state: std::sync::Mutex<SharedState>,
}

impl Service {
    pub fn new(args: &Command) -> Self {
        // Generate random 256-bit key for HMAC-SHA256 if signing enabled
        let signing_key = if args.no_sign {
            None
        } else {
            let mut key_bytes = [0u8; 32];
            rand::rng().fill(&mut key_bytes);

            Some(bpsec::key::Key {
                key_type: bpsec::key::Type::OctetSequence {
                    key: key_bytes.into(),
                },
                key_algorithm: Some(bpsec::key::KeyAlgorithm::HS256),
                operations: Some(HashSet::from([
                    bpsec::key::Operation::Sign,
                    bpsec::key::Operation::Verify,
                ])),
                ..Default::default()
            })
        };

        Self {
            sink: std::sync::OnceLock::new(),
            node_id: args.node_id().unwrap().to_string(),
            destination: args.destination.clone(),
            lifetime: args.lifetime(),
            quiet: args.quiet,
            signing_key,
            count: args.count,
            semaphore: args.count.map(|_| Arc::new(tokio::sync::Semaphore::new(0))),
            state: std::sync::Mutex::new(SharedState {
                sent_bundles: HashMap::new(),
                expected_responses: HashMap::new(),
                stats: Statistics::default(),
                path_hops: HashMap::new(),
                creation_to_seqno: HashMap::new(),
            }),
        }
    }

    /// Remove entries older than bundle lifetime to prevent unbounded memory growth.
    fn cleanup_expired(&self, now: time::OffsetDateTime) {
        let cutoff = now - self.lifetime;
        let mut state = self.state.lock().trace_expect("Failed to lock state mutex");
        state
            .sent_bundles
            .retain(|_, entry| entry.send_time > cutoff);
        state
            .expected_responses
            .retain(|_, send_time| *send_time > cutoff);
        state
            .creation_to_seqno
            .retain(|_, (_, send_time)| *send_time > cutoff);
    }

    /// Print and clear the discovered path for a sequence number.
    /// Shows outbound (ping) and return (pong) paths as a hairpin visualization.
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

        // Separate hops by direction
        let (outbound, return_trip): (Vec<_>, Vec<_>) =
            hops.iter().partition(|h| !h.is_return_trip);

        // Helper to format a path segment
        let format_segment = |hops: &[&PathHop]| -> Vec<String> {
            // Group hops by node
            let mut nodes: Vec<(&Eid, Vec<&PathHop>)> = Vec::new();
            for hop in hops {
                if let Some((_, node_hops)) = nodes.iter_mut().find(|(n, _)| *n == &hop.node) {
                    node_hops.push(hop);
                } else {
                    nodes.push((&hop.node, vec![*hop]));
                }
            }

            // Sort nodes by their earliest status time
            nodes.sort_by_key(|(_, node_hops)| node_hops.iter().map(|h| h.elapsed).min());

            // Format each node's status times
            nodes
                .iter()
                .map(|(node, node_hops)| {
                    let mut statuses: Vec<String> = node_hops
                        .iter()
                        .map(|h| {
                            let abbrev = match h.kind {
                                hardy_bpa::services::StatusNotify::Received => "rcv",
                                hardy_bpa::services::StatusNotify::Forwarded => "fwd",
                                hardy_bpa::services::StatusNotify::Delivered => "dlv",
                                hardy_bpa::services::StatusNotify::Deleted => "del",
                            };
                            format!("{} {}", abbrev, humantime::format_duration(h.elapsed))
                        })
                        .collect();
                    statuses.sort();
                    format!("{} ({})", node, statuses.join(", "))
                })
                .collect()
        };

        // Format outbound path (ping)
        let outbound_refs: Vec<&PathHop> = outbound.into_iter().collect();
        let outbound_nodes = format_segment(&outbound_refs);

        // Format return path (pong), reversed for right-to-left display
        let return_refs: Vec<&PathHop> = return_trip.into_iter().collect();
        let mut return_nodes = format_segment(&return_refs);
        return_nodes.reverse();

        // Print hairpin visualization
        if !outbound_nodes.is_empty() && !return_nodes.is_empty() {
            // Both paths - show hairpin
            let outbound_str = outbound_nodes.join(" -> ");
            let return_str = return_nodes.join(" <- ");
            println!("  path: {} \u{2500}\u{2510}", outbound_str); // ─┐
            println!("        {} <\u{2518}", return_str); // <┘
        } else if !outbound_nodes.is_empty() {
            // Only outbound
            println!("  path: {}", outbound_nodes.join(" -> "));
        } else if !return_nodes.is_empty() {
            // Only return
            println!("  path: {}", return_nodes.join(" <- "));
        }
    }

    /// Sign the payload block with BIB-HMAC-SHA2.
    fn sign_bundle(
        &self,
        bundle_bytes: &[u8],
        args: &Command,
        key: &bpsec::key::Key,
    ) -> anyhow::Result<Box<[u8]>> {
        // Parse the bundle to get access to block structure
        let parsed = ParsedBundle::parse(bundle_bytes, bpsec::no_keys)
            .map_err(|e| anyhow::anyhow!("Failed to parse bundle for signing: {e}"))?;

        let source = args.source.clone().unwrap();

        // Sign the payload block (block number 1)
        // Exclude primary block from scope - the echo service modifies it
        // (swapping source/destination). Include target and security headers.
        let scope = bpsec::rfc9173::ScopeFlags {
            include_primary_block: false,
            include_target_header: true,
            include_security_header: true,
            unrecognised: None,
        };
        let signed_bytes = bpsec::signer::Signer::new(&parsed.bundle, bundle_bytes)
            .sign_block(
                1, // payload block
                bpsec::signer::Context::HMAC_SHA2(scope),
                source,
                key,
            )
            .map_err(|(_, e)| anyhow::anyhow!("Failed to sign payload: {e}"))?
            .rebuild()
            .map_err(|e| anyhow::anyhow!("Failed to rebuild signed bundle: {e}"))?;

        Ok(signed_bytes)
    }

    /// Print summary statistics in IP ping format.
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

        // Show last-seen info for lost bundles (remaining entries in path_hops)
        let remaining_hops = &state.path_hops;

        if !remaining_hops.is_empty() {
            println!();
            println!("Lost bundles last seen:");

            let mut seqnos: Vec<_> = remaining_hops.keys().copied().collect();
            seqnos.sort();

            for seqno in seqnos {
                if let Some(hops) = remaining_hops.get(&seqno) {
                    // Find the latest status report for this bundle
                    if let Some(last_hop) = hops.iter().max_by_key(|h| h.elapsed) {
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
        }
    }

    pub async fn send(&self, args: &Command, seq_no: u32) -> anyhow::Result<()> {
        // build_payload returns raw bundle bytes and creation timestamp
        let (bundle_bytes, creation_timestamp) = ping::payload::build_payload(args, seq_no)?;

        // Sign the bundle if signing is enabled
        let bundle_bytes = if let Some(ref key) = self.signing_key {
            self.sign_bundle(&bundle_bytes, args, key)?
        } else {
            bundle_bytes
        };

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
            // Store reverse lookup for matching return-trip status reports
            state
                .creation_to_seqno
                .insert(creation_timestamp, (seq_no, send_time));
            state.expected_responses.insert(seq_no, send_time);
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

    async fn on_receive(&self, data: hardy_bpa::Bytes, _expiry: time::OffsetDateTime) {
        // Record receive time immediately for accurate RTT
        let receive_time = time::OffsetDateTime::now_utc();

        // Parse the raw bundle, verifying BIB if we have a signing key
        let (bundle, corrupted) = if let Some(ref key) = self.signing_key {
            // Create a KeySet that provides our key for verification
            let key_set = bpsec::key::KeySet::new(vec![key.clone()]);
            match hardy_bpv7::bundle::ParsedBundle::parse_with_keys(&data, &key_set) {
                Ok(b) => (b.bundle, false),
                Err(e) => {
                    // Check if this is a BIB verification failure (integrity check failed)
                    let is_integrity_failure = matches!(
                        e,
                        hardy_bpv7::Error::InvalidBPSec(bpsec::Error::IntegrityCheckFailed)
                    );
                    if is_integrity_failure {
                        // Try parsing without keys to get basic bundle info for logging
                        match hardy_bpv7::bundle::ParsedBundle::parse(&data, bpsec::no_keys) {
                            Ok(b) => (b.bundle, true),
                            Err(e2) => {
                                eprintln!("Failed to parse corrupted bundle: {e2}");
                                return;
                            }
                        }
                    } else {
                        eprintln!("Failed to parse bundle: {e}");
                        return;
                    }
                }
            }
        } else {
            // No signing key - parse without verification
            match hardy_bpv7::bundle::ParsedBundle::parse(&data, bpsec::no_keys) {
                Ok(b) => (b.bundle, false),
                Err(e) => {
                    eprintln!("Failed to parse bundle: {e}");
                    return;
                }
            }
        };

        // Extract payload from payload block (block number 1)
        let payload_block = match bundle.blocks.get(&1) {
            Some(b) => b,
            None => {
                eprintln!("Bundle has no payload block");
                return;
            }
        };
        let payload_data = match payload_block.payload(&data) {
            Some(p) => p,
            None => {
                eprintln!("Bundle has no payload data");
                return;
            }
        };

        // Check if this is an admin record (status report)
        // Must check BEFORE source validation - status reports come from intermediate nodes
        if bundle.flags.is_admin_record {
            self.handle_status_report(payload_data, &bundle.id.source);
            return;
        }

        // For ping responses (not admin records), verify source is the echo destination
        if bundle.id.source != self.destination {
            eprintln!(
                "Ignoring bundle from unexpected source EID '{}'",
                bundle.id.source
            );
            return;
        }

        // Parse CBOR payload
        let payload = match payload::Payload::from_cbor(payload_data) {
            Ok((p, _, _)) => p,
            Err(e) => {
                eprintln!("Failed to parse ping payload: {e}");
                return;
            }
        };

        // Handle corrupted bundles
        if corrupted {
            eprintln!(
                "WARNING: Ping {} integrity check FAILED - payload corrupted!",
                payload.seqno
            );
            {
                let mut state = self.state.lock().trace_expect("Failed to lock state mutex");
                state.stats.corrupted += 1;
                // Remove from expected responses but don't count in RTT stats
                state.expected_responses.remove(&payload.seqno);
            }

            // Still signal response received for count-based termination
            if let Some(semaphore) = &self.semaphore {
                semaphore.add_permits(1);
            }
            return;
        }

        let sent_time = self
            .state
            .lock()
            .trace_expect("Failed to lock state mutex")
            .expected_responses
            .remove(&payload.seqno);
        let Some(sent_time) = sent_time else {
            eprintln!(
                "Ignoring unexpected ping response with sequence number {}",
                payload.seqno
            );
            return;
        };

        // Calculate RTT using LOCAL timestamps (not payload timestamps)
        // This avoids clock synchronization issues between nodes
        if let Ok(rtt) = (receive_time - sent_time).try_into() {
            // Record statistics
            self.state
                .lock()
                .trace_expect("Failed to lock state mutex")
                .stats
                .record_rtt(rtt);

            if !self.quiet {
                println!(
                    "Reply from {}: seq={} rtt={}",
                    bundle.id.source,
                    payload.seqno,
                    humantime::format_duration(rtt)
                );
            }
            // Print path (also cleans up path_hops entry)
            self.print_path(payload.seqno);
        } else {
            eprintln!(
                "Failed to compute round-trip time for ping {}",
                payload.seqno
            );
        }

        // Indicate that we have received a response
        if let Some(semaphore) = &self.semaphore {
            semaphore.add_permits(1);
        }
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

impl Service {
    /// Handle incoming status reports (admin records).
    fn handle_status_report(&self, payload_data: &[u8], from: &Eid) {
        use hardy_bpv7::status_report::ReasonCode;

        // Parse as status report
        let report = match hardy_cbor::decode::parse::<AdministrativeRecord>(payload_data) {
            Ok(AdministrativeRecord::BundleStatusReport(r)) => r,
            _ => return,
        };

        // Lookup seqno/send_time - try bundle_id first (outbound), then creation_timestamp (return-trip)
        let (seqno, send_time, is_return_trip) = {
            let state = self.state.lock().trace_expect("Failed to lock state mutex");
            if let Some(e) = state.sent_bundles.get(&report.bundle_id) {
                (e.seqno, e.send_time, false)
            } else if let Some(&(seq, time)) =
                state.creation_to_seqno.get(&report.bundle_id.timestamp)
            {
                (seq, time, true)
            } else {
                if !self.quiet {
                    eprintln!("Spurious status report received!");
                }
                return;
            }
        };

        // Process each assertion with same output format as before
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

            let direction = if is_return_trip { "Pong" } else { "Ping" };
            let mut output = format!("{direction} {seqno}");

            match kind {
                hardy_bpa::services::StatusNotify::Received => output.push_str(" received"),
                hardy_bpa::services::StatusNotify::Forwarded => output.push_str(" forwarded"),
                hardy_bpa::services::StatusNotify::Delivered => output.push_str(" delivered"),
                hardy_bpa::services::StatusNotify::Deleted => {
                    output.push_str(" deleted");
                    if let Some(semaphore) = &self.semaphore {
                        semaphore.add_permits(1);
                    }
                }
            }

            if from.to_string() != self.node_id {
                output = format!("{output} by {from}");
            } else {
                output.push_str(" locally");
            }

            if !matches!(report.reason, ReasonCode::NoAdditionalInformation) {
                output = format!("{output}, {:?},", report.reason);
            }

            let report_time = assertion.0.unwrap_or_else(time::OffsetDateTime::now_utc);
            if let Ok(elapsed) = (report_time - send_time).try_into() {
                let elapsed: std::time::Duration = elapsed;
                output = format!("{output} after {}", humantime::format_duration(elapsed));

                // Track hops for path display (exclude local node)
                if from.to_string() != self.node_id {
                    let mut state = self.state.lock().trace_expect("Failed to lock state mutex");
                    state.path_hops.entry(seqno).or_default().push(PathHop {
                        node: from.clone(),
                        elapsed,
                        kind,
                        is_return_trip,
                    });
                }
            }

            if !self.quiet {
                println!("{output}");
            }
        }
    }
}
