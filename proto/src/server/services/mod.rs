// The rpc services, one file per wire surface, and the shared
// engines: [`stream_delivery`] pumps the BPA's segments out as wire
// chunks, generic only over the response message it emits (the up
// engine is `crate::server::adapter`); [`watch_session`] parks on a
// session stream until it ends; [`Deliveries`] holds a session's
// announced streams for its Receive door. Each surface keeps its
// doors next to the schema they implement; `application.rs` is the
// template, and the messages' `Chunk`/`Cancel`/`Unregister`
// capabilities live with the wire contract in `crate::stream`.

pub mod application;
pub mod cla;
pub mod routing;
pub mod service;

use std::{collections::HashMap, sync::Mutex};

use hardy_async::CancellationToken;
use hardy_bpa::{
    Bytes, services,
    stream::{Receiver, Segment},
};
use prost_types::Timestamp;
use time::OffsetDateTime;
use tokio::sync::mpsc::Sender;
use tonic::{Status, Streaming};
use tracing::{debug, warn};

use crate::stream::{Cancel, Chunk, Unregister, chunks};

// Both types are foreign, so this is a free function rather than a
// `From`; shared by the surfaces that emit status-report timestamps.
fn to_timestamp(t: OffsetDateTime) -> Timestamp {
    Timestamp {
        seconds: t.unix_timestamp(),
        nanos: t.nanosecond() as i32,
    }
}

// The one point where BPA service errors become gRPC statuses, shared
// by every surface's doors.
fn service_status(error: services::Error) -> Status {
    use services::Error;
    match error {
        Error::ServiceIdInUse(_) | Error::DuplicateBundle => {
            Status::already_exists(error.to_string())
        }
        Error::InvalidDestination(_)
        | Error::InvalidSource(_)
        | Error::InvalidBundle(_)
        | Error::PayloadUnderrun { .. } => Status::invalid_argument(error.to_string()),
        Error::NodeId(_) => Status::failed_precondition(error.to_string()),
        Error::PayloadTooLarge { .. } | Error::PayloadUnaddressable { .. } => {
            Status::resource_exhausted(error.to_string())
        }
        Error::Disconnected => Status::unavailable("Unregistered"),
        Error::StreamCancelled => Status::cancelled(error.to_string()),
        Error::Dropped(_) => Status::aborted(error.to_string()),
        Error::Internal(e) => Status::from_error(e),
    }
}

// One session's watch: parks until the session ends, however it ends
// — the trigger firing, the client's Unregister, half-close, or a
// failed stream. Unexpected messages are ignored without being
// Debug-formatted, uniformly with the data-plane doors.
async fn watch_session<Req: Unregister>(
    cancelled: CancellationToken,
    mut requests: Streaming<Req>,
) {
    loop {
        tokio::select! {
            biased;
            _ = cancelled.cancelled() => return,
            request = requests.message() => match request {
                Ok(Some(request)) if request.is_unregister() => return,
                Ok(Some(_)) => warn!("Ignoring unexpected message on the session stream"),
                Ok(None) | Err(_) => return,
            },
        }
    }
}

// Dead-entry sweep threshold for [`Deliveries`]: below it the map is
// too small to matter, above it each hold first reclaims entries
// whose bundle has certainly died (expiry passed), so the map tracks
// the parked set rather than the session's announcement history.
const DELIVERIES_SWEEP_THRESHOLD: usize = 64;

type HeldStream = (OffsetDateTime, Box<dyn Receiver<Segment>>);

// One session's announced-but-uncollected deliveries: the stream each
// `on_deliver` carried, with its bundle's expiry, keyed by the wire
// form of its bundle id. Dropping the map on session death leaves the
// bundles parked in the BPA.
#[derive(Default)]
struct Deliveries(Mutex<HashMap<String, HeldStream>>);

impl Deliveries {
    // Holds an announced stream for the Receive door. A bundle can die
    // while parked (expiry is the latest that can happen) with no
    // withdraw signal to the bridge, so dead entries are reclaimed
    // here, amortised against holds.
    fn hold(&self, key: String, expiry: OffsetDateTime, stream: Box<dyn Receiver<Segment>>) {
        let mut deliveries = self.0.lock().expect("deliveries lock poisoned");
        if deliveries.len() >= DELIVERIES_SWEEP_THRESHOLD {
            let now = OffsetDateTime::now_utc();
            deliveries.retain(|_, (expiry, _)| *expiry > now);
        }
        deliveries.insert(key, (expiry, stream));
    }

