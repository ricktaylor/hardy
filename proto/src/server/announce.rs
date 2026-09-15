//! The rendezvous between a bundle the BPA offers and the call that collects
//! it.
//!
//! The BPA offers a bundle through a trait call (`on_deliver`, `forward`) that
//! holds the segment stream, and the client collects it through a separate call
//! (`Receive`, `Forward`) that names the bundle by id. [`Announcements`] joins
//! the two: the offering side announces the id, pushes the event that offers it
//! and waits, and the collecting call presents the id, hands over its request
//! stream and takes the response stream the announcer writes.

use std::collections::{HashMap, hash_map::Entry};

use hardy_async::sync::spin::Mutex;
use hardy_bpv7::bundle::Id as BundleId;
use tokio::sync::{
    mpsc::{self, OwnedPermit, Receiver, Sender},
    oneshot,
};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Status, Streaming};

use crate::{
    chunking::BUFFERED_CHUNKS,
    timeouts::{Stage, Timeouts},
};

/// A guard on how a collecting call ends, holding the one slot kept back on
/// its response channel so that the message it ends with always has room.
///
/// A client that stops reading fills the channel, and the status that says so
/// is exactly the message it then needs; without the reservation that status
/// would be dropped and the client would see the stream simply stop. The
/// guard is armed from the announcement: dropped as it is, it ends the call
/// with `ABORTED`, since an exchange abandoned where it stands, before or
/// after the client claimed it, must not end as a completed transfer.
/// [`disarm`](CollectionGuard::disarm) hands the slot to the caller to end the
/// call its own way, with a status sent through it or by dropping it unused,
/// which closes the stream `OK`.
pub struct CollectionGuard<Rsp>(Option<OwnedPermit<Result<Rsp, Status>>>);

impl<Rsp> CollectionGuard<Rsp> {
    /// Reserves the ending slot on `responses_tx`.
    ///
    /// # Panics
    ///
    /// Panics if the channel has no room, which cannot happen: the slot is
    /// taken as the channel is created, before anything is written.
    fn reserve(responses_tx: &Sender<Result<Rsp, Status>>) -> Self {
        Self(Some(
            responses_tx
                .clone()
                .try_reserve_owned()
                .expect("a freshly created channel has spare capacity"),
        ))
    }

    /// Disarms the guard and returns the reserved slot, for the caller to end
    /// the call with.
    pub fn disarm(mut self) -> OwnedPermit<Result<Rsp, Status>> {
        self.0
            .take()
            .expect("the slot is only taken by `disarm`, which consumes the guard")
    }
}

impl<Rsp> Drop for CollectionGuard<Rsp> {
    fn drop(&mut self) {
        if let Some(permit) = self.0.take() {
            permit.send(Err(Status::aborted("the bundle was withdrawn")));
        }
    }
}

/// The collecting call, as the side that announced the bundle receives it.
pub struct Collection<Rsp, Req> {
    /// The call's request stream, on which the client's `ack`, result, or `cancel`
    /// arrives.
    pub requests: Streaming<Req>,
    /// The channel the call's responses go down.
    pub responses_tx: Sender<Result<Rsp, Status>>,
    /// The guard on the call's ending, holding the room reserved for it.
    pub guard: CollectionGuard<Rsp>,
}

/// Why an announcement yielded no collecting call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AnnounceError {
    /// The bundle id is announced already. The BPA is offering a bundle this
    /// session already holds, and the refusal is the answer.
    #[error("the bundle is already announced")]
    AlreadyAnnounced,
    /// The session ended, or the client outlasted a bound, before the bundle
    /// was collected. A stall is recorded and logged where it is detected.
    #[error("the bundle was not collected")]
    Uncollected,
}

/// What an announcement holds for the call that collects it: the read end of
/// the response stream, and the hand-over for the call's request stream.
struct Pending<Rsp, Req> {
    responses_rx: Receiver<Result<Rsp, Status>>,
    requests_tx: oneshot::Sender<Streaming<Req>>,
}

/// A session's table of announced, uncollected bundles.
///
/// An announcement is keyed by bundle id and is single-use: the first
/// collection removes it, so a second `Receive` or `Forward` for the same
/// bundle finds nothing. A duplicate announcement is refused while the first is
/// live, and an announcement nobody collects is withdrawn.
pub struct Announcements<Rsp, Req> {
    /// The table holds one live entry per bundle the BPA is offering, which
    /// is one at a time per endpoint or peer, so a map under one lock is the
    /// right size for it.
    state: Mutex<HashMap<BundleId, Pending<Rsp, Req>>>,
}

