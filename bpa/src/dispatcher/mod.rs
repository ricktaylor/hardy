mod admin;
mod dispatch;
mod fragment;
mod ingress;
mod local;
mod report;

use super::*;
use hardy_bpv7::{eid::Eid, status_report::ReasonCode};
use metadata::*;

type Error = Box<dyn std::error::Error + Send + Sync>;

pub struct Dispatcher {
    cancel_token: tokio_util::sync::CancellationToken,
    task_tracker: tokio_util::task::TaskTracker,
    store: Arc<store::Store>,
    service_registry: Arc<service_registry::ServiceRegistry>,
    rib: Arc<rib::Rib>,
    ipn_2_element: Arc<hardy_eid_pattern::EidPatternSet>,

    // Config options
    status_reports: bool,
    node_ids: node_ids::NodeIds,
}

impl Dispatcher {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: &config::Config,
        store: Arc<store::Store>,
        service_registry: Arc<service_registry::ServiceRegistry>,
        rib: Arc<rib::Rib>,
        //keys: Box<[hardy_bpv7::bpsec::key::Key]>,
    ) -> Self {
        Self {
            cancel_token: tokio_util::sync::CancellationToken::new(),
            task_tracker: tokio_util::task::TaskTracker::new(),
            store,
            service_registry,
            rib,
            ipn_2_element: Arc::new(config.ipn_2_element.iter().fold(
                hardy_eid_pattern::EidPatternSet::new(),
                |mut acc, e| {
                    acc.insert(e.clone());
                    acc
                },
            )),
            status_reports: config.status_reports,
            node_ids: config.node_ids.clone(),
        }
    }

    pub fn start(self: &Arc<Self>) -> Result<(), Error> {
        // Start the store - this can take a while as the store is walked
        let dispatcher = self.clone();
        self.task_tracker.spawn(async move {
            // Now check the metadata storage for old data
            dispatcher
                .store
                .start(dispatcher.clone(), &dispatcher.cancel_token)
                .await
                .trace_expect("Storage check failed")
        });
        Ok(())
    }

    pub async fn shutdown(&self) {
        self.cancel_token.cancel();
        self.task_tracker.close();
        self.task_tracker.wait().await;
    }

    async fn load_data(self: &Arc<Self>, bundle: &bundle::Bundle) -> Result<Option<Bytes>, Error> {
        let Some(storage_name) = bundle.metadata.storage_name.as_ref() else {
            error!("Bad bundle has made it deep into the pipeline");
            return Ok(None);
        };

        if let Some(data) = self.store.load_data(storage_name).await? {
            Ok(Some(data))
        } else {
            warn!("Bundle data {storage_name} has gone from storage");
            Ok(None)
        }
    }

    #[instrument(skip(self))]
    async fn drop_bundle(
        self: &Arc<Self>,
        bundle: bundle::Bundle,
        reason: Option<ReasonCode>,
    ) -> Result<(), Error> {
        if let Some(reason) = reason {
            self.report_bundle_deletion(&bundle, reason).await;
        }

        // Delete the bundle from the bundle store
        if let Some(storage_name) = &bundle.metadata.storage_name {
            self.store.delete_data(storage_name).await?;
        }
        self.store.tombstone_metadata(&bundle.bundle.id).await
    }
}

impl hardy_bpv7::bpsec::key::KeyStore for Dispatcher {
    fn decrypt_keys<'a>(
        &'a self,
        _source: &Eid,
        _operation: &[hardy_bpv7::bpsec::key::Operation],
    ) -> impl Iterator<Item = &'a hardy_bpv7::bpsec::key::Key> {
        std::iter::empty()
    }
}
