use core::num::NonZeroU64;

use futures::join;
use hardy_bpv7::{eid::Eid, status_report::ReasonCode};
use hardy_eid_patterns::EidPattern;

use super::*;

mod admin;
mod deliver;
mod dispatch;
mod forward;
mod ingress;
mod originate;
mod reassemble;
mod report;
mod restart;

// The default bound on a single reassembled bundle. Streaming producers
// dissolved the transport-level caps that used to bound ingress implicitly,
// so the concat chokepoint enforces one; sized generously above the old
// 16 MiB wire cap to leave room for large ADUs.
const DEFAULT_MAX_BUNDLE_SIZE: NonZeroU64 = NonZeroU64::new(64 * 1024 * 1024).unwrap();

/// The dispatcher's plain configuration values, gathered by the builder.
pub struct Config {
    pub status_reports: bool,
    pub poll_channel_depth: core::num::NonZeroUsize,
    pub processing_pool_size: core::num::NonZeroUsize,
    pub max_bundle_size: Option<NonZeroU64>,
    /// Require primary-block integrity protection (RFC 9171 §4.3.1).
    pub primary_block_integrity: bool,
    /// Require a Bundle Age block on clockless bundles (RFC 9171 §4.4.2).
    pub bundle_age_required: bool,
    /// Peers whose next hop requires legacy 2-element IPN EID encoding in
    /// the per-hop rewrite stage.
    pub ipn_legacy_peers: Vec<EidPattern>,
}

/// The resolution of a hand-off offer: every exit of `offer_to_cla` and
/// `offer_to_service` is one of these variants, consumed exactly once by
/// `Dispatcher::resolve_offer`. `ForwardAckPending`/`DeliveryAckPending`
/// have no storage poller and the reaper defers their expiry, so a claim an
/// offer left unresolved would be a bundle invisible until restart — this
/// return type makes that unwritable.
#[must_use = "a hand-off claim must be resolved"]
enum OfferOutcome {
    /// The hand-off completed: the bundle left the node (transfer) or was
    /// consumed by the service (delivery).
    Completed(bundle::Bundle),
    /// Terminal failure: drop with the reason (reported), or delete
    /// silently when the filter gave none.
    Dropped(bundle::Bundle, Option<ReasonCode>),
    /// Park for a future opportunity, closing the park-vs-poll window
    /// against the routing snapshot captured when the flight began.
    Parked(bundle::Bundle, bundle::BundleStatus, routing::RibSnapshot),
    /// Deliberate detach: the CLA owns the transfer (`Accepted`) and
    /// resolves the claim later via `transfer_outcome`.
    Detached(bundle::Bundle),
    /// Re-enter dispatch for a fresh routing decision.
    Redispatch(bundle::Bundle),
    /// Another resolver (a sweep, the reaper, a duplicate outcome) claimed
    /// the bundle first; its resolution stands.
    Lost,
}

/// Which hand-off produced an [`OfferOutcome`] — completion reports and
/// counters differ.
enum OfferKind {
    Forward,
    Delivery,
}

pub(crate) struct Dispatcher {
    tasks: hardy_async::TaskPool,
    processing_pool: hardy_async::BoundedTaskPool,
    store: Arc<storage::store::Store>,
    rib: Arc<routing::Rib>,
    key_provider: Arc<dyn keys::KeyProvider>,
    filter_engine: Arc<filter::FilterEngine>,
    cla_registry: hardy_async::sync::spin::Once<Arc<cla::registry::ClaRegistry>>,

    // Dispatch queue
    dispatch_tx: storage::channel::Sender,

    // Config options
    status_reports: bool,
    node_ids: Arc<node_ids::NodeIds>,
    poll_channel_depth: usize,
    max_bundle_size: NonZeroU64,
    primary_block_integrity: bool,
    bundle_age_required: bool,
    ipn_legacy_peers: Vec<EidPattern>,
}

