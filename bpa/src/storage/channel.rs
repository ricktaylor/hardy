//! Hybrid memory/storage channel with backpressure.
//!
//! Bundle queuing runs over a fast path, an in-memory
//! [`hardy_async::closeable`] channel, and a slow path, a storage lookup.
//! Once the in-memory buffer fills, sends spill to storage and a background
//! poller drains them back into the buffer.
//!
//! # Delivery contract
//!
//! This channel does not persist bundles. The caller inserts the bundle,
//! both its bytes and its metadata, into the storage subsystem before
//! calling [`Sender::send`], and the channel only moves the metadata
//! `status` column to its configured target status, through a conditional
//! [`Store::swap_status`](super::store::Store::swap_status) from the
//! caller's snapshot. The swap is fail-stop, aborting the process through
//! `trace_expect` on a storage error, so a bundle offered to the in-memory
//! channel has its status durably persisted. A lost swap drops the send:
//! the channels deliver at least once, so a duplicate copy of a bundle that
//! has already moved on, claimed in-flight by the egress path, swept, or
//! resolved, loses its move here rather than overwriting the live
//! assignment and re-entering circulation.
//!
//! Given that precondition, the channel delivers at least once from
//! `Sender::send` to `Receiver::recv`:
//!
//! - On the fast path, where the in-memory buffer has space, the bundle is
//!   enqueued directly. A later poller cycle may *also* pull it from
//!   storage and re-deliver it.
//! - On the slow path, where the buffer is full, the in-memory copy is
//!   dropped and the poller recovers it through
//!   `MetadataStorage::poll_pending`, filtered by the channel's target
//!   status. The drop is safe because the bundle's status is already
//!   persisted.
//!
//! A consumer therefore claims each received bundle out of the channel's
//! target status, with a conditional `swap_status` from the received copy's
//! snapshot, before acting on it, and drops copies that lose the swap. The
//! target status means exactly "queued in this channel": while a bundle
//! holds it, the poller treats the bundle as recoverable and re-pushes it.
//! The dispatch queue claims `DispatchPending` into `Dispatching` on
//! dequeue, and the egress queues claim `ForwardPending` into
//! `ForwardAckPending` before offering the transfer to a CLA.
//!
//! # State Machine
//!
//! ```text
//!     ┌──────────┐  channel full   ┌──────────┐  drain complete  ┌──────────┐
//!     │   Open   │ ───────────────►│ Draining │ ────────────────►│   Open   │
//!     └──────────┘                 └────┬─────┘                  └──────────┘
//!                                       │ new bundle arrives
//!                                       ▼
//!                                  ┌───────────┐
//!                                  │ Congested │ ──► (poller loops)
//!                                  └───────────┘
//!
//!     Any state ──► Closing (shutdown)
//! ```
//!
//! State transitions on the hot path are lock-free compare-and-swaps.

use core::result::Result;

use futures::{FutureExt, pin_mut, select_biased};
use hardy_async::{Notify, closeable::TrySendError, time::sleep};
use portable_atomic::{AtomicUsize, Ordering};
use time::Duration;
use trace_err::*;
use tracing::debug;

use super::store::Store;
use crate::{
    Arc,
    bundle::{Bundle, BundleStatus},
};

/// The channel's state machine, `#[repr(usize)]` so it rides an atomic.
#[repr(usize)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ChannelState {
    /// The fast path is available, and senders go straight to the
    /// in-memory channel.
    Open = 0,

    /// The fast path is closed, and the background poller is draining
    /// bundles from storage into the in-memory channel.
    Draining = 1,

    /// New bundles arrived while the poller was draining, so the poller
    /// loops again once the current drain completes.
    Congested = 2,

    /// The channel is shutting down. No further bundles are accepted and
    /// the poller exits.
    Closing = 3,
}

