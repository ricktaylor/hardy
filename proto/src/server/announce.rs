// The claim-check exchange behind announce-and-collect: one side
// announces a bundle under its id, the announcing event carries only
// that id, and the other side collects it exactly once.
//
// Keyed by the typed [`BundleId`] rather than the wire string, so an
// equivalent but differently-rendered id still matches. What is
// announced is always a [`Collection`], the two stream ends of the
// collecting call, so one table serves every surface. The announcer
// holds one [`Announced`] for the whole exchange: it waits on it, and
// dropping it withdraws, so no entry outlives the call that made it and
// the table needs no reaper.
//
// [`announce`]: Announcements::announce
// [`collect`]: Announcements::collect

use core::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;
use hardy_async::CancellationToken;
use hardy_bpv7::bundle::Id as BundleId;
use tokio::sync::{mpsc::Sender, oneshot};
use tonic::{Status, Streaming};

// The streams of the call that collects an announcement: the bundle
// goes down `responses_tx`, the collector's verdict comes back up
// `requests`. The door hands these to the awaiting announcer.
pub struct Collection<Rsp, Req> {
    pub responses_tx: Sender<Result<Rsp, Status>>,
    pub requests: Streaming<Req>,
}

// The one-shot the door answers with its call, under the sequence
// number the table minted for it.
type Entry<Rsp, Req> = (u64, oneshot::Sender<Collection<Rsp, Req>>);

// One session's announced-but-uncollected work in one direction, keyed
// by bundle id. Dropping the table on session death leaves the bundles
// parked in the BPA.
//
// The sequence number is an entry's identity for its own withdraw:
// where a successor can overwrite a live entry for the same bundle, a
// stale announcement must recognise the entry is no longer its own and
// spare it. Uniqueness is all the check needs, so a plain atomic.
//
// Every operation is one keyed access, so the sharded map suffices with
// no table-wide lock. The hasher is the DoS-resistant default, unlike
// [`Sessions`](crate::server::subscribe::Sessions): these keys come
// from a remote peer.
pub struct Announcements<Rsp, Req> {
    state: DashMap<BundleId, Entry<Rsp, Req>>,
    seq: AtomicU64,
}

impl<Rsp, Req> Announcements<Rsp, Req> {
    // Records one announcement. Call it before the event that carries
    // the bundle id to the client, so a racing door always finds it.
    pub fn announce<'a>(&'a self, bundle_id: &'a BundleId) -> Announced<'a, Rsp, Req> {
        let (call_tx, call_rx) = oneshot::channel();
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        self.state.insert(bundle_id.clone(), (seq, call_tx));
        Announced {
            table: self,
            bundle_id,
            seq,
            call_rx,
        }
    }

    // Takes an announcement's single collection capability. `None` for
    // an id never announced, already collected, or withdrawn; the door
    // answers all three as not-found.
    pub fn collect(&self, bundle_id: &BundleId) -> Option<oneshot::Sender<Collection<Rsp, Req>>> {
        self.state
            .remove(bundle_id)
            .map(|(_, (_, call_tx))| call_tx)
    }
}

// Manual, so an empty table asks nothing of the wire types.
impl<Rsp, Req> Default for Announcements<Rsp, Req> {
    fn default() -> Self {
        Self {
            state: DashMap::new(),
            seq: AtomicU64::new(0),
        }
    }
}

/// One live announcement, held by the announcer for the whole
/// exchange: [`collected`](Self::collected) waits on it for the
/// collecting call, and dropping it withdraws the announcement, so the
/// BPA announces the parked bundle again to a later registration.
///
/// The withdrawal runs on every exit of the announcer's frame, a
/// dropped future included, which is why it is the drop's job and not a
/// call the announcer could forget on one path. It is a no-op once the
/// door has collected the entry, and an announcement that has been
/// superseded spares its successor's live one (see [`Announcements`]).
///
/// Bind it (`let mut announced = …`): assigning to `_` drops it
/// immediately and withdraws the announcement just made.
pub struct Announced<'a, Rsp, Req> {
    table: &'a Announcements<Rsp, Req>,
    bundle_id: &'a BundleId,
    seq: u64,
    call_rx: oneshot::Receiver<Collection<Rsp, Req>>,
}

impl<Rsp, Req> Announced<'_, Rsp, Req> {
    /// Waits for the door that collects this announcement, and yields
    /// the streams of its call.
    ///
    /// `None` if the session ends first, or if the collecting call is
    /// gone before it could hand them over. Both leave the bundle with
    /// the BPA, to be announced again to a later registration, so both
    /// are the announcer's disconnection.
    ///
    /// Bounding the wait is the caller's: wrap the call in whatever
    /// deadline the exchange already has.
    pub async fn collected(
        &mut self,
        cancelled: &CancellationToken,
    ) -> Option<Collection<Rsp, Req>> {
        tokio::select! {
            biased;
            _ = cancelled.cancelled() => None,
            call = &mut self.call_rx => call.ok(),
        }
    }
}

impl<Rsp, Req> Drop for Announced<'_, Rsp, Req> {
    fn drop(&mut self) {
        // An entry a successor has already replaced is not this one's
        // to remove.
        self.table
            .state
            .remove_if(self.bundle_id, |_, (seq, _)| *seq == self.seq);
    }
}

#[cfg(test)]
mod tests {
    use hardy_bpv7::creation_timestamp::CreationTimestamp;

    use super::*;

    // The table never touches a `Collection`, so the unit type
    // exercises the same bookkeeping.
    type Table = Announcements<(), ()>;

    // Distinctness rides the source EID, so two different `n` can never
    // name the same bundle.
    fn ticket(n: u32) -> BundleId {
        BundleId {
            source: format!("ipn:{n}.1").parse().unwrap(),
            timestamp: CreationTimestamp::now(),
            fragment_info: None,
        }
    }

    #[test]
    fn a_collect_is_single_use() {
        let announcements = Table::default();
        let a = ticket(1);
        let _announced = announcements.announce(&a);

        assert!(announcements.collect(&a).is_some());
        assert!(
            announcements.collect(&a).is_none(),
            "a collect is single-use"
        );
    }

    #[test]
    fn a_dropped_announcement_is_withdrawn() {
        let announcements = Table::default();
        let a = ticket(1);
        {
            let _announced = announcements.announce(&a);
        }

        assert!(
            announcements.collect(&a).is_none(),
            "an abandoned announcement must be withdrawn"
        );
    }

    #[test]
    fn a_stale_announcement_spares_a_successor() {
        let announcements = Table::default();
        let a = ticket(1);

        let stale = announcements.announce(&a);
        let _live = announcements.announce(&a);

        // Dropping the superseded announcement both withdraws it and
        // drops its receiver, which tells the two entries apart: only
        // the successor's one-shot still has a receiver to answer.
        drop(stale);
        let call_tx = announcements
            .collect(&a)
            .expect("a stale announcement must not remove a successor's entry");
        assert!(
            !call_tx.is_closed(),
            "the entry left under the ticket must be the successor's"
        );
    }
}
