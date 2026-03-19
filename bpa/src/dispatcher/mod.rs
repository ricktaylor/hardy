mod admin;
mod dispatch;
mod forward;
mod local;
mod reassemble;
mod report;
mod restart;

#[cfg(feature = "tracing")]
use crate::instrument;

use bytes::Bytes;
use futures::join;
use hardy_async::BoundedTaskPool;
use hardy_bpv7::bpsec::key::KeySource as Bpv7KeySource;
use hardy_bpv7::bundle::Bundle as Bpv7Bundle;
use hardy_bpv7::eid::Eid;
use hardy_bpv7::status_report::ReasonCode;
use trace_err::TraceErrOption;
use tracing::warn;

use crate::bundle::{Bundle, BundleStatus};
use crate::cla::ClaRegistry;
use crate::filters::FilterRegistry;
use crate::keys::KeyRegistry;
use crate::node_ids::NodeIds;
use crate::rib::Rib;
use crate::storage::Store;
use crate::storage::channel::Sender;
use crate::{Arc, NonZeroUsize};

pub(crate) struct Dispatcher {
    tasks: hardy_async::TaskPool,
    processing_pool: BoundedTaskPool,
    store: Arc<Store>,
    cla_registry: Arc<ClaRegistry>,
    rib: Arc<Rib>,
    keys_registry: Arc<KeyRegistry>,
    filter_registry: Arc<FilterRegistry>,

    // Dispatch queue
    dispatch_tx: Sender,

    // Config options
    status_reports: bool,
    node_ids: NodeIds,
    poll_channel_depth: usize,
}

impl Dispatcher {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        status_reports: bool,
        poll_channel_depth: NonZeroUsize,
        processing_pool_size: NonZeroUsize,
        node_ids: NodeIds,
        store: Arc<Store>,
        cla_registry: Arc<ClaRegistry>,
        rib: Arc<Rib>,
        keys_registry: Arc<KeyRegistry>,
        filter_registry: Arc<FilterRegistry>,
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
        poll_channel_depth: NonZeroUsize,
        processing_pool_size: NonZeroUsize,
        node_ids: NodeIds,
        store: Arc<Store>,
        cla_registry: Arc<ClaRegistry>,
        rib: Arc<Rib>,
        keys_registry: Arc<KeyRegistry>,
        filter_registry: Arc<FilterRegistry>,
    ) -> (Arc<Self>, impl FnOnce(&Arc<Self>)) {
        if status_reports {
            warn!("Bundle status reports are enabled");
        }

        let poll_channel_depth_usize: usize = poll_channel_depth.into();

        // Create the dispatch queue channel
        let (dispatch_tx, dispatch_rx) =
            store.channel(BundleStatus::Dispatching, poll_channel_depth_usize);

        let dispatcher = Arc::new(Self {
            tasks: hardy_async::TaskPool::new(),
            processing_pool: BoundedTaskPool::new(processing_pool_size),
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

    pub fn node_ids(&self) -> &NodeIds {
        &self.node_ids
    }

    pub async fn shutdown(&self) {
        self.dispatch_tx.close().await;
        self.processing_pool.shutdown().await;
        self.tasks.shutdown().await;
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all))]
    async fn load_data(&self, bundle: &Bundle) -> Option<Bytes> {
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

    #[cfg_attr(feature = "tracing", instrument(skip(self, bundle)))]
    pub async fn drop_bundle(&self, bundle: Bundle, reason: Option<ReasonCode>) {
        if let Some(reason) = reason {
            self.report_bundle_deletion(&bundle, reason).await;
        }

        self.delete_bundle(bundle).await
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self, bundle)))]
    pub async fn delete_bundle(&self, bundle: Bundle) {
        // Delete the bundle from the bundle store
        if let Some(storage_name) = &bundle.metadata.storage_name {
            self.store.delete_data(storage_name).await;
        }
        self.store.tombstone_metadata(&bundle.bundle.id).await
    }

    pub async fn poll_service_waiting(self: &Arc<Self>, source: &Eid) {
        let (tx, rx) = flume::bounded::<Bundle>(self.poll_channel_depth);

        let dispatcher = self.clone();

        join!(self.store.poll_service_waiting(source.clone(), tx), async {
            while let Ok(bundle) = rx.recv_async().await {
                let dispatcher = dispatcher.clone();

                if let Some(data) = dispatcher.load_data(&bundle).await {
                    dispatcher.ingest_bundle(bundle, data).await;
                } else {
                    dispatcher.drop_bundle(bundle, None).await;
                }
            }
        });
    }

    fn key_provider(&self) -> impl Fn(&Bpv7Bundle, &[u8]) -> Box<dyn Bpv7KeySource> + Clone {
        let keys_registry = self.keys_registry.clone();
        move |bundle, data| keys_registry.key_source(bundle, data)
    }

    fn key_source(&self, bundle: &Bpv7Bundle, data: &[u8]) -> Box<dyn Bpv7KeySource> {
        self.keys_registry.key_source(bundle, data)
    }
}