impl<Rsp, Req> Default for Announcements<Rsp, Req> {
    fn default() -> Self {
        Self {
            state: Mutex::new(HashMap::new()),
        }
    }
}

impl<Rsp, Req> Announcements<Rsp, Req> {
    /// Announces `bundle_id`, offers it to the client with `offer`, and waits,
    /// under the claim bound of `timeouts`, for the call that collects it.
    ///
    /// The announcement is entered before `offer` runs, so that a client which
    /// collects the instant it reads the event finds it, and it is withdrawn
    /// when this call ends, including when the BPA drops the wait, so neither
    /// end can wedge an id. The response stream is this side's from the start,
    /// so a collecting call that arrives as the announcement is withdrawn is
    /// ended with `ABORTED` by the guard, like one abandoned later.
    ///
    /// # Errors
    ///
    /// Returns [`AnnounceError::AlreadyAnnounced`] if `bundle_id` is announced
    /// and not yet collected, and [`AnnounceError::Uncollected`] if the offer
    /// cannot be pushed, the session ends or the claim bound passes first.
    pub async fn announce(
        &self,
        timeouts: &Timeouts,
        bundle_id: &BundleId,
        offer: impl AsyncFnOnce() -> Result<(), Status>,
    ) -> Result<Collection<Rsp, Req>, AnnounceError> {
        // One slot above the chunk depth, for the ending.
        let (responses_tx, responses_rx) = mpsc::channel(BUFFERED_CHUNKS + 1);
        let (requests_tx, requests_rx) = oneshot::channel();
        match self.state.lock().entry(bundle_id.clone()) {
            Entry::Occupied(_) => return Err(AnnounceError::AlreadyAnnounced),
            Entry::Vacant(entry) => {
                entry.insert(Pending {
                    responses_rx,
                    requests_tx,
                });
            }
        }
        let guard = CollectionGuard::reserve(&responses_tx);
        let mut withdrawal = Withdrawal {
            announcements: self,
            bundle_id,
            requests_rx,
        };

        // An offer the client was never told of ends the same way a claim the
        // client never made does.
        offer().await.map_err(|_| AnnounceError::Uncollected)?;
        let requests = timeouts
            .bound(Stage::Claim, async {
                (&mut withdrawal.requests_rx)
                    .await
                    .map_err(|_| Status::unavailable("registration closed"))
            })
            .await
            .map_err(|_| AnnounceError::Uncollected)?;

        Ok(Collection {
            requests,
            responses_tx,
            guard,
        })
    }

    /// Collects the announcement for `bundle_id`, handing the announcer
    /// `requests`, and returns the stream the collecting call answers with.
    ///
    /// Returns `None` if `bundle_id` is not announced, has already been collected,
    /// or was withdrawn before the hand-over.
    pub fn collect(
        &self,
        bundle_id: &BundleId,
        requests: Streaming<Req>,
    ) -> Option<ReceiverStream<Result<Rsp, Status>>> {
        let Pending {
            responses_rx,
            requests_tx,
        } = self.take(bundle_id)?;
        requests_tx.send(requests).ok()?;
        Some(ReceiverStream::new(responses_rx))
    }

    /// Removes the announcement for `bundle_id` and returns what it holds, if
    /// it is announced.
    fn take(&self, bundle_id: &BundleId) -> Option<Pending<Rsp, Req>> {
        self.state.lock().remove(bundle_id)
    }
}

/// Withdraws an announcement when the call that made it ends, however it ends.
///
/// The entry is made before the client is told of the bundle, so the
/// withdrawal has to run even when the BPA drops the wait. Everything it does
/// is synchronous, so it runs from `Drop`.
struct Withdrawal<'a, Rsp, Req> {
    announcements: &'a Announcements<Rsp, Req>,
    bundle_id: &'a BundleId,
    requests_rx: oneshot::Receiver<Streaming<Req>>,
}

