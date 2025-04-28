mod admin;
mod collect;
mod dispatch;
mod forward;
mod fragment;
mod ingress;
mod local;
mod report;

use super::*;
use dispatch::DispatchResult;
use metadata::*;

pub type Error = Box<dyn std::error::Error + Send + Sync>;

pub struct Dispatcher {
    cancel_token: tokio_util::sync::CancellationToken,
    store: Arc<store::Store>,
    tx: tokio::sync::mpsc::Sender<bundle::Bundle>,
    service_registry: Arc<service_registry::ServiceRegistry>,
    rib: Arc<rib::Rib>,
    cla_registry: Arc<cla_registry::ClaRegistry>,
    ipn_2_element: Arc<eid_pattern::EidPatternSet>,

    // Config options
    status_reports: bool,
    admin_endpoints: admin_endpoints::AdminEndpoints,

    // JoinHandles
    run_handle: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Dispatcher {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: &config::Config,
        store: Arc<store::Store>,
        service_registry: Arc<service_registry::ServiceRegistry>,
        rib: Arc<rib::Rib>,
        cla_registry: Arc<cla_registry::ClaRegistry>,
    ) -> Arc<Self> {
        // Create a channel for bundles
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let dispatcher = Arc::new(Self {
            cancel_token: tokio_util::sync::CancellationToken::new(),
            store,
            tx,
            service_registry,
            rib,
            cla_registry,
            ipn_2_element: Arc::new(config.ipn_2_element.iter().fold(
                eid_pattern::EidPatternSet::new(),
                |mut acc, e| {
                    acc.insert(e.clone());
                    acc
                },
            )),
            status_reports: config.status_reports,
            admin_endpoints: config.admin_endpoints.clone(),
            run_handle: std::sync::Mutex::new(None),
        });

        // Spawn the dispatch task
        *dispatcher
            .run_handle
            .lock()
            .trace_expect("Lock issue in dispatcher new()") = Some(tokio::spawn(
            dispatcher::Dispatcher::run(dispatcher.clone(), rx),
        ));

        dispatcher
    }

    pub async fn shutdown(&self) {
        self.cancel_token.cancel();

        if let Some(j) = {
            self.run_handle
                .lock()
                .trace_expect("Lock issue in dispatcher shutdown()")
                .take()
        } {
            _ = j.await;
        }
    }

    async fn load_data(&self, bundle: &bundle::Bundle) -> Result<Option<storage::DataRef>, Error> {
        // Try to load the data, but treat errors as 'Storage Depleted'
        let storage_name = bundle.metadata.storage_name.as_ref().unwrap();
        if let Some(data) = self.store.load_data(storage_name).await? {
            return Ok(Some(data));
        }

        warn!("Bundle data {storage_name} has gone from storage");

        // Report the bundle has gone
        self.report_bundle_deletion(bundle, bpv7::StatusReportReasonCode::DepletedStorage)
            .await
            .map(|_| None)
    }

    #[instrument(skip(self))]
    pub async fn drop_bundle(
        &self,
        mut bundle: bundle::Bundle,
        reason: Option<bpv7::StatusReportReasonCode>,
    ) -> Result<(), Error> {
        if let Some(reason) = reason {
            self.report_bundle_deletion(&bundle, reason).await?;
        }

        // Leave a tombstone in the metadata, so we can ignore duplicates
        if let BundleStatus::Tombstone(_) = bundle.metadata.status {
            // Don't update Tombstone timestamp
        } else {
            self.store
                .set_status(
                    &mut bundle,
                    BundleStatus::Tombstone(time::OffsetDateTime::now_utc()),
                )
                .await?;
        }

        // Delete the bundle from the bundle store
        if let Some(storage_name) = bundle.metadata.storage_name {
            self.store.delete_data(&storage_name).await?;
        }

        /* Do not keep Tombstones for our own bundles
         * This is done even after we have set a Tombstone
         * status above to avoid a race
         */
        if self.admin_endpoints.contains(&bundle.bundle.id.source) {
            self.store.delete_metadata(&bundle.bundle.id).await?;
        }
        Ok(())
    }
}