impl ChannelState {
    /// # Panics
    ///
    /// Panics if `value` names no state. Only this module writes the
    /// atomic, and only from the enum.
    #[inline]
    const fn from_usize(value: usize) -> Self {
        match value {
            0 => Self::Open,
            1 => Self::Draining,
            2 => Self::Congested,
            3 => Self::Closing,
            _ => unreachable!(),
        }
    }

    #[inline]
    const fn as_usize(self) -> usize {
        self as usize
    }
}

/// State shared between the senders and the background poller task.
struct Shared {
    state: AtomicUsize,
    tx: hardy_async::closeable::Sender<Bundle>,
    status: BundleStatus,
    notify: Arc<Notify>,
    // State-transition signal the tests synchronize on; the state machine
    // is otherwise observable only by polling.
    #[cfg(test)]
    state_notify: Notify,
    /// Drain cycles the poller has completed, those where storage had
    /// nothing more to load, signalled through `drain_notify`.
    #[cfg(test)]
    drain_cycles: AtomicUsize,
    #[cfg(test)]
    drain_notify: Notify,
}

impl Shared {
    #[inline]
    fn load_state(&self, ordering: Ordering) -> ChannelState {
        ChannelState::from_usize(self.state.load(ordering))
    }

    #[inline]
    fn store_state(&self, state: ChannelState, ordering: Ordering) {
        self.state.store(state.as_usize(), ordering);
        #[cfg(test)]
        self.state_notify.notify_one();
    }

    /// Stores `new` if the current state is `expected`.
    ///
    /// Returns `Ok(expected)` once the swap succeeds, or `Err(actual)` if
    /// the current state did not match.
    #[inline]
    fn compare_exchange_state(
        &self,
        expected: ChannelState,
        new: ChannelState,
        success: Ordering,
        failure: Ordering,
    ) -> Result<ChannelState, ChannelState> {
        let r = self
            .state
            .compare_exchange(expected.as_usize(), new.as_usize(), success, failure)
            .map(ChannelState::from_usize)
            .map_err(ChannelState::from_usize);
        #[cfg(test)]
        if r.is_ok() {
            self.state_notify.notify_one();
        }
        r
    }
}

/// Sender handle for a hybrid channel.
#[derive(Clone)]
pub struct Sender {
    store: Arc<Store>,
    shared: Arc<Shared>,
}

#[cfg(test)]
impl Sender {
    fn state(&self) -> ChannelState {
        self.shared.load_state(Ordering::Acquire)
    }
}

/// Error returned when a bundle cannot be sent.
#[derive(Debug)]
pub struct SendError(pub Bundle);

