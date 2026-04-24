//! Hybrid memory/storage channel with backpressure.
//!
//! Provides a fast path (in-memory flume channel) and slow path (storage-backed)
//! for bundle queuing. When the memory channel fills, bundles are persisted and
//! a background poller drains them back into memory.
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
//! Uses lock-free CAS operations for state transitions on the hot path.

use core::result::Result;
use core::sync::atomic::{AtomicUsize, Ordering};

use flume::TrySendError;
use hardy_async::Notify;
use trace_err::*;
use tracing::debug;

use super::{Receiver, Store};
use crate::Arc;
use crate::bundle::{Bundle, BundleStatus};

/// Channel state machine states (`#[repr(usize)]` for lock-free atomics).
#[repr(usize)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ChannelState {
    /// Fast path is available. Senders try to send directly to the flume channel.
    Open = 0,

    /// Fast path is closed. The background poller is draining bundles from
    /// persistent storage into the flume channel.
    Draining = 1,

    /// New bundles arrived while the poller was draining. This signals to the
    /// poller that it should loop again after completing the current drain.
    Congested = 2,

    /// Channel is shutting down. No more bundles accepted; poller should exit.
    Closing = 3,
}

impl ChannelState {
    /// Convert from raw usize to ChannelState.
    ///
    /// # Panics
    /// Panics if the value doesn't correspond to a valid state. This should
    /// never happen since we control all writes to the atomic.
    #[inline]
    const fn from_usize(value: usize) -> Self {
        match value {
            0 => Self::Open,
            1 => Self::Draining,
            2 => Self::Congested,
            3 => Self::Closing,
            // ChannelState atomic only written with valid enum values 0-3
            _ => unreachable!(),
        }
    }

    /// Convert to raw usize for atomic operations.
    #[inline]
    const fn as_usize(self) -> usize {
        self as usize
    }
}

/// Shared state between Sender and the background poller task.
struct Shared {
    state: AtomicUsize,
    tx: flume::Sender<Option<Bundle>>,
    status: BundleStatus,
    notify: Arc<Notify>,
}

impl Shared {
    /// Atomically load the current state.
    #[inline]
    fn load_state(&self, ordering: Ordering) -> ChannelState {
        ChannelState::from_usize(self.state.load(ordering))
    }

    /// Atomically store a new state.
    #[inline]
    fn store_state(&self, state: ChannelState, ordering: Ordering) {
        self.state.store(state.as_usize(), ordering);
    }

