// Rendezvous between a bundle announced to the client and the call
// that collects it. Dropping an `Announced` withdraws its entry.

use dashmap::{DashMap, Entry};
use hardy_bpv7::bundle::Id as BundleId;
use tokio::sync::{mpsc, mpsc::Sender, oneshot};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Status, Streaming};

use crate::server::{
    error::{self, Error},
    leases::{Lease, Leases},
};

// Chunk buffer depth of a collection's response channel. Kept small:
// HTTP/2 flow control paces the stream.
pub const DATA_CHANNEL_DEPTH: usize = 4;

// Both streams of the collecting call: bundle chunks go out on
// `responses_tx`, the collector replies on `requests`.
pub struct Collection<Rsp, Req> {
    pub responses_tx: Sender<Result<Rsp, Status>>,
    pub requests: Streaming<Req>,
}

// One session's announced but uncollected bundles, keyed by id. The
// hasher is the DoS-resistant default: bundle ids derive from
// untrusted wire data.
pub struct Announcements<Rsp, Req> {
    state: DashMap<BundleId, oneshot::Sender<Collection<Rsp, Req>>>,
    leases: Leases,
}

impl<Rsp, Req> Announcements<Rsp, Req> {
    pub fn new(leases: Leases) -> Self {
        Self {
            state: DashMap::new(),
            leases,
        }
    }

    // Registers `bundle_id`, refusing a duplicate: replacing the entry
    // would break a collection already in flight. Must run before the
    // event that carries the id reaches the client.
    pub fn announce<'a>(
        &'a self,
        bundle_id: &'a BundleId,
    ) -> error::Result<Announced<'a, Rsp, Req>> {
        match self.state.entry(bundle_id.clone()) {
            Entry::Occupied(_) => Err(Error::DuplicateAnnouncement),
            Entry::Vacant(entry) => {
                let (call_tx, call_rx) = oneshot::channel();
                entry.insert(call_tx);
                Ok(Announced {
                    table: self,
                    bundle_id,
                    call_rx,
                })
            }
        }
    }

    // Hands the collecting call's streams to the announcer waiting on
    // `bundle_id` and returns the stream the call responds with.
    // `None` if the id is unknown or the announcer is gone.
    pub fn collect(
        &self,
        bundle_id: &str,
        requests: Streaming<Req>,
    ) -> Option<ReceiverStream<Result<Rsp, Status>>> {
        let call_tx = self.take(&BundleId::from_key(bundle_id).ok()?)?;

        let (responses_tx, responses_rx) = mpsc::channel(DATA_CHANNEL_DEPTH);
        call_tx
            .send(Collection {
                responses_tx,
                requests,
            })
            .ok()?;
        Some(ReceiverStream::new(responses_rx))
    }

    fn take(&self, bundle_id: &BundleId) -> Option<oneshot::Sender<Collection<Rsp, Req>>> {
        self.state.remove(bundle_id).map(|(_, call_tx)| call_tx)
    }
}

// A live announcement; dropping it withdraws the entry. Bind it to a
// name: `_` drops it immediately.
pub struct Announced<'a, Rsp, Req> {
    table: &'a Announcements<Rsp, Req>,
    bundle_id: &'a BundleId,
    call_rx: oneshot::Receiver<Collection<Rsp, Req>>,
}

impl<Rsp, Req> Announced<'_, Rsp, Req> {
    // Waits under the claim lease for the collecting call and returns
    // its streams.
    pub async fn collected(mut self) -> error::Result<Collection<Rsp, Req>> {
        let leases = &self.table.leases;

        tokio::select! {
            biased;
            _ = leases.cancelled() => Err(Error::SessionClosed),
            call = &mut self.call_rx => call.map_err(|_| Error::SessionClosed),
            expired = leases.expired(Lease::Claim) => Err(expired),
        }
    }
}

impl<Rsp, Req> Drop for Announced<'_, Rsp, Req> {
    fn drop(&mut self) {
        self.table.state.remove(self.bundle_id);
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use hardy_async::CancellationToken;
    use hardy_bpv7::creation_timestamp::CreationTimestamp;

    use super::*;
    use crate::server::Limits;

    // The table never touches a `Collection`; unit types suffice.
    type Table = Announcements<(), ()>;

    // Default leases; no test using this fixture waits on them.
    fn table() -> Table {
        Table::new(Leases::new(
            CancellationToken::new(),
            "test",
            Limits::default(),
        ))
    }

    // A distinct `n` gives a distinct source EID and bundle id.
    fn ticket(n: u32) -> BundleId {
        BundleId {
            source: format!("ipn:{n}.1").parse().unwrap(),
            timestamp: CreationTimestamp::now(),
            fragment_info: None,
        }
    }

    #[test]
    fn a_collect_is_single_use() {
        let announcements = table();
        let a = ticket(1);
        let _announced = announcements
            .announce(&a)
            .expect("a fresh id must announce");

        assert!(announcements.take(&a).is_some());
        assert!(announcements.take(&a).is_none(), "a collect is single-use");
    }

    #[test]
    fn a_dropped_announcement_is_withdrawn() {
        let announcements = table();
        let a = ticket(1);
        {
            let _announced = announcements
                .announce(&a)
                .expect("a fresh id must announce");
        }

        assert!(
            announcements.take(&a).is_none(),
            "an abandoned announcement must be withdrawn"
        );
        assert!(
            announcements.announce(&a).is_ok(),
            "a withdrawn id must be free to announce again"
        );
    }

    #[test]
    fn a_duplicate_announcement_is_refused() {
        let announcements = table();
        let a = ticket(1);

        let _live = announcements
            .announce(&a)
            .expect("a fresh id must announce");
        assert!(
            matches!(
                announcements.announce(&a),
                Err(Error::DuplicateAnnouncement)
            ),
            "a second announcement under a live id must be refused"
        );

        assert!(
            announcements.take(&a).is_some(),
            "a refused duplicate must not disturb the live entry"
        );
    }

    #[tokio::test]
    async fn an_unanswered_wait_ends_on_its_claim_lease() {
        // A zero claim lease: with no collector the wait can only expire.
        let leases = Leases::new(
            CancellationToken::new(),
            "test",
            Limits {
                claim: Duration::ZERO,
                ..Limits::default()
            },
        );
        let unleased = Announcements::<(), ()>::new(leases.clone());
        let a = ticket(1);
        let announced = unleased.announce(&a).expect("a fresh id must announce");
        assert!(
            matches!(
                announced.collected().await,
                Err(Error::LeaseExpired(Lease::Claim))
            ),
            "a lease that runs out unanswered is the client's"
        );
    }
}