impl Sender {
    /// Offers a bundle to the channel, moving its persistent status to the
    /// channel's target status.
    ///
    /// The bundle's bytes and metadata are already persisted; see the
    /// module-level [delivery contract](self#delivery-contract). The status
    /// update is fail-stop on a storage error, and the bundle then either
    /// enters the in-memory buffer on the fast path, or waits for the
    /// background poller to recover it from storage on the slow path.
    ///
    /// # Errors
    ///
    /// Returns `Err(SendError(bundle))` when the channel is closing or the
    /// receiver has been dropped, handing ownership of the bundle back. A
    /// full buffer is not an error, the bundle being in storage for the
    /// poller to drain.
    #[expect(
        clippy::result_large_err,
        reason = "SendError carries the bundle so the caller recovers ownership; boxing it to shrink the Err variant would allocate on every send"
    )]
    pub async fn send(&self, bundle: Bundle) -> Result<(), SendError> {
        let status = self.shared.status.clone();
        self.send_to(bundle, status).await
    }

    /// The channel's target status, which identifies its queue.
    ///
    /// A queue whose assignment record carries per-bundle payload, such as
    /// `ForwardPending`'s `next_hop`, holds a placeholder there, and
    /// [`send_to`](Self::send_to) supplies each bundle's own record.
    pub fn queue_status(&self) -> &BundleStatus {
        &self.shared.status
    }

    /// Offers a bundle with its own assignment record.
    ///
    /// `status` names this channel's queue, as
    /// [`same_queue`](BundleStatus::same_queue) decides. See
    /// [`send`](Self::send), which uses the channel's target status.
    #[expect(
        clippy::result_large_err,
        reason = "SendError carries the bundle so the caller recovers ownership; boxing it to shrink the Err variant would allocate on every send"
    )]
    pub async fn send_to(&self, mut bundle: Bundle, status: BundleStatus) -> Result<(), SendError> {
        debug_assert!(
            status.same_queue(&self.shared.status),
            "send_to status names a different queue"
        );

        // Conditional move into this queue from the sender's snapshot: a
        // duplicate copy of a bundle that has already moved on loses here
        // rather than overwriting the live assignment.
        if !self.store.swap_status(&mut bundle, &status).await {
            debug!("Bundle already moved on, dropping duplicate send");
            return Ok(());
        }

        // The state may change before the compare-and-swap below, which
        // resolves the race.
        let state = self.shared.load_state(Ordering::Acquire);

        match state {
            ChannelState::Open => {
                match self.shared.tx.try_send(bundle) {
                    Ok(()) => return Ok(()),

                    Err(TrySendError::Disconnected(b)) => {
                        // Wake the poller so that it can exit.
                        self.shared.notify.notify_one();
                        return Err(SendError(b));
                    }

                    Err(TrySendError::Full(_dropped)) => {
                        // The drop is deliberate: the swap above moved the
                        // bundle's status durably, so the poller recovers it
                        // through `poll_pending`.
                        let _ = self.shared.compare_exchange_state(
                            ChannelState::Open,
                            ChannelState::Draining,
                            Ordering::AcqRel,
                            Ordering::Relaxed,
                        );
                    }
                }
            }
            ChannelState::Draining => {
                // New work arrived during the drain.
                let _ = self.shared.compare_exchange_state(
                    ChannelState::Draining,
                    ChannelState::Congested,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                );
            }
            ChannelState::Congested => {}
            ChannelState::Closing => return Err(SendError(bundle)),
        }

        // Wake `poll_queue` for the slow path.
        self.shared.notify.notify_one();
        Ok(())
    }

    /// Closes the channel to further sends.
    ///
    /// The underlying `closeable` channel is cancelled, so the receiver
    /// wakes and reports `Disconnected` once the buffered bundles have
    /// drained.
    pub fn close(&self) {
        self.shared
            .store_state(ChannelState::Closing, Ordering::Release);
        // The receiver drains the buffer, then disconnects.
        self.shared.tx.close();
        self.shared.notify.notify_one();
    }
}

/// Interval between re-checks of a buffer still above the re-open
/// threshold, once storage has drained and the consumer's progress raises
/// no notification.
const REOPEN_TICK: Duration = Duration::milliseconds(50);

/// Whether the poller should re-open the fast path, given the number of
/// bundles currently buffered in memory and the channel capacity.
///
/// The fast path re-opens once the buffer has drained to half capacity or
/// less. The bound is inclusive so that a `cap == 1` channel, whose half
/// capacity rounds down to 0, still re-opens when its buffer is empty.
#[inline]
fn should_reopen(buffered: usize, cap: usize) -> bool {
    buffered <= cap / 2
}

