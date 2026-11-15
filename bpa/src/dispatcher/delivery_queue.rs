// A registered service's delivery queue: the per-service channel, its
// consumer draining into the service's delivery lanes, and the
// re-registration poll of bundles parked waiting for the service.
use futures::join;
use hardy_bpv7::eid::Eid;

use super::*;
use crate::lanes::seat;

impl Dispatcher {
    /// Create a per-service delivery channel: the hybrid storage channel
    /// whose target status is `DeliverPending { service }`. The channel's
    /// creation-time poll recovers any bundle already persisted in that
    /// status for this EID. The sender becomes the `Service`'s constructor
    /// argument; the receiver goes to
    /// [`start_delivery_queue`](Self::start_delivery_queue) once the
    /// service is published.
    pub fn new_delivery_channel(
        &self,
        service_eid: &Eid,
    ) -> (
        storage::channel::Sender,
        hardy_async::closeable::Receiver<bundle::Bundle>,
    ) {
        self.store.channel(
            bundle::BundleStatus::DeliverPending {
                service: service_eid.clone(),
            },
            self.poll_channel_depth,
        )
    }

    /// Spawn a published service's delivery consumer. Mirrors the per-peer
    /// egress queues (`cla::peers`), including their lanes: the consumer
    /// keeps up to the service's effective lanes of deliveries in flight —
    /// its declared lane count, one unless it opted into more, bounded by
    /// the delivery seat cap — each running as a task of its own. While
    /// every lane is busy the consumer pulls nothing more, so the queue's
    /// channel fills and spills to storage; with one lane, deliveries run
    /// one at a time in queue order. Deliveries start in queue order and
    /// may complete out of order. Once the queue closes (unregistration)
    /// and drains, the consumer exits only after its in-flight deliveries
    /// have finished.
    pub fn start_delivery_queue(
        self: &Arc<Self>,
        service: Arc<services::registry::Service>,
        rx: hardy_async::closeable::Receiver<bundle::Bundle>,
    ) {
        let dispatcher = self.clone();
        let lanes = seat::budget(seat::effective(service.lanes, self.delivery_lane_cap));
        hardy_async::spawn!(self.tasks, "delivery_queue_poller", async move {
            let deliveries = dispatcher.tasks.clone();
            seat::drain(&rx, &lanes, |bundle, lane| {
                let dispatcher = dispatcher.clone();
                let service = service.clone();
                hardy_async::spawn!(deliveries, "delivery", async move {
                    dispatcher.deliver_bundle(service, bundle).await;
                    drop(lane);
                })
            })
            .await
        });
    }

    pub async fn poll_service_waiting(self: &Arc<Self>, source: &Eid) {
        let (stream, rx) = hardy_async::channel::bounded::<bundle::Bundle>(self.poll_channel_depth);

        let dispatcher = self.clone();

        join!(
            async {
                self.store
                    .poll_service_waiting(source.clone(), &stream)
                    .await;
                drop(stream);
            },
            async {
                while let Ok(mut bundle) = rx.recv().await {
                    // Claim the bundle out of WaitingForService: overlapping
                    // polls (a re-registering service) or a concurrent cancel
                    // must not dispatch — and potentially deliver — the same
                    // bundle twice
                    if !dispatcher
                        .store
                        .swap_status(&mut bundle, &bundle::BundleStatus::Dispatching)
                        .await
                    {
                        debug!("Service-waiting bundle already claimed, skipping");
                        continue;
                    }
                    dispatcher.dispatch_bundle(bundle).await;
                }
            }
        );
    }
}
