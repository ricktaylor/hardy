use crate::ping::payload::Payload;

use super::*;
use hardy_bpa::async_trait;
use hardy_bpv7::eid::Eid;
use std::{collections::HashMap, sync::Arc};

pub struct Service {
    sink: std::sync::OnceLock<Box<dyn hardy_bpa::service::Sink>>,
    destination: Eid,
    lifetime: std::time::Duration,
    flags: hardy_bpa::service::SendFlags,
    semaphore: Option<Arc<tokio::sync::Semaphore>>,
    count: Option<u32>,
    sent_bundles: std::sync::Mutex<HashMap<Box<str>, u32>>,
    expected_responses: std::sync::Mutex<HashMap<u32, time::OffsetDateTime>>,
    format: Format,
}

impl Service {
    pub fn new(args: &Command) -> Self {
        Self {
            sink: std::sync::OnceLock::new(),
            destination: args.destination.clone(),
            lifetime: args.lifetime(),
            flags: {
                let mut flags = args.flags.clone().unwrap_or_default();
                flags.do_not_fragment = true;
                flags.request_ack = true;
                flags
            },
            count: args.count,
            semaphore: args.count.map(|_| Arc::new(tokio::sync::Semaphore::new(0))),
            sent_bundles: std::sync::Mutex::new(HashMap::new()),
            expected_responses: std::sync::Mutex::new(HashMap::new()),
            format: args.format,
        }
    }

    pub async fn send(&self, args: &Command, seq_no: u32) -> anyhow::Result<()> {
        let (payload, creation) = ping::payload::build_payload(args, seq_no)?;

        println!("Sending ping seq={} ... ", seq_no);

        let id = self
            .sink
            .get()
            .unwrap()
            .send(
                self.destination.clone(),
                payload.as_ref(),
                self.lifetime,
                Some(self.flags.clone()),
            )
            .await
            .map_err(|e| anyhow::anyhow!("Failed to send bundle: {e}"))?;

        self.sent_bundles
            .lock()
            .trace_expect("Failed to lock sent_bundles mutex")
            .insert(id, seq_no);

        self.expected_responses
            .lock()
            .trace_expect("Failed to lock expected_responses mutex")
            .insert(seq_no, creation);

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
impl hardy_bpa::service::Service for Service {
    async fn on_register(&self, _source: &Eid, sink: Box<dyn hardy_bpa::service::Sink>) {
        if self.sink.set(sink).is_err() {
            panic!("Double connect()");
        }
    }

    async fn on_unregister(&self) {
        // Nothing to do
    }

    async fn on_receive(&self, bundle: hardy_bpa::service::Bundle) {
        if bundle.source != self.destination {
            // Ignore spurious responses
            eprintln!(
                "Ignoring bundle from unexpected source EID '{}'",
                bundle.source
            );
            return;
        }

        // Try to unpack the payload
        let payload = match self.format {
            Format::Text => {
                let Ok(s) = str::from_utf8(&bundle.payload) else {
                    eprintln!("Failed to parse ping payload as UTF-8 text");
                    return;
                };
                Payload::from_text_fmt(s)
            }
            Format::Binary => Payload::from_bin_fmt(&bundle.payload),
        };
        let payload = match payload {
            Ok(p) => p,
            Err(e) => {
                eprintln!("Failed to parse ping payload: {e}");
                return;
            }
        };

        if payload.service_flag != 0x02 {
            // Not a ping response
            eprintln!(
                "Ignoring bundle with unexpected service flag: {}",
                payload.service_flag
            );
            return;
        }

        let sent_time = self
            .expected_responses
            .lock()
            .trace_expect("Failed to lock expected_responses mutex")
            .remove(&payload.seqno);
        let Some(sent_time) = sent_time else {
            eprintln!(
                "Ignoring unexpected ping response with seqno {}",
                payload.seqno
            );
            return;
        };

        if let Ok(rtt) = (payload.creation - sent_time).try_into() {
            println!(
                "Reply from {}: seq={} rtt={}",
                &bundle.source,
                payload.seqno,
                humantime::format_duration(rtt)
            );
        } else {
            eprintln!("Failed to compute RTT for seqno {}", payload.seqno);
        }

        // Indicate that we have received a response
        if let Some(semaphore) = &self.semaphore {
            semaphore.add_permits(1);
        }
    }

    async fn on_status_notify(
        &self,
        bundle_id: &str,
        kind: hardy_bpa::service::StatusNotify,
        reason: hardy_bpv7::status_report::ReasonCode,
        timestamp: Option<hardy_bpv7::dtn_time::DtnTime>,
    ) {
        if let Some(seq_no) = self
            .sent_bundles
            .lock()
            .trace_expect("Failed to lock sent_bundles mutex")
            .get(bundle_id)
        {
            let timestamp = timestamp.map_or("<not reported>".to_string(), |t| {
                time::OffsetDateTime::from(t).to_string()
            });
            if matches!(
                reason,
                hardy_bpv7::status_report::ReasonCode::NoAdditionalInformation
            ) {
                println!("Status report for seq {seq_no}: {kind:?}, timestamp: {timestamp}",)
            } else {
                println!(
                    "Status report for seq {seq_no}: {kind:?}, reason: {reason:?}, timestamp: {timestamp}",
                )
            }
        } else {
            eprintln!("Spurious status report received!");
        }
    }
}