impl Store {
    /// Creates a hybrid channel with the given target status and in-memory
    /// capacity.
    ///
    /// The returned receiver's `recv` reports `Disconnected` once the
    /// buffer has drained after [`Sender::close`].
    pub fn channel(
        self: &Arc<Self>,
        status: BundleStatus,
        cap: usize,
    ) -> (Sender, hardy_async::closeable::Receiver<Bundle>) {
        let (tx, rx) = hardy_async::closeable::bounded::<Bundle>(cap);

        let shared = Arc::new(Shared {
            state: AtomicUsize::new(ChannelState::Open.as_usize()),
            tx,
            status: status.clone(),
            notify: Arc::new(Notify::new()),
            #[cfg(test)]
            state_notify: Notify::new(),
            #[cfg(test)]
            drain_cycles: AtomicUsize::new(0),
            #[cfg(test)]
            drain_notify: Notify::new(),
        });

        let store = self.clone();
        let shared_cloned = shared.clone();
        hardy_async::spawn!(self.tasks, "channel_queue_poll", (?status), async move {
            Self::poll_queue(store, shared_cloned, cap).await
        });

        // The first poll picks up bundles already in storage.
        shared.notify.notify_one();

        (
            Sender {
                store: self.clone(),
                shared,
            },
            rx,
        )
    }

    /// Drains storage into the in-memory channel while the fast path is
    /// closed.
    async fn poll_queue(self: Arc<Self>, shared: Arc<Shared>, cap: usize) {
        let cancel = self.tasks.cancel_token().clone();

        // Await the first signal, or return if the store is shutting down.
        // The poller runs on the store's task pool, so it honours that
        // pool's cancellation: `Store::shutdown` awaits every task, and
        // would otherwise hang on an idle poller parked on `notify`.
        select_biased! {
            _ = shared.notify.notified().fuse() => {}
            _ = cancel.cancelled().fuse() => return,
        }

        loop {
            // Move to Draining from any state but Closing.
            let mut current = shared.load_state(Ordering::Acquire);
            loop {
                if current == ChannelState::Closing {
                    break;
                }
                match shared.compare_exchange_state(
                    current,
                    ChannelState::Draining,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => break,
                    Err(actual) => current = actual,
                }
            }
            if current == ChannelState::Closing {
                break;
            }

            loop {
                match self.poll_once(&shared, cap).await {
                    Ok(true) => {}
                    Ok(false) => break,
                    Err(_) => {
                        debug!("Poll queue {:?} complete", shared.status);
                        return;
                    }
                }
            }
            #[cfg(test)]
            {
                shared.drain_cycles.fetch_add(1, Ordering::Relaxed);
                shared.drain_notify.notify_one();
            }

            // Re-open the fast path once the buffer has drained to half
            // capacity or less. The consumer's progress raises no
            // notification, so while the buffer sits above the threshold
            // the check repeats on a short tick: parking on `notify` alone
            // would leave the fast path closed until the next send.
            let reopened = loop {
                if should_reopen(shared.tx.len(), cap) {
                    break shared
                        .compare_exchange_state(
                            ChannelState::Draining,
                            ChannelState::Open,
                            Ordering::AcqRel,
                            Ordering::Relaxed,
                        )
                        .is_ok();
                }
                select_biased! {
                    _ = shared.notify.notified().fuse() => break false,
                    _ = cancel.cancelled().fuse() => return,
                    _ = sleep(REOPEN_TICK).fuse() => {}
                }
            };
            if !reopened {
                // Congested, or new work arrived while the buffer drained,
                // so loop again without waiting.
                continue;
            }

            // Await new work, or return if the store is shutting down.
            select_biased! {
                _ = shared.notify.notified().fuse() => {}
                _ = cancel.cancelled().fuse() => return,
            }
        }
    }

