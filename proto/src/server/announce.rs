//! The rendezvous between a bundle the BPA offers and the call that collects
//! it.
//!
//! The BPA offers a bundle through a trait call (`on_deliver`, `forward`) that
//! holds the segment stream, and the client collects it through a separate call
//! (`Receive`, `Forward`) that names the bundle by id. [`Announcements`] joins
//! the two: the offering side announces the id, pushes the event that offers it
//! and waits, and the collecting call presents the id and is handed the streams
//! of the exchange.

use std::sync::Arc;

use dashmap::{DashMap, Entry};
use hardy_async::CancellationToken;
use hardy_bpv7::bundle::Id as BundleId;
use tokio::sync::{
    mpsc::{self, OwnedPermit, Sender},
    oneshot,
};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Code, Status, Streaming};

use crate::server::watchdog::Watchdog;

/// Why an announcement yielded no collecting call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AnnounceError {
    /// The bundle id is announced already. The BPA is offering a bundle this
    /// session already holds, and the refusal is the answer.
    #[error("the bundle is already announced")]
    AlreadyAnnounced,
    /// The session ended, on either side, before the bundle was collected.
    #[error("the session closed before the bundle was collected")]
    SessionClosed,
    /// The client left the bundle uncollected for longer than the claim bound,
    /// or fell behind the event stream's bound before it was even told of it.
    #[error("the bundle was not collected in time")]
    CollectionTimedOut,
}

/// The number of chunks the response channel of a collection buffers ahead of
/// the client, besides the slot held back for the message the call ends with.
pub const DATA_CHANNEL_DEPTH: usize = 4;

/// The collecting call, as the side that announced the bundle receives it.
pub struct Collection<Rsp, Req> {
    /// The call's request stream, on which the client's `ack`, result, or `cancel`
    /// arrives.
    pub requests: Streaming<Req>,
    /// The channel the call's responses go down.
    pub responses_tx: Sender<Result<Rsp, Status>>,
    /// The one slot held back on that channel so that the message the call ends
    /// with always has room.
    ///
    /// A client that stops reading fills the channel, and the status that says
    /// so is exactly the message it then needs; without the reservation that
    /// status would be dropped and the client would see the stream simply stop.
    /// Whoever holds the permit sends the ending, and it ends the call once.
    pub permit: OwnedPermit<Result<Rsp, Status>>,
}

impl<Rsp, Req> Collection<Rsp, Req> {
    /// Takes up the collecting call whose request stream is `requests` and whose
    /// response stream is fed from `responses_tx`.
    ///
    /// # Panics
    ///
    /// Panics if `responses_tx` has no room, which cannot happen: the slot is
    /// taken as the channel is created, before anything is written.
    fn new(requests: Streaming<Req>, responses_tx: Sender<Result<Rsp, Status>>) -> Self {
        let permit = responses_tx
            .clone()
            .try_reserve_owned()
            .expect("a freshly created channel has spare capacity");
        Self {
            requests,
            responses_tx,
            permit,
        }
    }
}

/// A session's table of announced, uncollected bundles.
///
/// An announcement is keyed by bundle id and is single-use: the first
/// collection removes it, so a second `Receive` or `Forward` for the same
/// bundle finds nothing. A duplicate announcement is refused while the first is
/// live, and an announcement nobody collects is withdrawn.
pub struct Announcements<Rsp, Req> {
    state: DashMap<BundleId, oneshot::Sender<Collection<Rsp, Req>>>,
    cancel: CancellationToken,
    watchdog: Arc<Watchdog>,
}

impl<Rsp, Req> Announcements<Rsp, Req> {
    /// Creates an empty table whose waits end when `cancel` fires and are bounded
    /// by the claim rule of `watchdog`.
    pub fn new(cancel: CancellationToken, watchdog: Arc<Watchdog>) -> Self {
        Self {
            state: DashMap::new(),
            cancel,
            watchdog,
        }
    }

    /// Announces `bundle_id`, offers it to the client with `offer`, and waits for
    /// the call that collects it.
    ///
    /// The announcement is entered before `offer` runs, so that a client which
    /// collects the instant it reads the event finds it, and it lasts no longer
    /// than this call: an entry an announcer has left behind hands nothing over
    /// and is replaced by the next announcement of the same bundle, so the BPA
    /// dropping the wait cannot wedge an id.
    ///
    /// # Errors
    ///
    /// Returns [`AnnounceError::AlreadyAnnounced`] if `bundle_id` is announced
    /// and not yet collected, [`AnnounceError::SessionClosed`] if the session
    /// ends first, and [`AnnounceError::CollectionTimedOut`] if a bound passes
    /// first. A stall is logged where it is detected. A collecting call that
    /// arrives as the announcement is withdrawn is ended with `ABORTED`, since
    /// the client must not read a stream that then closes as a completed
    /// transfer.
    pub async fn announce(
        &self,
        bundle_id: &BundleId,
        offer: impl AsyncFnOnce() -> Result<(), Status>,
    ) -> Result<Collection<Rsp, Req>, AnnounceError> {
        let (call_tx, mut call_rx) = oneshot::channel();
        match self.state.entry(bundle_id.clone()) {
            // An entry whose announcer has gone is spent, and the id is free.
            Entry::Occupied(mut entry) if entry.get().is_closed() => {
                entry.insert(call_tx);
            }
            Entry::Occupied(_) => return Err(AnnounceError::AlreadyAnnounced),
            Entry::Vacant(entry) => {
                entry.insert(call_tx);
            }
        }

        let collected = match offer().await {
            // An offer the client was never told of ends the same way the event
            // stream did: stalled, or closed.
            Err(status) if status.code() == Code::DeadlineExceeded => {
                Err(AnnounceError::CollectionTimedOut)
            }
            Err(_) => Err(AnnounceError::SessionClosed),
            Ok(()) => tokio::select! {
                biased;
                _ = self.cancel.cancelled() => Err(AnnounceError::SessionClosed),
                call = &mut call_rx => call.map_err(|_| AnnounceError::SessionClosed),
                _ = self.watchdog.claim() => Err(AnnounceError::CollectionTimedOut),
            },
        };
        if collected.is_err() {
            // A collection that won the hand-over as the wait ended is told
            // the bundle is gone, through the slot it holds for its ending.
            if let Ok(collection) = call_rx.try_recv() {
                collection
                    .permit
                    .send(Err(Status::aborted("the bundle was withdrawn")));
            }
            // Dropping this end first marks the entry spent, so the removal can
            // only take the announcement this call made.
            drop(call_rx);
            self.state
                .remove_if(bundle_id, |_, call_tx| call_tx.is_closed());
        }
        collected
    }

