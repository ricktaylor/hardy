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

use super::*;
use core::result::Result;
use core::sync::atomic::{AtomicUsize, Ordering};

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
            _ => panic!("invalid ChannelState value"),
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
    tx: flume::Sender<bundle::Bundle>,
    status: metadata::BundleStatus,
    notify: Arc<hardy_async::Notify>,
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

    /// Atomically swap to a new state, returning the old state.
    #[inline]
    fn swap_state(&self, new: ChannelState, ordering: Ordering) -> ChannelState {
        ChannelState::from_usize(self.state.swap(new.as_usize(), ordering))
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

/// Error returned when a bundle cannot be sent.
pub struct SendError(pub bundle::Bundle);

impl Sender {
    /// Send a bundle, updating its status to match the channel's target status.
    pub async fn send(&self, mut bundle: bundle::Bundle) -> Result<(), SendError> {
        if bundle.metadata.status != self.shared.status {
            bundle.metadata.status = self.shared.status.clone();
            self.store.update_metadata(&bundle).await;
        }

        // State can change between load and CAS - this is fine, CAS handles races
        let state = self.shared.load_state(Ordering::Acquire);

        match state {
            // Fast path is open, try to send directly to the in-memory channel.
            ChannelState::Open => {
                match self.shared.tx.try_send(bundle) {
                    // Success! The bundle is sent, and we can return immediately.
                    Ok(()) => return Ok(()),

                    Err(flume::TrySendError::Disconnected(b)) => {
                        // Wake up the poller task so it can exit
                        self.shared.notify.notify_one();
                        return Err(SendError(b));
                    }

                    Err(flume::TrySendError::Full(_)) => {
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
    pub async fn close(&self) {
        self.shared
            .store_state(ChannelState::Closing, Ordering::Release);
        self.shared.notify.notify_one();
    }
}

/// Receiver handle (re-export of flume::Receiver).
pub type Receiver = flume::Receiver<bundle::Bundle>;

impl Store {
    /// Create a hybrid channel with the given target status and memory capacity.
    pub fn channel(
        self: &Arc<Self>,
        status: metadata::BundleStatus,
        cap: usize,
    ) -> (Sender, Receiver) {
        let (tx, rx) = flume::bounded::<bundle::Bundle>(cap);

        let shared = Arc::new(Shared {
            state: AtomicUsize::new(ChannelState::Open.as_usize()),
            tx,
            status: status.clone(),
            notify: Arc::new(hardy_async::Notify::new()),
        });

        let store = self.clone();
        let shared_cloned = shared.clone();
        hardy_async::spawn!(self.tasks, "channel_queue_poll", (?status), async move {
            Self::poll_queue(store, shared_cloned, cap).await
        });

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

            let old_state = shared.swap_state(ChannelState::Draining, Ordering::AcqRel);
            if old_state == ChannelState::Closing {
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
        let (inner_tx, inner_rx) = flume::bounded::<bundle::Bundle>(cap);
        let shared_cloned = shared.clone();

        let h = hardy_async::spawn!(self.tasks, "poll_pending_once", async move {
            let mut pushed_one = false;
            while let Ok(bundle) = inner_rx.recv_async().await {
                // Just do some checks
                if !bundle.has_expired() && bundle.metadata.status == shared_cloned.status {
                    // Send into queue
                    shared_cloned.tx.send_async(bundle).await.map_err(|_| ())?;

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
    // use super::*;

    // // TODO: Implement test for 'Fast Path Saturation' (Fill memory channel to trigger Draining state)
    // #[test]
    // fn test_fast_path_saturation() {
    //     todo!("Verify Fill memory channel to trigger Draining state");
    // }

    // // TODO: Implement test for 'Congestion Signal' (Send while Draining to trigger Congested state)
    // #[test]
    // fn test_congestion_signal() {
    //     todo!("Verify Send while Draining to trigger Congested state");
    // }

    // // TODO: Implement test for 'Hysteresis Recovery' (Verify fast path re-opens only after drain)
    // #[test]
    // fn test_hysteresis_recovery() {
    //     todo!("Verify fast path re-opens only after drain");
    // }

    // // TODO: Implement test for 'Lazy Expiry' (Verify expired bundles are dropped during poll)
    // #[test]
    // fn test_lazy_expiry() {
    //     todo!("Verify expired bundles are dropped during poll");
    // }

    // // TODO: Implement test for 'Close Safety' (Verify sends fail when closing)
    // #[test]
    // fn test_close_safety() {
    //     todo!("Verify sends fail when closing");
    // }

    // // TODO: Implement test for 'Drop-to-Storage Integrity' (Verify bundle dropped from memory is retrieved from persistent storage)
    // #[test]
    // fn test_drop_to_storage_integrity() {
    //     todo!("Verify bundle dropped from memory is retrieved from persistent storage");
    // }

    // // TODO: Implement test for 'Hybrid Duplication' (Verify bundles already in channel are not re-injected by poller)
    // #[test]
    // fn test_hybrid_duplication() {
    //     todo!("Verify bundles already in channel are not re-injected by poller");
    // }

    // // TODO: Implement test for 'Ordering Preservation' (Verify FIFO/Priority is maintained during mode switch)
    // #[test]
    // fn test_ordering_preservation() {
    //     todo!("Verify FIFO/Priority is maintained during mode switch");
    // }

    // // TODO: Implement test for 'Status Consistency' (Verify bundles with mismatched status are filtered)
    // #[test]
    // fn test_status_consistency() {
    //     todo!("Verify bundles with mismatched status are filtered");
    // }

    // // TODO: Implement test for 'Zombie Task Leak' (Verify poller task exits when Sender is dropped)
    // #[test]
    // fn test_zombie_task_leak() {
    //     todo!("Verify poller task exits when Sender is dropped");
    // }
}