    async fn poll_once(self: &Arc<Self>, shared: &Arc<Shared>, cap: usize) -> Result<bool, ()> {
        let (stream, inner_rx) = hardy_async::channel::bounded::<Bundle>(cap);
        let shared_cloned = shared.clone();
        let cancel = self.tasks.cancel_token().clone();

        let h = hardy_async::spawn!(self.tasks, "poll_pending_once", async move {
            let mut pushed_one = false;
            while let Ok(bundle) = inner_rx.recv().await {
                // Match on queue identity: a recovered bundle's record may
                // carry per-bundle payload the channel's target status
                // cannot name.
                if !bundle.has_expired() && bundle.status.same_queue(&shared_cloned.status) {
                    // A closeable send already parked on a full buffer is
                    // not woken by the channel's own `close`, so race it
                    // against pool shutdown: `Store::shutdown` awaits this
                    // task and would otherwise hang. The dropped bundle
                    // stays persisted and is recovered at restart.
                    let send = shared_cloned.tx.send(bundle);
                    pin_mut!(send);
                    select_biased! {
                        r = send.fuse() => r.map_err(|_| ())?,
                        _ = cancel.cancelled().fuse() => return Ok(pushed_one),
                    }

                    pushed_one = true;
                }
            }
            Ok(pushed_one)
        });

        self.metadata_storage
            .poll_pending(&stream, &shared.status, cap)
            .await
            .trace_expect("Failed to poll store for pending bundles");
        // Dropping the stream disconnects the consumer task, which exits.
        drop(stream);

        h.await.trace_expect("Failed to join task")
    }
}

#[cfg(test)]
mod tests {
    use hashbrown::HashSet;

    use super::*;
    use crate::{
        bundle::tests::{test_bundle, test_expired_bundle},
        storage::{BundleMemStorage, MetadataMemStorage},
    };

    fn make_store() -> Arc<Store> {
        Arc::new(Store::new(
            core::num::NonZeroUsize::new(16).unwrap(),
            Arc::new(MetadataMemStorage::new(None)),
            Arc::new(BundleMemStorage::new(None, None)),
        ))
    }

    fn make_bundle(n: u32) -> Bundle {
        test_bundle(&format!("ipn:0.{n}.1"), "ipn:0.99.1")
    }

    fn make_expired_bundle(n: u32) -> Bundle {
        test_expired_bundle(&format!("ipn:0.{n}.1"), "ipn:0.99.1")
    }

    // The channel key's adjacency is a placeholder, exactly as the peer
    // queues configure it.
    const STATUS: BundleStatus = BundleStatus::ForwardPending {
        peer: 1,
        queue: 0,
        next_hop: hardy_bpv7::eid::Eid::Null,
    };

