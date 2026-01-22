use super::*;
use core::result::Result;

enum ChannelState {
    Open,      // Fast path is available
    Draining,  // Fast path is closed. Poller should take over
    Congested, // Bundles arrive while draining
    Closing,   // Channel is closing down
}

struct Shared {
    use_tx: std::sync::Mutex<ChannelState>,
    tx: flume::Sender<bundle::Bundle>,
    status: metadata::BundleStatus,
    notify: Arc<tokio::sync::Notify>,
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

        // Hold the lock for the minimum time needed to check and update the state.
        let mut use_tx = self
            .shared
            .use_tx
            .lock()
            .trace_expect("Failed to lock mutex");

        match *use_tx {
            // Fast path is open, try to send directly to the in-memory channel.
            ChannelState::Open => {
                match self.shared.tx.try_send(bundle) {
                    // Success! The bundle is sent, and we can return immediately.
                    Ok(_) => return Ok(()),
                    Err(flume::TrySendError::Disconnected(b)) => {
                        // Wake up the poller task so it can exit
                        self.shared.notify.notify_one();
                        return Err(SendError(b));
                    }
                    Err(flume::TrySendError::Full(_)) => {
                        // The channel is full. Transition to the Draining state to
                        // signal the slow path poller to take over.
                        *use_tx = ChannelState::Draining;
                    }
                }
            }
            // The poller is draining the store. If a new bundle arrives now, we move
            // to the Congested state to signal that new work arrived during the drain.
            ChannelState::Draining => {
                *use_tx = ChannelState::Congested;
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
        // MArk the channel as closing
        *self
            .shared
            .use_tx
            .lock()
            .trace_expect("Failed to lock mutex") = ChannelState::Closing;

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
            use_tx: std::sync::Mutex::new(ChannelState::Open),
            tx,
            status: status.clone(),
            notify: Arc::new(tokio::sync::Notify::new()),
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
            let old_state = {
                let mut state = shared.use_tx.lock().trace_expect("Failed to lock mutex");
                std::mem::replace(&mut *state, ChannelState::Draining)
            };
            if let ChannelState::Closing = old_state {
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
                let mut use_tx = shared.use_tx.lock().trace_expect("Failed to lock mutex");

                // CRITICAL: Check if any senders arrived *while* we were draining (step 3).
                // If the state is still Draining, it means no new senders arrived,
                // and it is safe to re-open the fast path.
                if let ChannelState::Draining = *use_tx {
                    *use_tx = ChannelState::Open;
                }
                // If the state was changed to Congested, we do nothing here. The
                // notification from that new sender will cause this loop to run again,
                // ensuring the new items are processed before the fast path re-opens.
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
