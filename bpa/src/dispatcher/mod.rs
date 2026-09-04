use futures::join;
use hardy_async::sync::Mutex;
use hardy_bpv7::{bundle::Id, eid::Eid, status_report::ReasonCode};

use super::*;

mod admin;
mod dispatch;
mod forward;
mod ingress;
mod local;
mod reassemble;
mod report;
mod restart;

// The default bound on a single reassembled bundle. Streaming producers
// dissolved the transport-level caps that used to bound ingress implicitly,
// so the concat chokepoint enforces one; sized generously above the old
// 16 MiB wire cap to leave room for large ADUs.
const DEFAULT_MAX_BUNDLE_SIZE: core::num::NonZeroUsize =
    core::num::NonZeroUsize::new(64 * 1024 * 1024).unwrap();

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

    // Bundles parked as WaitingForService by their own in-flight delivery.
    // The status value alone cannot distinguish "parked mid-announcement"
    // from "parked awaiting a registration", so `poll_service_waiting`
    // consults this set before claiming a bundle out of the parked state;
    // without it an overlapping poll would deliver the same bundle twice.
    deliveries_in_flight: Mutex<HashSet<Id>>,

    // Config options
    status_reports: bool,
    node_ids: Arc<node_ids::NodeIds>,
    poll_channel_depth: usize,
    max_bundle_size: usize,
}

impl Dispatcher {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        status_reports: bool,
        poll_channel_depth: core::num::NonZeroUsize,
        processing_pool_size: core::num::NonZeroUsize,
        max_bundle_size: Option<core::num::NonZeroUsize>,
        node_ids: Arc<node_ids::NodeIds>,
        store: Arc<storage::store::Store>,
        rib: Arc<routing::Rib>,
        key_provider: Arc<dyn keys::KeyProvider>,
        filter_engine: Arc<filter::FilterEngine>,
    ) -> Arc<Self> {
        let (dispatcher, start) = Self::new_inner(
            status_reports,
            poll_channel_depth,
            processing_pool_size,
            max_bundle_size,
            node_ids,
            store,
            rib,
            key_provider,
            filter_engine,
        );
        start(&dispatcher);
        dispatcher
    }

    #[allow(clippy::too_many_arguments)]
    fn new_inner(
        status_reports: bool,
        poll_channel_depth: core::num::NonZeroUsize,
        processing_pool_size: core::num::NonZeroUsize,
        max_bundle_size: Option<core::num::NonZeroUsize>,
        node_ids: Arc<node_ids::NodeIds>,
        store: Arc<storage::store::Store>,
        rib: Arc<routing::Rib>,
        key_provider: Arc<dyn keys::KeyProvider>,
        filter_engine: Arc<filter::FilterEngine>,
    ) -> (Arc<Self>, impl FnOnce(&Arc<Self>)) {
        if status_reports {
            warn!("Bundle status reports are enabled");
        }

        let poll_channel_depth_usize: usize = poll_channel_depth.into();

        // Create the dispatch queue channel
        let (dispatch_tx, dispatch_rx) =
            store.channel(bundle::BundleStatus::Dispatching, poll_channel_depth_usize);

        let dispatcher = Arc::new(Self {
            tasks: hardy_async::TaskPool::new(),
            processing_pool: hardy_async::BoundedTaskPool::new(processing_pool_size),
            store,
            rib,
            key_provider,
            filter_engine,
            cla_registry: hardy_async::sync::spin::Once::new(),
            dispatch_tx,
            deliveries_in_flight: Mutex::new(HashSet::new()),
            status_reports,
            node_ids,
            poll_channel_depth: poll_channel_depth_usize,
            max_bundle_size: max_bundle_size.unwrap_or(DEFAULT_MAX_BUNDLE_SIZE).get(),
        });

        (dispatcher, |d| {
            let dispatcher = d.clone();
            hardy_async::spawn!(d.tasks, "dispatch_queue_consumer", async move {
                dispatcher.run_dispatch_queue(dispatch_rx).await
            });
        })
    }

    pub fn set_cla_registry(&self, cla_registry: Arc<cla::registry::ClaRegistry>) {
        self.cla_registry.call_once(|| cla_registry);
    }

    fn cla_registry(&self) -> &Arc<cla::registry::ClaRegistry> {
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
                bundle.bundle.id
            );
            return;
        }
        metrics::counter!("bpa.bundle.dropped", "reason" => crate::otel_metrics::reason_label(&reason)).increment(1);
        self.report_bundle_deletion(&bundle, reason).await;
        self.delete_bundle(bundle).await
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, bundle)))]
    async fn delete_bundle(&self, bundle: bundle::Bundle) {
        // Delete the bundle from the bundle store
        if let Some(storage_name) = &bundle.metadata.storage_name {
            self.store.delete_data(storage_name).await;
        }
        self.store.tombstone_metadata(&bundle.bundle.id).await;

        metrics::gauge!("bpa.bundle.status", "state" => crate::otel_metrics::status_label(&bundle.metadata.status)).decrement(1.0);
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
                    // A bundle parked by its own in-flight delivery is not
                    // claimable: the announcement is still open and will
                    // resolve it (or re-park it for a later poll).
                    if dispatcher
                        .deliveries_in_flight
                        .lock()
                        .contains(&bundle.bundle.id)
                    {
                        debug!("Service-waiting bundle has a delivery in flight, skipping");
                        continue;
                    }

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
