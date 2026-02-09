use super::*;
use core::result::Result;
use core::sync::atomic::{AtomicUsize, Ordering};

// =============================================================================
// Channel State Machine
// =============================================================================
//
// This module implements a hybrid memory/storage channel with a lock-free state
// machine. The state machine controls whether bundles are sent via a fast path
// (direct to in-memory flume channel) or a slow path (persisted to storage and
// polled by a background task).
//
// ## Why Lock-Free?
//
// We use atomic Compare-And-Swap (CAS) operations instead of a mutex because:
//
// 1. **Performance**: CAS is a single CPU instruction (CMPXCHG on x86, LDXR/STXR
//    on ARM), whereas mutex acquisition involves syscalls under contention.
//
// 2. **No blocking**: Lock-free operations never block the calling thread. Even
//    under contention, all threads make progress (lock-free guarantee).
//
// 3. **Portability**: Using `#[repr(usize)]` with `AtomicUsize` ensures we use
//    the processor's native word size, which has guaranteed atomic operations
//    on all platforms. Sub-word atomics (u8) may require emulation on some
//    embedded architectures.
//
// 4. **Simplicity**: Our state transitions are simple enough that CAS is cleaner
//    than a mutex. We don't need mutual exclusion - we just need atomic state
//    transitions where "last writer wins" is acceptable.
//
// ## State Transitions
//
// ```text
//                    ┌─────────────────────────────────────────────┐
//                    │                                             │
//                    ▼                                             │
//     ┌──────────────────────────┐                                 │
//     │          Open            │ ◄───────────────────────────────┤
//     │  (fast path available)   │                                 │
//     └────────────┬─────────────┘                                 │
//                  │                                               │
//                  │ channel full (try_send fails)                 │
//                  ▼                                               │
//     ┌──────────────────────────┐                                 │
//     │        Draining          │ ─── drain complete ─────────────┘
//     │  (poller taking over)    │     (CAS: Draining → Open)
//     └────────────┬─────────────┘
//                  │
//                  │ new bundle arrives during drain
//                  ▼
//     ┌──────────────────────────┐
//     │        Congested         │ ─── (poller loops back to drain)
//     │  (work arrived while     │
//     │   draining)              │
//     └──────────────────────────┘
//
//                    ┌──────────────────────────┐
//     Any state ───► │        Closing           │
//                    │  (channel shutting down) │
//                    └──────────────────────────┘
// ```
//
// ## Memory Ordering
//
// We use the following orderings:
//
// - `Acquire` on loads: Ensures we see all writes that happened before the
//   corresponding Release store.
//
// - `Release` on stores: Ensures all our prior writes are visible to threads
//   that subsequently Acquire-load this value.
//
// - `AcqRel` on CAS: Combines Acquire (for the load) and Release (for the store).
//
// - `Relaxed` on CAS failure: The failed load doesn't establish synchronization.
//
// =============================================================================

/// Channel state machine states.
///
/// Using `#[repr(usize)]` ensures the enum has the same size as `AtomicUsize`,
/// allowing direct casting without conversion overhead. This also guarantees
/// native atomic operations on all architectures (some embedded platforms
/// lack native sub-word atomics for u8/u16).
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
    /// Atomic state machine controlling fast/slow path routing.
    ///
    /// We use `AtomicUsize` with lock-free CAS operations instead of a mutex
    /// because state transitions are simple (single enum value) and this is
    /// on the hot path for every bundle send.
    state: AtomicUsize,

    /// The in-memory channel for fast path sends.
    tx: flume::Sender<bundle::Bundle>,

    /// The bundle status that this channel handles (e.g., ForwardPending).
    status: metadata::BundleStatus,

    /// Notification primitive to wake the background poller.
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

#[derive(Clone)]
pub struct Sender {
    store: Arc<Store>,
    shared: Arc<Shared>,
}

pub struct SendError(pub bundle::Bundle);

