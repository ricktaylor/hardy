// Sample Announce+Pull wrappers for the simple bpa trait APIs

use dashmap::DashMap;
use hardy_bpa::{services, stream};

pub struct Announcements {
    waiting: DashMap<hardy_bpv7::bundle::Id, hardy_async::channel::Receiver<stream::Segment>>,
}

impl Announcements {
    // Works just as well for on_forward
    pub async fn announce(
        &self,
        bundle_id: &hardy_bpv7::bundle::Id,
        expiry: time::OffsetDateTime,
        total_len: u64,
        stream: &mut dyn stream::Receiver<stream::Segment>,
    ) -> services::Result<()> {
        let tx = match self.waiting.entry(bundle_id.clone()) {
            dashmap::Entry::Occupied(_) => {
                // Duplicates are pretty much impossible (the BPA drives at most
                // one delivery per bundle id at a time), but Ok would complete
                // the delivery and delete the bundle - defer it instead.
                return Err(services::Error::Internal("duplicate announcement".into()));
            }
            dashmap::Entry::Vacant(entry) => {
                // Rendezvous channel: a send completes only when the puller
                // takes the segment, so Ok from this method means the puller
                // took Final - the wire's commit point.
                let (tx, rx) = hardy_async::channel::bounded(0);
                entry.insert(rx);
                tx
            }
        };

        // Announce
        if let Err(e) = dummy_announce(bundle_id, expiry, total_len).await {
            // Remove the stashed announce value
            self.waiting.remove(bundle_id);
            return Err(e);
        }

        // Pump, bounded by bundle expiry. "Announced but never pulled" is this
        // layer's timeout to enforce: the BPA cannot see that the wire splits
        // delivery into announce and collect, and a rendezvous send parked
        // waiting for a puller that never comes is out of reach of anything
        // the BPA does to `stream` (withdrawal or rate-control pacing only
        // surface at our next recv()). Pacing a slow-but-live puller any
        // tighter than expiry is the BPA's job on the stream it passed in -
        // or, to preempt mid-send, by cancelling this whole call.
        let r = tokio::time::timeout(
            (expiry - time::OffsetDateTime::now_utc())
                .try_into()
                .unwrap_or(core::time::Duration::ZERO),
            async {
                loop {
                    match stream.recv().await {
                        Ok(stream::Segment::Next(b)) => tx
                            .send(stream::Segment::Next(b))
                            .await
                            .map_err(|_| services::Error::StreamCancelled)?,
                        Ok(stream::Segment::Final(b)) => {
                            break tx
                                .send(stream::Segment::Final(b))
                                .await
                                .map_err(|_| services::Error::StreamCancelled);
                        }
                        Err(_) => break Err(services::Error::StreamCancelled),
                    }
                }
            },
        )
        .await
        .unwrap_or(Err(services::Error::Internal(
            "bundle expired before collection".into(),
        )));

        if r.is_err() {
            // Remove the stashed announce value - just for safety
            self.waiting.remove(bundle_id);
        }
        r
    }

    pub fn pull(
        &self,
        bundle_id: &hardy_bpv7::bundle::Id,
    ) -> Option<hardy_async::channel::Receiver<stream::Segment>> {
        self.waiting.remove(bundle_id).map(|(_, rx)| rx)
    }

    // Call from on_unregister: dropping the stashed receivers fails any
    // announce blocked in send immediately (SendError -> Err -> the bundle
    // parks for re-announcement to the next registration).
    pub fn clear(&self) {
        self.waiting.clear();
    }
}

async fn dummy_announce(
    _bundle_id: &hardy_bpv7::bundle::Id,
    _expiry: time::OffsetDateTime,
    _total_len: u64,
) -> services::Result<()> {
    // Tell some remote gRPC client that data is available to pull
    todo!()
}