    /// Collects the announcement for `bundle_id`, handing the announcer `requests`
    /// and a fresh response channel, and returns the stream the collecting call
    /// answers with.
    ///
    /// Returns `None` if `bundle_id` is not announced, has already been collected,
    /// or was withdrawn before the hand-over.
    pub fn collect(
        &self,
        bundle_id: &BundleId,
        requests: Streaming<Req>,
    ) -> Option<ReceiverStream<Result<Rsp, Status>>> {
        let call_tx = self.take(bundle_id)?;
        // One slot above the chunk depth, for the ending.
        let (responses_tx, responses_rx) = mpsc::channel(DATA_CHANNEL_DEPTH + 1);
        call_tx.send(Collection::new(requests, responses_tx)).ok()?;
        Some(ReceiverStream::new(responses_rx))
    }

    /// Removes the announcement for `bundle_id` and returns its hand-over channel,
    /// if it is announced.
    fn take(&self, bundle_id: &BundleId) -> Option<oneshot::Sender<Collection<Rsp, Req>>> {
        self.state.remove(bundle_id).map(|(_, call_tx)| call_tx)
    }
}

#[cfg(test)]
mod tests {
    use core::{
        pin::pin,
        task::{Context, Waker},
        time::Duration,
    };

    use hardy_bpv7::creation_timestamp::CreationTimestamp;

    use super::*;
    use crate::server::{Limits, watchdog::Stage};

    type Table = Announcements<(), ()>;

    fn table() -> Table {
        Table::new(
            CancellationToken::new(),
            Arc::new(Watchdog::new(Limits::default())),
        )
    }

    fn ticket(n: u32) -> BundleId {
        BundleId {
            source: format!("ipn:{n}.1").parse().unwrap(),
            timestamp: CreationTimestamp::now(),
            fragment_info: None,
        }
    }

    /// An offer the client reads at once.
    async fn offered() -> Result<(), Status> {
        Ok(())
    }

    #[tokio::test]
    async fn a_collect_is_single_use() {
        let announcements = table();
        let a = ticket(1);
        let mut announced = pin!(announcements.announce(&a, offered));
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
        let announcements = table();
        let a = ticket(1);
        {
            let mut announced = pin!(announcements.announce(&a, offered));
            assert!(
                announced
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending(),
                "the announcement must wait to be collected"
            );
        }

        let mut again = pin!(announcements.announce(&a, offered));
        assert!(
            again
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending(),
            "an abandoned announcement must leave its id free"
        );
        let call_tx = announcements.take(&a).expect("the id must be announced");
        assert!(
            !call_tx.is_closed(),
            "a collection must reach the live announcement, not the abandoned one"
        );
    }

    #[tokio::test]
    async fn a_duplicate_announcement_is_refused() {
        let announcements = table();
        let a = ticket(1);
        let mut live = pin!(announcements.announce(&a, offered));
        assert!(
            live.as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending(),
            "the announcement must wait to be collected"
        );

        let Err(refused) = announcements.announce(&a, offered).await else {
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
        let announcements = table();
        let a = ticket(1);

        let Err(refused) = announcements
            .announce(&a, async || Err(Status::unavailable("registration closed")))
            .await
        else {
            panic!("an offer that cannot be sent leaves nobody to collect");
        };
        assert_eq!(refused, AnnounceError::SessionClosed);
        assert!(
            announcements.take(&a).is_none(),
            "an announcement the client never heard must be withdrawn"
        );
    }

    #[tokio::test]
    async fn an_uncollected_announcement_stalls_at_the_claim_stage() {
        let watchdog = Arc::new(Watchdog::new(Limits {
            claim: Duration::ZERO,
            ..Limits::default()
        }));
        let unclaimed = Announcements::<(), ()>::new(CancellationToken::new(), watchdog.clone());
        let a = ticket(1);

        let Err(refused) = unclaimed.announce(&a, offered).await else {
            panic!("an offer nobody collects must end the announcement");
        };
        assert_eq!(refused, AnnounceError::CollectionTimedOut);
        assert_eq!(
            watchdog.stalled().await,
            Stage::Claim,
            "the stall must be recorded against the claim stage"
        );
    }
}