    // Withdraws a held stream: a dead session dropping its
    // announcement, so the BPA announces the parked bundle again to a
    // later registration.
    fn withdraw(&self, key: &str) {
        self.0.lock().expect("deliveries lock poisoned").remove(key);
    }

    // Takes the single collection capability for an announcement; a
    // repeat claim reads as not-found.
    fn claim(&self, key: &str) -> Option<Box<dyn Receiver<Segment>>> {
        self.0
            .lock()
            .expect("deliveries lock poisoned")
            .remove(key)
            .map(|(_, stream)| stream)
    }
}

// The request side of one Receive call, reduced to the only thing
// that matters about it: its terminal status. A cancel message is the
// abandonment, and a failed request stream is treated the same way (a
// partial collection must never look complete). Half-close is neither
// (normal for a client that sends only the metadata) and unexpected
// messages are ignored, so on those this future parks instead of
// resolving: it only ever resolves to end the collection.
async fn abandonment<Req: Cancel>(mut requests: Streaming<Req>) -> Status {
    loop {
        match requests.message().await {
            Ok(Some(req)) if req.is_cancel() => {
                return Status::cancelled("Collection abandoned");
            }
            // Not Debug-formatted: a stray metadata message carries the
            // session token, which must never reach the logs.
            Ok(Some(_)) => warn!("Ignoring unexpected message on the Receive request side"),
            // Nothing can follow a half-close: the request side goes
            // inert for the rest of the call.
            Ok(None) => core::future::pending().await,
            Err(e) => {
                debug!("Receive stream failed: {e}");
                return Status::aborted("Receive stream failed");
            }
        }
    }
}

// The down half of one Receive call: pulls the delivery's segments
// from the BPA and streams them as chunks, starting from `first`, the
// segment the door's probe already pulled. Pulling the final segment
// is the completion signal: it is pulled only after the previous
// segment's chunks are queued and no abandonment is pending, so every
// abandonment resolved before that pull leaves the bundle parked in
// the BPA, which is RFC 9171 delivery deferral; once the final
// segment is pulled the delivery has completed, and its chunks are
// sent through regardless of a late cancel. A withdrawn delivery ends
// the stream with the wire's in-band cancel.
//
// `abandoned` is the request side already reduced by [`abandonment`]:
// the door hands the future in, so the engine is generic only over
// the response message it emits.
// Pumps the BPA's delivery stream into the rendezvous the collector
// pulls from, bounded by the bundle's expiry. A `bounded(0)` send
// completes only when the collector takes the segment, so returning
// `Ok` after the collector takes `Final` is the wire's commit point,
// and a dropped collector (client abandoned, or expiry elapsed with no
// collection) surfaces as `Err`, parking the bundle for re-announcement.
// The BPA cannot see the wire's announce/collect split, so bounding the
// hold by expiry is this layer's responsibility.
async fn pump_to_collector(
    cancelled: CancellationToken,
    expiry: OffsetDateTime,
    stream: &mut dyn Receiver<Segment>,
    tx: hardy_async::channel::Sender<Segment>,
) -> services::Result<()> {
    let hold = (expiry - OffsetDateTime::now_utc())
        .try_into()
        .unwrap_or(core::time::Duration::ZERO);
    let pump = tokio::time::timeout(hold, async {
        loop {
            match stream.recv().await {
                Ok(Segment::Next(b)) => tx
                    .send(Segment::Next(b))
                    .await
                    .map_err(|_| services::Error::StreamCancelled)?,
                Ok(Segment::Final(b)) => {
                    // Split the terminal segment: the data travels as a
                    // `Next`, then an empty `Final` is the commit marker.
                    // The collector's claim-time probe pulls the data
                    // without committing, so a client that receives the
                    // bytes and then abandons (dropping the collector
                    // before the marker) fails this final send, which
                    // defers the bundle for the next registration. An
                    // empty terminal segment is already just the marker.
                    if !b.is_empty() {
                        tx.send(Segment::Next(b))
                            .await
                            .map_err(|_| services::Error::StreamCancelled)?;
                    }
                    break tx
                        .send(Segment::Final(Bytes::new()))
                        .await
                        .map_err(|_| services::Error::StreamCancelled);
                }
                Err(_) => break Err(services::Error::StreamCancelled),
            }
        }
    });
    // A lost connection (the session's cancel firing) unblocks the
    // rendezvous with an error, so the delivery call returns Err and the
    // BPA parks the bundle to re-dispatch to the next registration,
    // rather than the pump blocking on a collector that will never pull.
    tokio::select! {
        biased;
        _ = cancelled.cancelled() => Err(services::Error::Disconnected),
        r = pump => r.unwrap_or(Err(services::Error::StreamCancelled)),
    }
}