impl Sender {
    pub async fn send(&self, mut bundle: bundle::Bundle) -> Result<(), SendError> {
        if bundle.metadata.status != self.shared.status {
            bundle.metadata.status = self.shared.status.clone();
            self.store.update_metadata(&bundle).await;
        }

        // ---------------------------------------------------------------------
        // Lock-free state machine dispatch
        // ---------------------------------------------------------------------
        //
        // We load the current state and handle each case. Note that the state
        // can change between our load and any subsequent operations - this is
        // intentional and correct:
        //
        // - If Open → Draining between load and try_send: Our try_send might
        //   still succeed (channel has room) or fail (full). Either is correct.
        //
        // - If we see Open but someone else already transitioned to Draining:
        //   Our CAS(Open → Draining) will fail, which is fine - someone else
        //   already did it.
        //
        // - Multiple concurrent CAS attempts are safe: exactly one succeeds,
        //   others fail and the overall state is still correct.
        //
        // ---------------------------------------------------------------------

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
                        // The channel is full. Transition to the Draining state to
                        // signal the slow path poller to take over.
                        //
                        // If this CAS fails, another sender already did the
                        // transition, which is fine - the poller will be notified
                        // and will drain the storage.
                        //
                        // We use AcqRel ordering:
                        // - Acquire: see any prior state changes
                        // - Release: our transition is visible to the poller
                        let _ = self.shared.compare_exchange_state(
                            ChannelState::Open,
                            ChannelState::Draining,
                            Ordering::AcqRel,
                            Ordering::Relaxed,
                        );
                    }
                }
            }

            // The poller is draining the store. If a new bundle arrives now, we move
            // to the Congested state to signal that new work arrived during the drain.
            //
            // If this CAS fails (state changed to Congested or Open), that's
            // fine - either another sender already signaled congestion, or
            // the poller finished and reopened the fast path.
            ChannelState::Draining => {
                let _ = self.shared.compare_exchange_state(
                    ChannelState::Draining,
                    ChannelState::Congested,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                );
            }

            // The channel is already congested. We don't need to do anything further;
            // the notification will ensure the poller runs again.
            ChannelState::Congested => {}

            // The channel is closing down. We cannot accept new bundles.
            ChannelState::Closing => {
                return Err(SendError(bundle));
            }
        }

        // Notify the poll_queue task that it has work to do on the slow path.
        self.shared.notify.notify_one();
        Ok(())
    }

    pub async fn close(&self) {
        // Mark the channel as closing.
        //
        // We use Release ordering to ensure any prior bundle writes are visible
        // to the poller when it sees the Closing state.
        self.shared
            .store_state(ChannelState::Closing, Ordering::Release);

        // Wake up the poller task so it can exit
        self.shared.notify.notify_one();
    }
}

pub type Receiver = flume::Receiver<bundle::Bundle>;

impl Store {
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

    async fn poll_queue(self: Arc<Self>, shared: Arc<Shared>, cap: usize) {
        // This is the main worker loop for the channel's slow path. It will run for the
        // lifetime of the channel, waking up when notified that the channel is congested.
        loop {
            // 1. Wait for a notification to start work. This can come from a sender that
            //    finds the channel full, or from a sender that arrives when the channel
            //    is already in a Draining or Congested state.
            shared.notify.notified().await;

            // 2. Set the state to Draining. This acts as a baseline for detecting any
            //    new senders that arrive while we are busy draining the store.
            //
            //    We use swap() which atomically returns the old state - this replaces
            //    the mutex lock + mem::replace pattern.
            //
            //    AcqRel ordering:
            //    - Acquire: see any bundle writes from senders
            //    - Release: senders see that we're now draining
            let old_state = shared.swap_state(ChannelState::Draining, Ordering::AcqRel);

            if old_state == ChannelState::Closing {
                // If we were notified because we are closing down, exit the loop.
                break;
            }

            // 3. Drain the store completely by repeatedly calling poll_once.
            loop {
                // Keep polling until the store is empty.
                match self.poll_once(&shared, cap).await {
                    Ok(true) => {}
                    Ok(false) => break,
                    Err(_) => {
                        debug!("Poll queue {:?} complete", shared.status);
                        return;
                    }
                }
            }

            // 4. After draining, check if it's safe to re-open the fast path.
            //    This provides hysteresis, preventing rapid switching between states.
            if shared.tx.len() < (cap / 2) {
                // CRITICAL: Check if any senders arrived *while* we were draining (step 3).
                // If the state is still Draining, it means no new senders arrived,
                // and it is safe to re-open the fast path.
                //
                // We use CAS instead of mutex: only transition if still Draining.
                let result = shared.compare_exchange_state(
                    ChannelState::Draining,
                    ChannelState::Open,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                );

                if result.is_err() {
                    // If the state was changed to Congested, we do nothing here. The
                    // notification from that new sender will cause this loop to run again,
                    // ensuring the new items are processed before the fast path re-opens.
                    continue;
                }
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