impl<Rsp, Req> Drop for Withdrawal<'_, Rsp, Req> {
    fn drop(&mut self) {
        let mut state = self.announcements.state.lock();
        // Closing the hand-over under the lock marks this call's entry, and
        // only this one, as spent, so the removal cannot take an announcement
        // made after a collection took this one.
        self.requests_rx.close();
        if state
            .get(self.bundle_id)
            .is_some_and(|pending| pending.requests_tx.is_closed())
        {
            state.remove(self.bundle_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use core::{
        pin::pin,
        task::{Context, Waker},
        time::Duration,
    };

    use hardy_async::CancellationToken;
    use hardy_bpv7::creation_timestamp::CreationTimestamp;
    use tonic::Code;

    use super::*;
    use crate::{server::Limits, timeouts::Stage};

    type Table = Announcements<(), ()>;

    fn unhurried() -> Timeouts {
        Timeouts::new(Limits::default(), CancellationToken::new())
    }

    fn ticket(n: u32) -> BundleId {
        BundleId {
            source: format!("ipn:{n}.1").parse().unwrap(),
            timestamp: CreationTimestamp::now(),
            fragment_info: None,
        }
    }

    // An offer the client reads at once.
    async fn offered() -> Result<(), Status> {
        Ok(())
    }

    #[tokio::test]
    async fn a_collect_is_single_use() {
        let timeouts = unhurried();
        let announcements = Table::default();
        let a = ticket(1);
        let mut announced = pin!(announcements.announce(&timeouts, &a, offered));
        assert!(
            announced
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending(),
            "the announcement must wait to be collected"
        );

        assert!(announcements.take(&a).is_some());
        assert!(announcements.take(&a).is_none(), "a collect is single-use");
    }

    #[tokio::test]
    async fn an_abandoned_announcement_frees_its_id() {
        let timeouts = unhurried();
        let announcements = Table::default();
        let a = ticket(1);
        {
            let mut announced = pin!(announcements.announce(&timeouts, &a, offered));
            assert!(
                announced
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending(),
                "the announcement must wait to be collected"
            );
        }

        assert!(
            announcements.take(&a).is_none(),
            "an abandoned announcement must be withdrawn, not left behind"
        );
        let mut again = pin!(announcements.announce(&timeouts, &a, offered));
        assert!(
            again
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending(),
            "an abandoned announcement must leave its id free"
        );
        let pending = announcements.take(&a).expect("the id must be announced");
        assert!(
            !pending.requests_tx.is_closed(),
            "a collection must reach the live announcement, not the abandoned one"
        );
    }

    #[tokio::test]
    async fn a_collection_racing_a_withdrawal_is_aborted() {
        let timeouts = unhurried();
        let announcements = Table::default();
        let a = ticket(1);
        // A collection takes the hand-over, and the announcement ends before
        // the collection can use it.
        let mut pending = {
            let mut announced = pin!(announcements.announce(&timeouts, &a, offered));
            assert!(
                announced
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending(),
                "the announcement must wait to be collected"
            );

            announcements.take(&a).expect("the id must be announced")
        };

        assert!(
            pending.requests_tx.is_closed(),
            "a withdrawn announcement must not still accept a collection"
        );
        let Some(Err(aborted)) = pending.responses_rx.try_recv().ok() else {
            panic!("a collection the announcer abandoned must be told so");
        };
        assert_eq!(aborted.code(), Code::Aborted);
    }

    #[tokio::test]
    async fn a_duplicate_announcement_is_refused() {
        let timeouts = unhurried();
        let announcements = Table::default();
        let a = ticket(1);
        let mut live = pin!(announcements.announce(&timeouts, &a, offered));
        assert!(
            live.as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending(),
            "the announcement must wait to be collected"
        );

        let Err(refused) = announcements.announce(&timeouts, &a, offered).await else {
            panic!("a second announcement under a live id must be refused");
        };
        assert_eq!(refused, AnnounceError::AlreadyAnnounced);
        assert!(
            announcements.take(&a).is_some(),
            "a refused duplicate must not disturb the live entry"
        );
    }

    #[tokio::test]
    async fn an_announcement_the_client_is_never_told_of_is_withdrawn() {
        let timeouts = unhurried();
        let announcements = Table::default();
        let a = ticket(1);

        let Err(refused) = announcements
            .announce(&timeouts, &a, async || {
                Err(Status::unavailable("registration closed"))
            })
            .await
        else {
            panic!("an offer that cannot be sent leaves nobody to collect");
        };
        assert_eq!(refused, AnnounceError::Uncollected);
        assert!(
            announcements.take(&a).is_none(),
            "an announcement the client never heard must be withdrawn"
        );
    }

    #[tokio::test]
    async fn an_uncollected_announcement_stalls_at_the_claim_stage() {
        let timeouts = Timeouts::new(
            Limits {
                claim: Duration::ZERO,
                ..Limits::default()
            },
            CancellationToken::new(),
        );
        let unclaimed = Table::default();
        let a = ticket(1);

        let Err(refused) = unclaimed.announce(&timeouts, &a, offered).await else {
            panic!("an offer nobody collects must end the announcement");
        };
        assert_eq!(refused, AnnounceError::Uncollected);
        assert_eq!(
            timeouts.timed_out().await,
            Stage::Claim,
            "the stall must be recorded against the claim stage"
        );
    }
}
