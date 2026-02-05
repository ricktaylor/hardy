use super::{metadata::*, *};
use hardy_bpv7::{eid::Eid, status_report::ReasonCode};
use std::collections::BTreeSet;

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
    service_registry: Arc<services::registry::ServiceRegistry>,
    cla_registry: Arc<cla::registry::Registry>,
    rib: Arc<rib::Rib>,
    ipn_2_element: Arc<BTreeSet<hardy_eid_patterns::EidPattern>>,
    keys_registry: Arc<keys::registry::Registry>,
    filter_registry: Arc<filters::registry::Registry>,

    // Dispatch queue (initialized in start())
    dispatch_tx: std::sync::OnceLock<storage::channel::Sender>,

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
        service_registry: Arc<services::registry::ServiceRegistry>,
        rib: Arc<rib::Rib>,
        keys_registry: Arc<keys::registry::Registry>,
        filter_registry: Arc<filters::registry::Registry>,
    ) -> Self {
        Self {
            tasks: hardy_async::TaskPool::new(),
            processing_pool: hardy_async::BoundedTaskPool::new(config.processing_pool_size),
            store,
            service_registry,
            cla_registry,
            rib,
            ipn_2_element: Arc::new(
                config
                    .ipn_2_element
                    .iter()
                    .cloned()
                    .collect::<BTreeSet<_>>(),
            ),
            keys_registry,
            filter_registry,
            dispatch_tx: std::sync::OnceLock::new(),
            status_reports: config.status_reports,
            node_ids: config.node_ids.clone(),
            poll_channel_depth: config.poll_channel_depth.into(),
        }
    }

    pub fn start(self: &Arc<Self>) {
        // Create the dispatch queue channel
        let (dispatch_tx, dispatch_rx) = self
            .store
            .channel(BundleStatus::Dispatching, self.poll_channel_depth);

        self.dispatch_tx
            .set(dispatch_tx)
            .ok()
            .trace_expect("Dispatcher already started");

        // Spawn the dispatch queue consumer
        let dispatcher = self.clone();
        hardy_async::spawn!(self.tasks, "dispatch_queue_consumer", async move {
            dispatcher.run_dispatch_queue(dispatch_rx).await
        });
    }

    pub async fn shutdown(&self) {
        self.dispatch_tx
            .get()
            .trace_expect("Dispatcher not started")
            .close()
            .await;
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

    /// Persist changes made by a filter based on mutation flags.
    /// If bundle data was modified, saves new data and updates storage_name in metadata.
    /// If metadata was modified (or data changed), updates metadata in store.
    #[cfg_attr(feature = "tracing", instrument(skip_all))]
    pub(super) async fn persist_filter_mutation(
        &self,
        mutation: filters::registry::Mutation,
        bundle: &mut bundle::Bundle,
        data: &Bytes,
    ) {
        if mutation.bundle {
            // Save new bundle data
            let new_storage_name = self.store.save_data(data).await;

            // Delete old data if it exists (save new before delete for crash safety)
            if let Some(old_storage_name) = bundle.metadata.storage_name.take() {
                self.store.delete_data(&old_storage_name).await;
            }

            // Update storage_name in metadata
            bundle.metadata.storage_name = Some(new_storage_name);
        }

        // Update metadata if either flag is set (data change implies metadata change)
        if mutation.metadata || mutation.bundle {
            self.store.update_metadata(bundle).await;
        }
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
