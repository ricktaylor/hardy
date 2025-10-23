use super::*;
use core::result::Result;

enum ChannelState {
    Open,      // Fast path is available
    Draining,  // Fast path is closed. Poller should take over
    Congested, // Bundles arrive while draining
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
        }

        // Notify the poll_queue task that it has work to do on the slow path.
        self.shared.notify.notify_one();
        Ok(())
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
        let task = async move { Self::poll_queue(store, shared_cloned, cap).await };

        #[cfg(feature = "tracing")]
        let task = {
            let span = tracing::trace_span!(parent: None, "channel_queue_poll", ?status);
            span.follows_from(tracing::Span::current());
            task.instrument(span)
        };

        self.task_tracker.spawn(task);

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
            *shared.use_tx.lock().trace_expect("Failed to lock mutex") = ChannelState::Draining;

            // 3. Drain the store completely by repeatedly calling poll_once.
            loop {
                // Keep polling until the store is empty.
                match self.poll_once(&shared, cap).await {
                    Ok(true) => {}
                    Ok(false) => break,
                    Err(_) => return,
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

        let task = async move {
            let mut pushed_one = false;
            loop {
                let Ok(bundle) = inner_rx.recv_async().await else {
                    break Ok(pushed_one);
                };

                // Just do some checks
                if !bundle.has_expired() && bundle.metadata.status == shared_cloned.status {
                    // Send into queue
                    shared_cloned.tx.send_async(bundle).await.map_err(|_| ())?;

                    pushed_one = true;
                }
            }
        };

        #[cfg(feature = "tracing")]
        let task = {
            let span = tracing::trace_span!(parent: None, "poll_pending_once");
            span.follows_from(tracing::Span::current());
            task.instrument(span)
        };

        let h = self.task_tracker.spawn(task);

        self.metadata_storage
            .poll_pending(inner_tx, &shared.status, cap)
            .await
            .trace_expect("Failed to poll store for pending bundles");

        h.await.trace_expect("Failed to join task")
    }
}
