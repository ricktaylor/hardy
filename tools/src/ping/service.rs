use super::*;
use hardy_bpa::async_trait;
use hardy_bpv7::{bpsec, bundle::ParsedBundle, eid::Eid};
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
    sent_bundles: std::sync::Mutex<HashMap<hardy_bpv7::bundle::Id, SentBundle>>,
    expected_responses: std::sync::Mutex<HashMap<u32, time::OffsetDateTime>>,
    stats: std::sync::Mutex<Statistics>,
    /// Path hops discovered via status reports, keyed by sequence number.
    path_hops: std::sync::Mutex<HashMap<u32, Vec<PathHop>>>,
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
            sent_bundles: std::sync::Mutex::new(HashMap::new()),
            expected_responses: std::sync::Mutex::new(HashMap::new()),
            stats: std::sync::Mutex::new(Statistics::default()),
            path_hops: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Remove entries older than bundle lifetime to prevent unbounded memory growth.
    fn cleanup_expired(&self, now: time::OffsetDateTime) {
        let cutoff = now - self.lifetime;

        self.sent_bundles
            .lock()
            .trace_expect("Failed to lock sent_bundles mutex")
            .retain(|_, entry| entry.send_time > cutoff);

        self.expected_responses
            .lock()
            .trace_expect("Failed to lock expected_responses mutex")
            .retain(|_, send_time| *send_time > cutoff);
    }

    /// Print and clear the discovered path for a sequence number.
    fn print_path(&self, seqno: u32) {
        let hops = self
            .path_hops
            .lock()
            .trace_expect("Failed to lock path_hops mutex")
            .remove(&seqno)
            .unwrap_or_default();

        if hops.is_empty() || self.quiet {
            return;
        }

        // Group hops by node, preserving all status types
        let mut nodes: Vec<(&Eid, Vec<&PathHop>)> = Vec::new();
        for hop in &hops {
            if let Some((_, node_hops)) = nodes.iter_mut().find(|(n, _)| *n == &hop.node) {
                node_hops.push(hop);
            } else {
                nodes.push((&hop.node, vec![hop]));
            }
        }

        // Sort nodes by their earliest status time
        nodes.sort_by_key(|(_, node_hops)| node_hops.iter().map(|h| h.elapsed).min());

        // Format each node's status times
        let path_str: Vec<String> = nodes
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
                statuses.sort(); // Alphabetical: dlv, fwd, rcv
                format!("{} ({})", node, statuses.join(", "))
            })
            .collect();

        println!("  path: {}", path_str.join(" -> "));
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
        let stats = self.stats.lock().trace_expect("Failed to lock stats mutex");

        println!();
        println!("--- {} ping statistics ---", self.destination);

        if stats.corrupted > 0 {
            println!(
                "{} bundles transmitted, {} received, {} corrupted, {:.0}% loss",
                stats.sent,
                stats.received,
                stats.corrupted,
                stats.loss_percent()
            );
        } else {
            println!(
                "{} bundles transmitted, {} received, {:.0}% loss",
                stats.sent,
                stats.received,
                stats.loss_percent()
            );
        }

        if let (Some(min), Some(avg), Some(max)) = (stats.min_rtt, stats.avg_rtt(), stats.max_rtt) {
            let stddev = stats.stddev_rtt().unwrap_or_default();
            println!(
                "rtt min/avg/max/stddev = {}/{}/{}/{}",
                humantime::format_duration(min),
                humantime::format_duration(avg),
                humantime::format_duration(max),
                humantime::format_duration(stddev),
            );
        }

        // Show last-seen info for lost bundles (remaining entries in path_hops)
        let remaining_hops = self
            .path_hops
            .lock()
            .trace_expect("Failed to lock path_hops mutex");

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
        // build_payload returns raw bundle bytes ready to send
        let (bundle_bytes, _creation) = ping::payload::build_payload(args, seq_no)?;

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

        self.sent_bundles
            .lock()
            .trace_expect("Failed to lock sent_bundles mutex")
            .insert(
                id,
                SentBundle {
                    seqno: seq_no,
                    send_time,
                },
            );

        self.expected_responses
            .lock()
            .trace_expect("Failed to lock expected_responses mutex")
            .insert(seq_no, send_time);

        self.stats
            .lock()
            .trace_expect("Failed to lock stats mutex")
            .sent += 1;

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

        if bundle.id.source != self.destination {
            // Ignore spurious responses
            eprintln!(
                "Ignoring bundle from unexpected source EID '{}'",
                bundle.id.source
            );
            return;
        }

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
            self.stats
                .lock()
                .trace_expect("Failed to lock stats mutex")
                .corrupted += 1;

            // Remove from expected responses but don't count in RTT stats
            self.expected_responses
                .lock()
                .trace_expect("Failed to lock expected_responses mutex")
                .remove(&payload.seqno);

            // Still signal response received for count-based termination
            if let Some(semaphore) = &self.semaphore {
                semaphore.add_permits(1);
            }
            return;
        }

        let sent_time = self
            .expected_responses
            .lock()
            .trace_expect("Failed to lock expected_responses mutex")
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
            self.stats
                .lock()
                .trace_expect("Failed to lock stats mutex")
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
        bundle_id: &hardy_bpv7::bundle::Id,
        from: &hardy_bpv7::eid::Eid,
        kind: hardy_bpa::services::StatusNotify,
        reason: hardy_bpv7::status_report::ReasonCode,
        timestamp: Option<time::OffsetDateTime>,
    ) {
        // Get entry while holding lock briefly
        let entry_data = self
            .sent_bundles
            .lock()
            .trace_expect("Failed to lock sent_bundles mutex")
            .get(bundle_id)
            .map(|e| (e.seqno, e.send_time));

        let Some((seqno, send_time)) = entry_data else {
            if !self.quiet {
                eprintln!("Spurious status report received!");
            }
            return;
        };

        let mut output = format!("Ping {seqno}");

        match kind {
            hardy_bpa::services::StatusNotify::Received => {
                output.push_str(" received");
            }
            hardy_bpa::services::StatusNotify::Forwarded => {
                output.push_str(" forwarded");
            }
            hardy_bpa::services::StatusNotify::Delivered => {
                output.push_str(" delivered");
            }
            hardy_bpa::services::StatusNotify::Deleted => {
                output.push_str(" deleted");
                // We're never going to receive a response now
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

        if !matches!(
            reason,
            hardy_bpv7::status_report::ReasonCode::NoAdditionalInformation
        ) {
            output = format!("{output}, {reason:?},");
        }

        // Show elapsed time from send to status report
        let report_time = timestamp.unwrap_or_else(time::OffsetDateTime::now_utc);
        if let Ok(elapsed) = (report_time - send_time).try_into() {
            let elapsed: std::time::Duration = elapsed;
            output = format!("{output} after {}", humantime::format_duration(elapsed));

            // Track hops for path display (exclude local node)
            if from.to_string() != self.node_id {
                let mut path_hops = self
                    .path_hops
                    .lock()
                    .trace_expect("Failed to lock path_hops mutex");
                let hops = path_hops.entry(seqno).or_default();

                // Avoid duplicates of same node+kind (e.g., retransmissions)
                if !hops.iter().any(|h| h.node == *from && h.kind == kind) {
                    hops.push(PathHop {
                        node: from.clone(),
                        elapsed,
                        kind,
                    });
                }
            }
        }

        if !self.quiet {
            println!("{output}");
        }
    }
}
