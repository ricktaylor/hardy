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
    fib: Arc<fib_impl::Fib>,
    ipn_2_element: bpv7::EidPatternMap<(), ()>,

    // Config options
    status_reports: bool,
    wait_sample_interval: time::Duration,
    admin_endpoints: Arc<admin_endpoints::AdminEndpoints>,
    max_forwarding_delay: u32,
}

impl Dispatcher {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: &bpa::Config,
        store: Arc<store::Store>,
        admin_endpoints: Arc<admin_endpoints::AdminEndpoints>,
        service_registry: Arc<service_registry::ServiceRegistry>,
        fib: Arc<fib_impl::Fib>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> (Self, tokio::sync::mpsc::Receiver<bundle::Bundle>) {
        // Create a channel for bundles
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        (
            Self {
                cancel_token,
                store,
                tx,
                service_registry,
                fib,
                ipn_2_element: config.ipn_2_element.clone().unwrap_or_default(),
                status_reports: config.status_reports,
                wait_sample_interval: config.wait_sample_interval,
                admin_endpoints,
                max_forwarding_delay: config.max_forwarding_delay,
            },
            rx,
        )
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
