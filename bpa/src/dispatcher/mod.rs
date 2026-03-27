use super::*;
use futures::join;
use hardy_bpv7::{eid::Eid, status_report::ReasonCode};

mod admin;
mod dispatch;
mod forward;
mod local;
mod reassemble;
mod report;
mod restart;
mod transitions;

pub(crate) struct Dispatcher {
    tasks: hardy_async::TaskPool,
    processing_pool: hardy_async::BoundedTaskPool,
    store: Arc<storage::Store>,
    cla_registry: Arc<cla::registry::Registry>,
    rib: Arc<rib::Rib>,
    keys_registry: Arc<keys::registry::Registry>,
    filter_registry: Arc<filters::registry::Registry>,

    // Dispatch queue
    dispatch_tx: storage::channel::Sender,

    // Config options
    status_reports: bool,
    node_ids: Arc<node_ids::NodeIds>,
    poll_channel_depth: usize,
}

impl Dispatcher {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        status_reports: bool,
        poll_channel_depth: core::num::NonZeroUsize,
        processing_pool_size: core::num::NonZeroUsize,
        node_ids: Arc<node_ids::NodeIds>,
        store: Arc<storage::Store>,
        cla_registry: Arc<cla::registry::Registry>,
        rib: Arc<rib::Rib>,
        keys_registry: Arc<keys::registry::Registry>,
        filter_registry: Arc<filters::registry::Registry>,
    ) -> Arc<Self> {
        let (dispatcher, start) = Self::new_inner(
            status_reports,
            poll_channel_depth,
            processing_pool_size,
            node_ids,
            store,
            cla_registry,
            rib,
            keys_registry,
            filter_registry,
        );
        start(&dispatcher);
        dispatcher
    }

    #[allow(clippy::too_many_arguments)]
    fn new_inner(
        status_reports: bool,
        poll_channel_depth: core::num::NonZeroUsize,
        processing_pool_size: core::num::NonZeroUsize,
        node_ids: Arc<node_ids::NodeIds>,
        store: Arc<storage::Store>,
        cla_registry: Arc<cla::registry::Registry>,
        rib: Arc<rib::Rib>,
        keys_registry: Arc<keys::registry::Registry>,
        filter_registry: Arc<filters::registry::Registry>,
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
            cla_registry,
            rib,
            keys_registry,
            filter_registry,
            dispatch_tx,
            status_reports,
            node_ids,
            poll_channel_depth: poll_channel_depth_usize,
        });

        (dispatcher, |d| {
            let dispatcher = d.clone();
            hardy_async::spawn!(d.tasks, "dispatch_queue_consumer", async move {
                dispatcher.run_dispatch_queue(dispatch_rx).await
            });
        })
    }

    pub async fn shutdown(&self) {
        self.dispatch_tx.close().await;
        self.processing_pool.shutdown().await;
        self.tasks.shutdown().await;
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    async fn load_data(&self, bundle: &bundle::Bundle) -> Option<Bytes> {
        let storage_name = bundle
            .metadata
            .storage_name
            .as_ref()
            .trace_expect("Bundle without storage_name reached load_data");

        if let Some(data) = self.store.load_data(storage_name).await {
            Some(data)
        } else {
            self.store.tombstone_metadata(&bundle.bundle.id).await;
            None
        }
    }

    pub async fn poll_service_waiting(self: &Arc<Self>, source: &Eid) {
        let (tx, rx) = flume::bounded::<bundle::Bundle>(self.poll_channel_depth);

        let dispatcher = self.clone();

        join!(self.store.poll_service_waiting(source.clone(), tx), async {
            while let Ok(bundle) = rx.recv_async().await {
                let dispatcher = dispatcher.clone();

                if let Some(data) = dispatcher.load_data(&bundle).await {
                    dispatcher.ingest_bundle(bundle, data).await;
                } else {
                    dispatcher.delete_bundle(bundle).await;
                }
            }
        });
    }

    fn key_provider(
        &self,
    ) -> impl Fn(&hardy_bpv7::bundle::Bundle, &[u8]) -> Box<dyn hardy_bpv7::bpsec::key::KeySource> + Clone
    {
        let keys_registry = self.keys_registry.clone();
        move |bundle, data| keys_registry.key_source(bundle, data)
    }

    fn key_source(
        &self,
        bundle: &hardy_bpv7::bundle::Bundle,
        data: &[u8],
    ) -> Box<dyn hardy_bpv7::bpsec::key::KeySource> {
        self.keys_registry.key_source(bundle, data)
    }
}