impl Dispatcher {
    /// Construct the dispatcher and return it with a deferred-start closure
    /// for the dispatch-queue consumer.
    ///
    /// The closure demands the CLA registry by value and wires it before
    /// spawning the consumer, because the dispatch channel's storage poller
    /// recovers persisted `DispatchPending` bundles as soon as the consumer
    /// drains them, and processing one dereferences the registry. It owns
    /// the channel receiver, so a second start path cannot exist.
    pub fn new(
        config: Config,
        node_ids: Arc<node_ids::NodeIds>,
        store: Arc<storage::store::Store>,
        rib: Arc<routing::Rib>,
        key_provider: Arc<dyn keys::KeyProvider>,
        filter_engine: Arc<filter::FilterEngine>,
    ) -> (Arc<Self>, impl FnOnce(Arc<cla::registry::ClaRegistry>)) {
        if config.status_reports {
            warn!("Bundle status reports are enabled");
        }

        let poll_channel_depth_usize: usize = config.poll_channel_depth.into();

        // Create the dispatch queue channel. DispatchPending marks "queued":
        // the consumer claims each bundle to Dispatching on dequeue, so the
        // channel's storage poller (which recovers by this status) can never
        // re-queue a bundle that is already being processed.
        let (dispatch_tx, dispatch_rx) = store.channel(
            bundle::BundleStatus::DispatchPending,
            poll_channel_depth_usize,
        );

        let dispatcher = Arc::new(Self {
            tasks: hardy_async::TaskPool::new(),
            processing_pool: hardy_async::BoundedTaskPool::new(config.processing_pool_size),
            store,
            rib,
            key_provider,
            filter_engine,
            cla_registry: hardy_async::sync::spin::Once::new(),
            dispatch_tx,
            status_reports: config.status_reports,
            node_ids,
            poll_channel_depth: poll_channel_depth_usize,
            max_bundle_size: config.max_bundle_size.unwrap_or(DEFAULT_MAX_BUNDLE_SIZE),
            primary_block_integrity: config.primary_block_integrity,
            bundle_age_required: config.bundle_age_required,
            ipn_legacy_peers: config.ipn_legacy_peers,
        });

        let d = dispatcher.clone();
        (dispatcher, move |cla_registry| {
            d.cla_registry.call_once(|| cla_registry);
            let dispatcher = d.clone();
            hardy_async::spawn!(d.tasks, "dispatch_queue_consumer", async move {
                dispatcher.run_dispatch_queue(dispatch_rx).await
            });
        })
    }

    // The dispatch size cap, advertised to CLAs at registration.
    pub(crate) fn max_bundle_size(&self) -> NonZeroU64 {
        self.max_bundle_size
    }

    fn cla_registry(&self) -> &Arc<cla::registry::ClaRegistry> {
        // Other readers (poll_waiting, driven from Rib::start) cannot be
        // proven wired by the type system; this expect is the backstop.
        self.cla_registry
            .get()
            .trace_expect("CLA registry not initialized")
    }

    pub async fn shutdown(&self) {
        self.dispatch_tx.close();
        self.processing_pool.shutdown().await;
        self.tasks.shutdown().await;
    }

    /// Load bundle data, dropping the bundle with `DepletedStorage` if the
    /// data is missing and the bundle has not yet expired. Expired-and-missing
    /// bundles are left for the reaper to handle (it will drop them with
    /// `LifetimeExpired`).
    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    async fn load_data_or_drop(&self, bundle: bundle::Bundle) -> Option<(bundle::Bundle, Bytes)> {
        let storage_name = bundle
            .metadata
            .storage_name
            .as_ref()
            .trace_expect("Bundle without storage_name reached load_data_or_drop");

        match self.store.load_data(storage_name).await {
            Some(data) => Some((bundle, data)),
            None => {
                if !bundle.has_expired() {
                    // Bundle data was deleted while queued - not reaped
                    self.drop_bundle(bundle, ReasonCode::DepletedStorage).await;
                }
                None
            }
        }
    }

    // Every drop is a terminal resolution, claimed with a conditional
    // tombstone. Callers own their bundle through the pipeline's claim
    // discipline, but the reaper expires bundles regardless of owner, so
    // any drop can race it, and it races them. Losing the claim means
    // another resolver's deletion report has gone out; this one stays
    // silent rather than contradict it. Callers therefore pass a bundle
    // whose metadata is already stored, with a current status snapshot.
    #[cfg_attr(feature = "instrument", instrument(skip(self, bundle)))]
    pub async fn drop_bundle(&self, bundle: bundle::Bundle, reason: ReasonCode) {
        if !self.store.tombstone_if(&bundle).await {
            debug!(
                "Drop of bundle {} lost the resolution race, ignored",
                bundle.id()
            );
            return;
        }
        metrics::counter!("bpa.bundle.dropped", "reason" => crate::otel_metrics::reason_label(&reason)).increment(1);
        self.report_bundle_deletion(&bundle, reason).await;
        self.delete_bundle(bundle).await
    }

