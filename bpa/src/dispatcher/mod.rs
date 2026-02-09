use super::{metadata::*, *};
use hardy_bpv7::{eid::Eid, status_report::ReasonCode};

mod admin;
mod dispatch;
mod forward;
mod local;
mod reassemble;
mod report;
mod restart;

pub(crate) struct Dispatcher {
    tasks: hardy_async::TaskPool,
    processing_pool: hardy_async::BoundedTaskPool,
    store: Arc<storage::Store>,
    service_registry: Arc<services::registry::Registry>,
    cla_registry: Arc<cla::registry::Registry>,
    rib: Arc<rib::Rib>,
    keys_registry: Arc<keys::registry::Registry>,
    filter_registry: Arc<filters::registry::Registry>,

    // Dispatch queue
    dispatch_tx: storage::channel::Sender,

    // Config options
    status_reports: bool,
    node_ids: node_ids::NodeIds,
    poll_channel_depth: usize,
}

impl Dispatcher {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: &config::Config,
        store: Arc<storage::Store>,
        cla_registry: Arc<cla::registry::Registry>,
        service_registry: Arc<services::registry::Registry>,
        rib: Arc<rib::Rib>,
        keys_registry: Arc<keys::registry::Registry>,
        filter_registry: Arc<filters::registry::Registry>,
    ) -> Arc<Self> {
        let (dispatcher, start) = Self::new_inner(
            config,
            store,
            cla_registry,
            service_registry,
            rib,
            keys_registry,
            filter_registry,
        );
        start(&dispatcher);
        dispatcher
    }

    #[allow(clippy::too_many_arguments)]
    fn new_inner(
        config: &config::Config,
        store: Arc<storage::Store>,
        cla_registry: Arc<cla::registry::Registry>,
        service_registry: Arc<services::registry::Registry>,
        rib: Arc<rib::Rib>,
        keys_registry: Arc<keys::registry::Registry>,
        filter_registry: Arc<filters::registry::Registry>,
    ) -> (Arc<Self>, impl FnOnce(&Arc<Self>)) {
        if config.status_reports {
            warn!("Bundle status reports are enabled");
        }

        let poll_channel_depth: usize = config.poll_channel_depth.into();

        // Create the dispatch queue channel
        let (dispatch_tx, dispatch_rx) =
            store.channel(BundleStatus::Dispatching, poll_channel_depth);

        let dispatcher = Arc::new(Self {
            tasks: hardy_async::TaskPool::new(),
            processing_pool: hardy_async::BoundedTaskPool::new(config.processing_pool_size),
            store,
            service_registry,
            cla_registry,
            rib,
            keys_registry,
            filter_registry,
            dispatch_tx,
            status_reports: config.status_reports,
            node_ids: config.node_ids.clone(),
            poll_channel_depth,
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

    #[cfg_attr(feature = "tracing", instrument(skip_all))]
    async fn load_data(&self, bundle: &bundle::Bundle) -> Option<Bytes> {
        let Some(storage_name) = bundle.metadata.storage_name.as_ref() else {
            error!("Bad bundle has made it deep into the pipeline");
            panic!("Bad bundle has made it deep into the pipeline");
        };

        if let Some(data) = self.store.load_data(storage_name).await {
            Some(data)
        } else {
            self.store.tombstone_metadata(&bundle.bundle.id).await;
            None
        }
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self, bundle)))]
    pub async fn drop_bundle(&self, bundle: bundle::Bundle, reason: Option<ReasonCode>) {
        if let Some(reason) = reason {
            self.report_bundle_deletion(&bundle, reason).await;
        }

        self.delete_bundle(bundle).await
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self, bundle)))]
    pub async fn delete_bundle(&self, bundle: bundle::Bundle) {
        // Delete the bundle from the bundle store
        if let Some(storage_name) = &bundle.metadata.storage_name {
            self.store.delete_data(storage_name).await;
        }
        self.store.tombstone_metadata(&bundle.bundle.id).await
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
