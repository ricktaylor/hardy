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

pub struct Dispatcher {
    cancel_token: tokio_util::sync::CancellationToken,
    task_tracker: tokio_util::task::TaskTracker,
    store: Arc<storage::Store>,
    service_registry: Arc<service_registry::ServiceRegistry>,
    cla_registry: Arc<cla::registry::Registry>,
    rib: Arc<rib::Rib>,
    ipn_2_element: Arc<BTreeSet<hardy_eid_patterns::EidPattern>>,
    //keys: Box<[hardy_bpv7::bpsec::key::Key]>,

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
        service_registry: Arc<service_registry::ServiceRegistry>,
        rib: Arc<rib::Rib>,
        //keys: Box<[hardy_bpv7::bpsec::key::Key]>,
    ) -> Self {
        Self {
            cancel_token: tokio_util::sync::CancellationToken::new(),
            task_tracker: tokio_util::task::TaskTracker::new(),
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
            //keys: keys.unwrap_or(Box<NoKeys>::new()),
            status_reports: config.status_reports,
            node_ids: config.node_ids.clone(),
            poll_channel_depth: config.poll_channel_depth.into(),
        }
    }

    pub async fn shutdown(&self) {
        self.cancel_token.cancel();
        self.task_tracker.close();
        self.task_tracker.wait().await;
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all))]
    async fn load_data(self: &Arc<Self>, bundle: &bundle::Bundle) -> Option<Bytes> {
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
    pub async fn drop_bundle(self: &Arc<Self>, bundle: bundle::Bundle, reason: Option<ReasonCode>) {
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

    fn key_store(&self) -> &impl hardy_bpv7::bpsec::key::KeyStore {
        self
    }
}

impl hardy_bpv7::bpsec::key::KeyStore for Dispatcher {
    // TODO: Dispatcher keys!
    fn decrypt_keys<'a>(
        &'a self,
        _source: &Eid,
        _operation: &[hardy_bpv7::bpsec::key::Operation],
    ) -> impl Iterator<Item = &'a hardy_bpv7::bpsec::key::Key> {
        std::iter::empty()
    }
}