    /// Park a claimed bundle for a future opportunity, closing the
    /// park-vs-poll window.
    ///
    /// The park is a conditional swap: the reaper or a sweep can resolve the
    /// bundle at any await, and a park must never resurrect a tombstone. On
    /// a win, if the route table changed while the bundle was in flight
    /// (`seen` is captured when the flight begins), the event's poll took
    /// its snapshot before this park was visible and cannot have seen the
    /// bundle — so re-enter dispatch once instead of sleeping. Re-dispatch
    /// only fires when the table actually changed, so a deterministic
    /// failure cannot spin; and a racing poll that *did* see the park
    /// arbitrates through the claim-back CAS, so exactly one side proceeds.
    ///
    /// A re-dispatch discards the in-hand copy and re-enters from the
    /// persisted representation: parks persist status only, and a failure
    /// exit may hand in a bundle carrying in-memory rewrites (hop count,
    /// filter mutations) whose block extents no longer index the stored
    /// bytes. Reloading here keeps that invariant in one place instead of
    /// at every caller's exit.
    pub async fn park_bundle(
        &self,
        mut bundle: bundle::Bundle,
        parked: bundle::BundleStatus,
        seen: &routing::RibSnapshot,
    ) {
        if !self.store.swap_status(&mut bundle, &parked).await {
            debug!("Bundle already resolved, dropping duplicate copy");
            return;
        }

        if self.rib.table_changed_since(seen)
            && self
                .store
                .swap_status(&mut bundle, &bundle::BundleStatus::Dispatching)
                .await
        {
            let Some(bundle) = self.store.get_metadata(bundle.id()).await else {
                // Someone resolved the bundle after the swap (e.g. the
                // reaper dropped it as expired); their resolution stands.
                debug!("Re-dispatch lost the bundle to a concurrent resolution");
                return;
            };
            debug!("Routing changed mid-flight, re-dispatching parked bundle");
            return self.dispatch_bundle(bundle).await;
        }

        self.store.watch_bundle(bundle).await
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, bundle)))]
    async fn delete_bundle(&self, bundle: bundle::Bundle) {
        // Delete the bundle from the bundle store
        if let Some(storage_name) = &bundle.metadata.storage_name {
            self.store.delete_data(storage_name).await;
        }
        self.store.tombstone_metadata(bundle.id()).await;

        metrics::gauge!("bpa.bundle.status", "state" => crate::otel_metrics::status_label(&bundle.status)).decrement(1.0);
    }

    /// Resolve a hand-off offer's outcome — the single consumer of
    /// [`OfferOutcome`], shared by forwarding and delivery.
    async fn resolve_offer(&self, kind: OfferKind, outcome: OfferOutcome) {
        match outcome {
            OfferOutcome::Completed(bundle) => {
                // The terminal claim is a conditional tombstone: a
                // concurrent resolver (a peer sweep mid-transfer, a restart
                // re-park) may have got there first, and losing the claim
                // means its resolution has gone out — this one stays silent
                // rather than contradict it.
                if !self.store.tombstone_if(&bundle).await {
                    debug!(
                        "Hand-off completion for {} lost the resolution race, ignored",
                        bundle.id()
                    );
                    return;
                }
                match kind {
                    OfferKind::Forward => {
                        metrics::counter!("bpa.bundle.forwarded").increment(1);
                        self.report_bundle_forwarded(&bundle).await;
                    }
                    OfferKind::Delivery => {
                        metrics::counter!("bpa.bundle.delivered").increment(1);
                        self.report_bundle_delivery(&bundle).await;
                    }
                }
                // Not drop_bundle(): a completed hand-off is not a
                // 'dropped bundle'.
                self.report_bundle_deletion(&bundle, ReasonCode::NoAdditionalInformation)
                    .await;
                self.delete_bundle(bundle).await
            }
            OfferOutcome::Dropped(bundle, Some(reason)) => self.drop_bundle(bundle, reason).await,
            OfferOutcome::Dropped(bundle, None) => self.delete_bundle(bundle).await,
            OfferOutcome::Parked(bundle, parked, seen) => {
                self.park_bundle(bundle, parked, &seen).await
            }
            OfferOutcome::Detached(bundle) => {
                // The counterparty owns the hand-off and resolves the claim
                // later (`transfer_outcome`). The watch stays armed even
                // though the expiry pass defers this status: if a peer
                // sweep parks the bundle before its expiry, the live entry
                // still reaps it promptly.
                self.store.watch_bundle(bundle).await
            }
            OfferOutcome::Redispatch(bundle) => self.dispatch_bundle(bundle).await,
            OfferOutcome::Lost => {}
        }
    }

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
    /// egress queues (`cla::peers`), including their serialization: one
    /// delivery at a time per service, in queue order.
    pub fn start_delivery_queue(
        self: &Arc<Self>,
        service: Arc<services::registry::Service>,
        rx: hardy_async::closeable::Receiver<bundle::Bundle>,
    ) {
        let dispatcher = self.clone();
        hardy_async::spawn!(self.tasks, "delivery_queue_poller", async move {
            while let Ok(bundle) = rx.recv().await {
                dispatcher.deliver_bundle(service.clone(), bundle).await;
            }
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

    fn key_provider(
        &self,
    ) -> impl Fn(&hardy_bpv7::Bundle, &[u8]) -> Box<dyn hardy_bpv7::bpsec::key::KeySource> + Clone + '_
    {
        |bundle, data| self.key_provider.key_source(bundle, data)
    }

    fn key_source(
        &self,
        bundle: &hardy_bpv7::Bundle,
        data: &[u8],
    ) -> Box<dyn hardy_bpv7::bpsec::key::KeySource> {
        self.key_provider.key_source(bundle, data)
    }
}