async fn stream_delivery<Resp: Chunk + Cancel + Send + 'static>(
    cancelled: CancellationToken,
    first: Segment,
    mut stream: Box<dyn Receiver<Segment>>,
    tx: Sender<Result<Resp, Status>>,
    abandoned: impl Future<Output = Status>,
) {
    // One abandonment future for the whole call, polled from both
    // waits: its half-close state lives inside it, and a cancel
    // arriving between segments or between chunks is never lost.
    //
    // Terminal statuses are try_send, best effort: they only matter to
    // a client that is reading, and awaiting a full channel after the
    // session died would park this task past pool shutdown.
    let mut abandoned = core::pin::pin!(abandoned);
    let mut probed = Some(first);
    loop {
        let segment = if let Some(segment) = probed.take() {
            segment
        } else {
            let segment = tokio::select! {
                biased;
                _ = cancelled.cancelled() => {
                    let _ = tx.try_send(Err(Status::aborted("Session closed")));
                    return;
                }
                status = &mut abandoned => {
                    let _ = tx.try_send(Err(status));
                    return;
                }
                segment = stream.recv() => segment,
            };
            match segment {
                Ok(segment) => segment,
                // Withdrawn mid-collection (expiry, shutdown): the
                // wire's in-band signal, then a clean end. Nothing
                // follows.
                Err(_) => {
                    let _ = tx.try_send(Ok(Resp::cancel()));
                    return;
                }
            }
        };

        if matches!(segment, Segment::Final(_)) {
            // Pulling the final segment completed the delivery in the
            // BPA, so an abandonment arriving now is too late to
            // honour: the remaining chunks must reach the wire, or the
            // client is told CANCELLED (the bundle stays held) about a
            // bundle that is already gone. Only session death ends the
            // sends early: those bytes have no reader, the irreducible
            // window.
            for segment in chunks(segment) {
                let permit = tokio::select! {
                    biased;
                    _ = cancelled.cancelled() => {
                        let _ = tx.try_send(Err(Status::aborted("Session closed")));
                        return;
                    }
                    permit = tx.reserve() => {
                        let Ok(permit) = permit else { return };
                        permit
                    }
                };
                permit.send(Ok(Resp::chunk(segment)));
            }
            return;
        }

        for segment in chunks(segment) {
            // Wait for send room, servicing cancellation and
            // abandonment while the client reads.
            let permit = tokio::select! {
                biased;
                _ = cancelled.cancelled() => {
                    let _ = tx.try_send(Err(Status::aborted("Session closed")));
                    return;
                }
                status = &mut abandoned => {
                    let _ = tx.try_send(Err(status));
                    return;
                }
                permit = tx.reserve() => {
                    let Ok(permit) = permit else { return };
                    permit
                }
            };
            permit.send(Ok(Resp::chunk(segment)));
        }
    }
}

#[cfg(test)]
mod tests {
    use hardy_bpa::Bytes;

    use super::*;

    fn held(seconds_from_now: i64) -> (OffsetDateTime, Box<dyn Receiver<Segment>>) {
        (
            OffsetDateTime::now_utc() + time::Duration::seconds(seconds_from_now),
            Box::new(Bytes::from_static(b"x")),
        )
    }

    #[tokio::test]
    async fn a_claim_is_single_use_and_withdraw_removes() {
        let deliveries = Deliveries::default();
        let (expiry, stream) = held(60);
        deliveries.hold("a".to_string(), expiry, stream);

        assert!(deliveries.claim("a").is_some());
        assert!(deliveries.claim("a").is_none(), "a claim is single-use");

        let (expiry, stream) = held(60);
        deliveries.hold("b".to_string(), expiry, stream);
        deliveries.withdraw("b");
        assert!(deliveries.claim("b").is_none());
    }

    #[tokio::test]
    async fn dead_entries_are_swept_once_the_threshold_is_reached() {
        let deliveries = Deliveries::default();
        for i in 0..DELIVERIES_SWEEP_THRESHOLD {
            let (expiry, stream) = held(-60);
            deliveries.hold(format!("dead-{i}"), expiry, stream);
        }
        // The hold at the threshold reclaims every expired entry first.
        let (expiry, stream) = held(60);
        deliveries.hold("live".to_string(), expiry, stream);

        assert_eq!(
            deliveries.0.lock().expect("deliveries lock poisoned").len(),
            1,
            "expired entries must be reclaimed, not accumulated"
        );
        assert!(deliveries.claim("live").is_some());
        assert!(deliveries.claim("dead-0").is_none());
    }
}
