mod admin;
mod collect;
mod config;
mod dispatch;
mod forward;
mod fragment;
mod ingress;
mod local;
mod report;

use super::*;
use hardy_cbor as cbor;
use std::sync::Arc;
use utils::cancel::cancellable_sleep;

use dispatch::DispatchResult;
pub use local::SendRequest;

pub struct Dispatcher {
    config: self::config::Config,
    cancel_token: tokio_util::sync::CancellationToken,
    store: Arc<store::Store>,
    tx: tokio::sync::mpsc::Sender<metadata::Bundle>,
    cla_registry: cla_registry::ClaRegistry,
    app_registry: app_registry::AppRegistry,
    fib: Option<fib::Fib>,
}

impl Dispatcher {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: &::config::Config,
        admin_endpoints: utils::admin_endpoints::AdminEndpoints,
        store: Arc<store::Store>,
        cla_registry: cla_registry::ClaRegistry,
        app_registry: app_registry::AppRegistry,
        fib: Option<fib::Fib>,
        task_set: &mut tokio::task::JoinSet<()>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> Arc<Self> {
        // Create a channel for bundles
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let dispatcher = Arc::new(Self {
            config: self::config::Config::new(config, admin_endpoints),
            cancel_token,
            store,
            tx,
            cla_registry,
            app_registry,
            fib,
        });

        // Spawn the dispatch task
        let dispatcher_cloned = dispatcher.clone();
        task_set.spawn(dispatch::dispatch_task(dispatcher_cloned, rx));

        dispatcher
    }

    async fn load_data(
        &self,
        bundle: &metadata::Bundle,
    ) -> Result<Option<hardy_bpa_api::storage::DataRef>, Error> {
        // Try to load the data, but treat errors as 'Storage Depleted'
        let storage_name = bundle.metadata.storage_name.as_ref().unwrap();
        match self.store.load_data(storage_name).await? {
            None => {
                warn!("Bundle data {storage_name} has gone from storage");

                // Report the bundle has gone
                self.report_bundle_deletion(bundle, bpv7::StatusReportReasonCode::DepletedStorage)
                    .await
                    .map(|_| None)
            }
            Some(data) => Ok(Some(data)),
        }
    }

    #[instrument(skip(self))]
    async fn drop_bundle(
        &self,
        mut bundle: metadata::Bundle,
        reason: Option<bpv7::StatusReportReasonCode>,
    ) -> Result<(), Error> {
        if let Some(reason) = reason {
            self.report_bundle_deletion(&bundle, reason).await?;
        }

        // Leave a tombstone in the metadata, so we can ignore duplicates
        if let metadata::BundleStatus::Tombstone(_) = bundle.metadata.status {
            // Don't update Tombstone timestamp
        } else {
            self.store
                .set_status(
                    &mut bundle,
                    metadata::BundleStatus::Tombstone(time::OffsetDateTime::now_utc()),
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
        if self
            .config
            .admin_endpoints
            .is_admin_endpoint(&bundle.bundle.id.source)
        {
            self.store.delete_metadata(&bundle.bundle.id).await?;
        }
        Ok(())
    }
}