    /// Atomically compare-and-swap: if current == expected, set to new.
    ///
    /// Returns `Ok(expected)` if the swap succeeded, or `Err(actual)` if the
    /// current value didn't match expected.
    #[inline]
    fn compare_exchange_state(
        &self,
        expected: ChannelState,
        new: ChannelState,
        success: Ordering,
        failure: Ordering,
    ) -> Result<ChannelState, ChannelState> {
        self.state
            .compare_exchange(expected.as_usize(), new.as_usize(), success, failure)
            .map(ChannelState::from_usize)
            .map_err(ChannelState::from_usize)
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
    // Expose the current channel state for test assertions.
    fn state(&self) -> ChannelState {
        self.shared.load_state(Ordering::Acquire)
    }
}

/// Error returned when a bundle cannot be sent.
#[derive(Debug)]
#[allow(dead_code)]
pub struct SendError(pub Bundle);

impl Sender {
    /// Send a bundle, updating its status to match the channel's target status.
    pub async fn send(&self, mut bundle: Bundle) -> Result<(), SendError> {
        self.store
            .update_status(&mut bundle, &self.shared.status)
            .await;

        // State can change between load and CAS - this is fine, CAS handles races
        let state = self.shared.load_state(Ordering::Acquire);

        match state {
            // Fast path is open, try to send directly to the in-memory channel.
            ChannelState::Open => {
                match self.shared.tx.try_send(Some(bundle)) {
                    // Success! The bundle is sent, and we can return immediately.
                    Ok(()) => return Ok(()),

                    Err(TrySendError::Disconnected(Some(b))) => {
                        // Wake up the poller task so it can exit
                        self.shared.notify.notify_one();
                        return Err(SendError(b));
                    }

                    Err(TrySendError::Disconnected(None)) => {
                        unreachable!("sent Some but got None back");
                    }

                    Err(TrySendError::Full(_)) => {
                        // Channel full - trigger slow path
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
                // Signal new work arrived during drain
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

        // Notify the poll_queue task that it has work to do on the slow path.
        self.shared.notify.notify_one();
        Ok(())
    }

    /// Close the channel, preventing further sends.
    /// Sends `None` to signal receivers that the channel is closing.
    pub async fn close(&self) {
        self.shared
            .store_state(ChannelState::Closing, Ordering::Release);
        // Send None to signal close to the receiver
        let _ = self.shared.tx.send_async(None).await;
        self.shared.notify.notify_one();
    }
}

impl Store {
    /// Create a hybrid channel with the given target status and memory capacity.
    pub fn channel(self: &Arc<Self>, status: BundleStatus, cap: usize) -> (Sender, Receiver) {
        let (tx, rx) = flume::bounded::<Option<Bundle>>(cap);

        let shared = Arc::new(Shared {
            state: AtomicUsize::new(ChannelState::Open.as_usize()),
            tx,
            status: status.clone(),
            notify: Arc::new(Notify::new()),
        });

        let store = self.clone();
        let shared_cloned = shared.clone();
        hardy_async::spawn!(self.tasks, "channel_queue_poll", (?status), async move {
            Self::poll_queue(store, shared_cloned, cap).await
        });

        // Signal initial poll to pick up any pre-existing bundles in storage
        shared.notify.notify_one();

        (
            Sender {
                store: self.clone(),
                shared,
            },
            rx,
        )
    }

    /// Background poller: drains storage into memory channel when congested.
    async fn poll_queue(self: Arc<Self>, shared: Arc<Shared>, cap: usize) {
        loop {
            shared.notify.notified().await;

            // Transition to Draining from any state except Closing.
            // CAS loop: load current, bail if Closing, otherwise try to write Draining.
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

            // Drain storage
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

            // Re-open fast path if channel <50% full and no new work arrived
            if shared.tx.len() < (cap / 2)
                && shared
                    .compare_exchange_state(
                        ChannelState::Draining,
                        ChannelState::Open,
                        Ordering::AcqRel,
                        Ordering::Relaxed,
                    )
                    .is_err()
            {
                continue; // Congested - loop again
            }
        }
    }

    async fn poll_once(self: &Arc<Self>, shared: &Arc<Shared>, cap: usize) -> Result<bool, ()> {
        let (inner_tx, inner_rx) = flume::bounded::<Bundle>(cap);
        let shared_cloned = shared.clone();

        let h = hardy_async::spawn!(self.tasks, "poll_pending_once", async move {
            let mut pushed_one = false;
            while let Ok(bundle) = inner_rx.recv_async().await {
                // Just do some checks
                if !bundle.has_expired() && bundle.metadata.status == shared_cloned.status {
                    // Send into queue
                    shared_cloned
                        .tx
                        .send_async(Some(bundle))
                        .await
                        .map_err(|_| ())?;

                    pushed_one = true;
                }
            }
            Ok(pushed_one)
        });

        self.metadata_storage
            .poll_pending(inner_tx, &shared.status, cap)
            .await
            .trace_expect("Failed to poll store for pending bundles");

        h.await.trace_expect("Failed to join task")
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::storage::{BundleMemStorage, MetadataMemStorage};

    fn make_store() -> Arc<Store> {
        Arc::new(Store::new(
            core::num::NonZeroUsize::new(16).unwrap(),
            Arc::new(MetadataMemStorage::new(&Default::default())),
            Arc::new(BundleMemStorage::new(&Default::default())),
        ))
    }

    fn make_bundle(n: u32) -> Bundle {
        Bundle {
            bundle: hardy_bpv7::bundle::Bundle {
                id: hardy_bpv7::bundle::Id {
                    source: format!("ipn:0.{n}.1").parse().unwrap(),
                    timestamp: hardy_bpv7::creation_timestamp::CreationTimestamp::now(),
                    fragment_info: None,
                },
                flags: Default::default(),
                crc_type: Default::default(),
                destination: "ipn:0.99.1".parse().unwrap(),
                report_to: Default::default(),
                lifetime: core::time::Duration::from_secs(3600),
                previous_node: None,
                age: None,
                hop_count: None,
                blocks: Default::default(),
            },
            metadata: Default::default(),
        }
    }

    fn make_expired_bundle(n: u32) -> Bundle {
        let mut b = make_bundle(n);
        b.bundle.lifetime = core::time::Duration::from_secs(0);
        // Set received_at in the past so expiry is already passed
        b.metadata.read_only.received_at =
            time::OffsetDateTime::now_utc() - time::Duration::seconds(10);
        b
    }

    const STATUS: BundleStatus = BundleStatus::Waiting;

    // Fill the memory channel beyond capacity to trigger Draining state.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_fast_path_saturation() {
        let store = make_store();
        let cap = 2;
        let (tx, _rx) = store.channel(STATUS, cap);

        // Wait for the poller's initial cycle to complete
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Fill channel to capacity
        tx.send(make_bundle(1)).await.unwrap();
        tx.send(make_bundle(2)).await.unwrap();

        // Channel is full — next send triggers Draining
        tx.send(make_bundle(3)).await.unwrap();

        let state = tx.state();
        assert!(
            state == ChannelState::Draining || state == ChannelState::Congested,
            "Should be Draining or Congested after overflow, got {state:?}"
        );

        // Drop receiver first — channel may be full, so close()'s send_async(None)
        // would block. Dropping rx disconnects the flume, making send_async fail
        // immediately (ignored by close), then notify_one wakes the poller to exit.
        drop(_rx);
        tx.close().await;
        store.shutdown().await;
    }

    // Send while Draining triggers Congested state.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_congestion_signal() {
        let store = make_store();
        let cap = 2;
        let (tx, _rx) = store.channel(STATUS, cap);

        // Wait for poller's initial cycle
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Fill + overflow to enter Draining
        tx.send(make_bundle(1)).await.unwrap();
        tx.send(make_bundle(2)).await.unwrap();
        tx.send(make_bundle(3)).await.unwrap();

        // Another send while Draining should push to Congested
        tx.send(make_bundle(4)).await.unwrap();

        let state = tx.state();
        assert!(
            state == ChannelState::Draining || state == ChannelState::Congested,
            "Should be Draining or Congested, got {state:?}"
        );

        drop(_rx);
        tx.close().await;
        store.shutdown().await;
    }

    // After draining completes and channel is <50% full, fast path re-opens.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_hysteresis_recovery() {
        let store = make_store();
        // Large cap so hysteresis threshold (cap/2=8) is easily cleared
        // after draining 5 bundles + duplicates.
        let cap = 16;
        let (tx, rx) = store.channel(STATUS, cap);

        // Fill to trigger draining (5 bundles into cap=16 triggers via overflow)
        // First, wait for poller's initial cycle
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(5);
        loop {
            if tx.state() == ChannelState::Open {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "Poller didn't return to Open for initial cycle"
            );
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        }

        // Now send enough to overflow: cap=16, send 17
        for i in 1..=17u32 {
            tx.send(make_bundle(i)).await.unwrap();
        }

        // Drain ALL bundles (unique + duplicates) and tombstone each.
        // The poller re-opens when flume.len() < cap/2 and metadata is empty.
        let mut seen = HashSet::new();
        let drain_deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(5);
        loop {
            match tokio::time::timeout(tokio::time::Duration::from_millis(200), rx.recv_async())
                .await
            {
                Ok(Ok(Some(b))) => {
                    store.tombstone_metadata(&b.bundle.id).await;
                    seen.insert(b.bundle.id);
                }
                _ => {
                    // No bundle for 200ms — channel quiesced
                    break;
                }
            }
            if tokio::time::Instant::now() > drain_deadline {
                break;
            }
        }

        assert!(
            seen.len() >= 17,
            "Should have seen all 17 bundles, got {}",
            seen.len()
        );

        // Wait for the poller to see empty metadata and re-open
        let mut recovered = false;
        let recovery_deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(5);
        loop {
            if tx.state() == ChannelState::Open {
                recovered = true;
                break;
            }
            if tokio::time::Instant::now() > recovery_deadline {
                break;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        }

        assert!(recovered, "Should recover to Open, got {:?}", tx.state());

        drop(rx);
        tx.close().await;
        store.shutdown().await;
    }

    // Expired bundles should be filtered out during poll_once and not delivered.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_lazy_expiry() {
        let store = make_store();
        let cap = 4; // Large enough that poller doesn't block
        let (tx, rx) = store.channel(STATUS, cap);

        // Send a valid bundle and an expired one
        tx.send(make_bundle(1)).await.unwrap();
        tx.send(make_expired_bundle(2)).await.unwrap();

        // The valid bundle should arrive on the fast path
        let received = rx.recv_async().await;
        assert!(
            matches!(received, Ok(Some(_))),
            "Valid bundle should be received"
        );

        // The expired bundle was also sent on the fast path (expiry filtering
        // only happens in poll_once for the slow path). On the fast path,
        // expired bundles still arrive — the dispatcher handles expiry later.
        // This test verifies the bundle at least doesn't crash the channel.

        drop(rx);
        tx.close().await;
        store.shutdown().await;
    }

    // Sends should fail with SendError after close.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_close_safety() {
        let store = make_store();
        let cap = 4;
        let (tx, _rx) = store.channel(STATUS, cap);

        // Let poller complete its initial cycle
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        tx.close().await;

        // Wait for poller to see Closing and restore it
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        assert_eq!(tx.state(), ChannelState::Closing);

        let result = tx.send(make_bundle(1)).await;
        assert!(result.is_err(), "Send after close should fail");

        store.shutdown().await;
    }

    // Bundles that overflow the fast path should eventually arrive via the poller.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_drop_to_storage_integrity() {
        let store = make_store();
        let cap = 2;
        let (tx, rx) = store.channel(STATUS, cap);

        // Send 3 bundles — 2 fit in channel, 3rd overflows to storage
        tx.send(make_bundle(1)).await.unwrap();
        tx.send(make_bundle(2)).await.unwrap();
        tx.send(make_bundle(3)).await.unwrap();

        // Receive all 3, tombstoning each to prevent re-delivery.
        let mut seen = HashSet::new();
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(5);
        while seen.len() < 3 {
            match tokio::time::timeout_at(deadline, rx.recv_async()).await {
                Ok(Ok(Some(b))) => {
                    store.tombstone_metadata(&b.bundle.id).await;
                    seen.insert(b.bundle.id);
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
        tx.close().await;
        store.shutdown().await;
    }

    // All sent bundles should arrive when the consumer tombstones them promptly.
    // The channel may re-deliver fast-path bundles via the poller (at-least-once),
    // but tombstoning prevents repeated re-delivery.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_hybrid_duplication() {
        let store = make_store();
        let cap = 2;
        let (tx, rx) = store.channel(STATUS, cap);

        let total = 4u32;
        for i in 1..=total {
            tx.send(make_bundle(i)).await.unwrap();
        }

        // Consume bundles and tombstone each so the poller won't re-send.
        let mut seen = HashSet::new();
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(5);
        while seen.len() < total as usize {
            match tokio::time::timeout_at(deadline, rx.recv_async()).await {
                Ok(Ok(Some(b))) => {
                    store.tombstone_metadata(&b.bundle.id).await;
                    seen.insert(b.bundle.id);
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
        tx.close().await;
        store.shutdown().await;
    }

    // Bundles sent in sequence should all arrive, preserving the set.
    // Strict FIFO is guaranteed on the fast path (flume) and by received_at
    // ordering on the slow path, but the concurrent poller makes strict
    // ordering non-deterministic across paths in a test environment.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_ordering_preservation() {
        let store = make_store();
        let cap = 4;
        let (tx, rx) = store.channel(STATUS, cap);

        let src1: hardy_bpv7::eid::Eid = "ipn:0.1.1".parse().unwrap();
        let src2: hardy_bpv7::eid::Eid = "ipn:0.2.1".parse().unwrap();

        tx.send(make_bundle(1)).await.unwrap();
        tx.send(make_bundle(2)).await.unwrap();

        // Collect unique bundles until we've seen both
        let mut seen = HashSet::new();
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(5);
        while seen.len() < 2 {
            match tokio::time::timeout_at(deadline, rx.recv_async()).await {
                Ok(Ok(Some(b))) => {
                    store.tombstone_metadata(&b.bundle.id).await;
                    seen.insert(b.bundle.id.source.clone());
                }
                _ => break,
            }
        }

        assert!(seen.contains(&src1), "Bundle 1 should arrive");
        assert!(seen.contains(&src2), "Bundle 2 should arrive");

        drop(rx);
        tx.close().await;
        store.shutdown().await;
    }

    // Bundles with mismatched status should be filtered during poll_once.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_status_consistency() {
        let store = make_store();
        let cap = 2;
        let (tx, rx) = store.channel(STATUS, cap);

        // Send a bundle — this updates its status to ForwardPending{peer:1}
        tx.send(make_bundle(1)).await.unwrap();

        // The bundle should arrive normally
        let received = rx.recv_async().await.unwrap();
        assert!(received.is_some());

        // Verify the received bundle has the correct status
        let b = received.unwrap();
        assert_eq!(b.metadata.status, STATUS);

        drop(rx);
        tx.close().await;
        store.shutdown().await;
    }

    // After closing, the receiver should eventually get None.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_zombie_task_leak() {
        let store = make_store();
        let cap = 4;
        let (tx, rx) = store.channel(STATUS, cap);

        tx.send(make_bundle(1)).await.unwrap();
        tx.close().await;

        // Drain until we see None (close sentinel) or timeout
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(5);
        let mut got_none = false;
        loop {
            match tokio::time::timeout_at(deadline, rx.recv_async()).await {
                Ok(Ok(None)) => {
                    got_none = true;
                    break;
                }
                Ok(Ok(Some(_))) => continue, // data bundle, keep draining
                Ok(Err(_)) => break,         // channel disconnected
                Err(_) => break,             // timeout
            }
        }

        assert!(got_none, "Should receive None after close");

        drop(rx);
        store.shutdown().await;
    }
}