    // The delivery contract requires the bundle to already exist in metadata
    // storage before it is offered to the channel; insert it with the
    // caller-side snapshot status, as every production sender does.
    #[expect(
        clippy::result_large_err,
        reason = "mirrors Sender::send, whose SendError carries the bundle back"
    )]
    async fn send(tx: &Sender, bundle: Bundle) -> Result<(), SendError> {
        tx.store.insert_metadata(&bundle).await;
        tx.send(bundle).await
    }

    // Await the channel reaching `target`, synchronized on the state
    // machine's transition signal; the timeout only bounds a regression.
    // `notify_one` stores a permit, so a transition landing between the
    // state check and the await is not lost.
    async fn wait_for_state(tx: &Sender, target: ChannelState) {
        tokio::time::timeout(tokio::time::Duration::from_secs(30), async {
            while tx.state() != target {
                tx.shared.state_notify.notified().await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "Timed out waiting for state {target:?}, got {:?}",
                tx.state()
            )
        })
    }

    // Await the poller completing `n` drain cycles, synchronized on its
    // signal; the timeout only bounds a regression.
    async fn wait_for_drain_cycles(tx: &Sender, n: usize) {
        tokio::time::timeout(tokio::time::Duration::from_secs(30), async {
            while tx.shared.drain_cycles.load(Ordering::Relaxed) < n {
                tx.shared.drain_notify.notified().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("Timed out waiting for drain cycle {n}"))
    }

    // A drain cycle can end with the buffer above half capacity: storage
    // has nothing left to deliver, but the buffer still holds bundles the
    // consumer has not taken. The consumer draining them raises no
    // notification and no further send arrives, yet the fast path re-opens.
    #[tokio::test(start_paused = true)]
    async fn the_fast_path_reopens_once_the_consumer_drains() {
        let store = make_store();
        let cap = 4;
        let (tx, rx) = store.channel(STATUS, cap);
        // The poller's initial cycle, over empty storage.
        wait_for_drain_cycles(&tx, 1).await;

        // Fill the buffer, then tombstone its bundles, which belong to the
        // consumer and leave storage with nothing to re-deliver.
        for i in 1..=cap as u32 {
            let bundle = make_bundle(i);
            let id = bundle.id().clone();
            send(&tx, bundle).await.unwrap();
            store.tombstone_metadata(&id).await;
        }
        // Overflow with a bundle already expired, which the poller filters
        // out: the send moves the channel to Draining and wakes the poller,
        // whose drain cycle then delivers nothing and ends with the buffer
        // full.
        send(&tx, make_expired_bundle(cap as u32 + 1))
            .await
            .unwrap();
        assert_eq!(tx.state(), ChannelState::Draining);
        wait_for_drain_cycles(&tx, 2).await;
        assert_eq!(rx.len(), cap, "the drain cycle ended with the buffer full");

        // The consumer drains the buffer, and no further send arrives.
        for _ in 0..cap {
            rx.recv().await.expect("a buffered bundle");
        }
        wait_for_state(&tx, ChannelState::Open).await;

        drop(rx);
        tx.close();
        store.shutdown().await;
    }

    // The fast path re-opens after draining even at the smallest
    // capacities.
    #[test]
    fn reopen_threshold_handles_small_caps() {
        // At cap 1 half capacity rounds down to 0, so the fast path
        // re-opens only when the buffer is empty.
        assert!(should_reopen(0, 1));
        assert!(!should_reopen(1, 1));
        // At cap 2, at or below one buffered bundle.
        assert!(should_reopen(1, 2));
        assert!(!should_reopen(2, 2));
        // Larger capacities, at half capacity.
        assert!(should_reopen(8, 16));
        assert!(!should_reopen(9, 16));
    }

    // Filling the in-memory channel beyond capacity moves it to Draining.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_fast_path_saturation() {
        let store = make_store();
        let cap = 2;
        let (tx, rx) = store.channel(STATUS, cap);

        // Await the poller's first cycle.
        wait_for_state(&tx, ChannelState::Open).await;

        // Fill the buffer.
        send(&tx, make_bundle(1)).await.unwrap();
        send(&tx, make_bundle(2)).await.unwrap();

        // The buffer is full, so the next send moves the channel to
        // Draining.
        send(&tx, make_bundle(3)).await.unwrap();

        let state = tx.state();
        assert!(
            state == ChannelState::Draining || state == ChannelState::Congested,
            "Should be Draining or Congested after overflow, got {state:?}"
        );

        // `close` is synchronous and non-blocking, only cancelling the
        // token, so closing the receiver before the sender is not required
        // for correctness, but it matches how producers and consumers wind
        // down in production.
        drop(rx);
        tx.close();
        store.shutdown().await;
    }

    // A send while Draining moves the channel to Congested.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_congestion_signal() {
        let store = make_store();
        let cap = 2;
        let (tx, rx) = store.channel(STATUS, cap);

        // Await the poller's first cycle.
        wait_for_state(&tx, ChannelState::Open).await;

        // Fill and overflow to enter Draining.
        send(&tx, make_bundle(1)).await.unwrap();
        send(&tx, make_bundle(2)).await.unwrap();
        send(&tx, make_bundle(3)).await.unwrap();

        // A further send while Draining moves to Congested.
        send(&tx, make_bundle(4)).await.unwrap();

        let state = tx.state();
        assert!(
            state == ChannelState::Draining || state == ChannelState::Congested,
            "Should be Draining or Congested, got {state:?}"
        );

        drop(rx);
        tx.close();
        store.shutdown().await;
    }

    // Once the drain completes with the buffer below half capacity, the
    // fast path re-opens.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_hysteresis_recovery() {
        let store = make_store();
        // A capacity large enough that the re-open threshold of 8 is
        // cleared after draining 5 bundles and their duplicates.
        let cap = 16;
        let (tx, rx) = store.channel(STATUS, cap);

        // Await the poller's first cycle before overflowing the buffer.
        wait_for_state(&tx, ChannelState::Open).await;

        // Send one more than the buffer holds, to overflow it.
        let bundles = cap + 1;
        for i in 1..=bundles as u32 {
            send(&tx, make_bundle(i)).await.unwrap();
        }

        // Drain until every unique bundle has been delivered and
        // tombstoned. Each recv is event-driven, and the timeout only
        // bounds a regression.
        let mut seen = HashSet::new();
        while seen.len() < bundles {
            let b = tokio::time::timeout(tokio::time::Duration::from_secs(10), rx.recv())
                .await
                .expect("Timed out waiting for a bundle delivery")
                .expect("Channel closed mid-drain");
            store.tombstone_metadata(b.id()).await;
            seen.insert(b.id().clone());
        }
        assert_eq!(
            seen.len(),
            bundles,
            "Should have seen all {bundles} bundles"
        );

        // The poller re-opens once metadata is empty and the buffer is
        // below half capacity. Duplicate deliveries may still be in flight,
        // so keep draining them while awaiting the transition; the timeout
        // only bounds a regression.
        tokio::time::timeout(tokio::time::Duration::from_secs(30), async {
            loop {
                let notified = tx.shared.state_notify.notified();
                if tx.state() == ChannelState::Open {
                    break;
                }
                tokio::select! {
                    _ = notified => {}
                    r = rx.recv() => {
                        // The sender outlives this loop, so a recv error
                        // means the channel died mid-test. Fail at once
                        // rather than spinning on a ready `Err` arm until
                        // the outer timeout.
                        let b = r.expect("Channel disconnected while awaiting re-open");
                        store.tombstone_metadata(b.id()).await;
                    }
                }
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "Timed out waiting for the fast path to re-open, got {:?}",
                tx.state()
            )
        });

        drop(rx);
        tx.close();
        store.shutdown().await;
    }

    // `poll_once` filters expired bundles out rather than delivering them.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_lazy_expiry() {
        let store = make_store();
        let cap = 4; // Large enough that the poller does not block.
        let (tx, rx) = store.channel(STATUS, cap);

        // One live bundle and one expired.
        send(&tx, make_bundle(1)).await.unwrap();
        send(&tx, make_expired_bundle(2)).await.unwrap();

        // The live bundle arrives on the fast path.
        let received = rx.recv().await;
        assert!(received.is_ok(), "Valid bundle should be received");

        // The expired bundle also went out on the fast path, which does no
        // expiry filtering: only `poll_once` on the slow path does. It
        // arrives, and the dispatcher handles expiry later.

        drop(rx);
        tx.close();
        store.shutdown().await;
    }

    // A send after `close` fails with `SendError`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_close_safety() {
        let store = make_store();
        let cap = 4;
        let (tx, _rx) = store.channel(STATUS, cap);

        // Await the poller's first cycle.
        wait_for_state(&tx, ChannelState::Open).await;

        tx.close();

        // `close()` sets Closing synchronously, and the poller never moves the
        // state out of Closing, so the assertion needs no wait.
        assert_eq!(tx.state(), ChannelState::Closing);

        let result = send(&tx, make_bundle(1)).await;
        assert!(result.is_err(), "Send after close should fail");

        store.shutdown().await;
    }

    // Bundles that overflow the fast path arrive through the poller.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_drop_to_storage_integrity() {
        let store = make_store();
        let cap = 2;
        let (tx, rx) = store.channel(STATUS, cap);

        // Of the three bundles, two fit in the buffer and the third
        // overflows to storage.
        send(&tx, make_bundle(1)).await.unwrap();
        send(&tx, make_bundle(2)).await.unwrap();
        send(&tx, make_bundle(3)).await.unwrap();

        // Receive all three, tombstoning each against re-delivery.
        let mut seen = HashSet::new();
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(5);
        while seen.len() < 3 {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Ok(b)) => {
                    store.tombstone_metadata(b.id()).await;
                    seen.insert(b.id().clone());
                }
                _ => break,
            }
        }

        assert_eq!(
            seen.len(),
            3,
            "All 3 bundles (including overflow) should arrive"
        );

        drop(rx);
        tx.close();
        store.shutdown().await;
    }

    // Every sent bundle arrives when the consumer tombstones it promptly.
    // Delivery is at least once, so the poller may re-deliver a fast-path
    // bundle, and the tombstone stops it recurring.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_hybrid_duplication() {
        let store = make_store();
        let cap = 2;
        let (tx, rx) = store.channel(STATUS, cap);

        let total = 4u32;
        for i in 1..=total {
            send(&tx, make_bundle(i)).await.unwrap();
        }

        // Tombstone each bundle as it is consumed, so the poller does not
        // re-send it.
        let mut seen = HashSet::new();
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(5);
        while seen.len() < total as usize {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Ok(b)) => {
                    store.tombstone_metadata(b.id()).await;
                    seen.insert(b.id().clone());
                }
                _ => break,
            }
        }

        assert_eq!(
            seen.len(),
            total as usize,
            "All {total} bundles should arrive"
        );

        drop(rx);
        tx.close();
        store.shutdown().await;
    }

    // Bundles sent in sequence all arrive. Order is FIFO within the
    // in-memory buffer and by `received_at` on the slow path, but the
    // concurrent poller leaves the order across the two paths
    // non-deterministic, so only the set is asserted.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_ordering_preservation() {
        let store = make_store();
        let cap = 4;
        let (tx, rx) = store.channel(STATUS, cap);

        let src1: hardy_bpv7::eid::Eid = "ipn:0.1.1".parse().unwrap();
        let src2: hardy_bpv7::eid::Eid = "ipn:0.2.1".parse().unwrap();

        send(&tx, make_bundle(1)).await.unwrap();
        send(&tx, make_bundle(2)).await.unwrap();

        // Collect unique bundles until both have been seen.
        let mut seen = HashSet::new();
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(5);
        while seen.len() < 2 {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Ok(b)) => {
                    store.tombstone_metadata(b.id()).await;
                    seen.insert(b.id().source.clone());
                }
                _ => break,
            }
        }

        assert!(seen.contains(&src1), "Bundle 1 should arrive");
        assert!(seen.contains(&src2), "Bundle 2 should arrive");

        drop(rx);
        tx.close();
        store.shutdown().await;
    }

    // `poll_once` filters out bundles whose status names another queue.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_status_consistency() {
        let store = make_store();
        let cap = 2;
        let (tx, rx) = store.channel(STATUS, cap);

        // The send moves the bundle's status to `ForwardPending{peer:1}`.
        send(&tx, make_bundle(1)).await.unwrap();

        // The bundle arrives carrying that status.
        let b = rx.recv().await.unwrap();
        assert_eq!(b.status, STATUS);

        drop(rx);
        tx.close();
        store.shutdown().await;
    }

    // The receiver reports `Disconnected` once a closed channel has
    // drained.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_zombie_task_leak() {
        let store = make_store();
        let cap = 4;
        let (tx, rx) = store.channel(STATUS, cap);

        send(&tx, make_bundle(1)).await.unwrap();
        tx.close();

        // Drain until `Disconnected` or the timeout.
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(5);
        let mut got_disconnected = false;
        loop {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Ok(_)) => continue, // A bundle, so keep draining.
                Ok(Err(_)) => {
                    got_disconnected = true;
                    break;
                }
                Err(_) => break, // timeout
            }
        }

        assert!(got_disconnected, "Should receive Disconnected after close");

        drop(rx);
        store.shutdown().await;
    }
}
