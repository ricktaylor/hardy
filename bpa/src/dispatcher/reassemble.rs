use hardy_bpv7::bundle::ParsedBundle;
use tracing::debug;

use super::Dispatcher;
use crate::bundle::{Bundle, BundleMetadata, BundleStatus};
use crate::storage::adu_reassembly::ReassemblyResult;

impl Dispatcher {
    pub async fn reassemble(&self, mut bundle: Bundle) {
        let (storage_name, data) = match self.store.adu_reassemble(&bundle).await {
            ReassemblyResult::NotReady => {
                let status = BundleStatus::AduFragment {
                    source: bundle.bundle.id.source.clone(),
                    timestamp: bundle.bundle.id.timestamp.clone(),
                };
                self.store.update_status(&mut bundle, &status).await;
                return self.store.watch_bundle(bundle).await;
            }
            ReassemblyResult::Failed => {
                debug!("Fragment reassembly failed for bundle {}", bundle.bundle.id);
                return;
            }
            ReassemblyResult::Done(storage_name, data) => (storage_name, data),
        };

        let metadata = BundleMetadata {
            storage_name: Some(storage_name.clone()),
            ..Default::default()
        };

        // TODO:  This check isn't enough, and really we need to feed the bundle back into the bottom half of Dispatcher::receive_bundle

        let parsed = ParsedBundle::parse(&data, self.key_provider());
        let Ok(ParsedBundle { bundle, .. }) = parsed else {
            metrics::counter!("bpa.bundle.reassembly.failed").increment(1);
            debug!("Reassembled bundle is invalid: {}", parsed.unwrap_err());

            // TODO: Report this as a reception failure for all the fragments
            // TODO: Wrap damaged bundle in "Junk Bundle Payload" for lost+found

            return self.store.delete_data(&storage_name).await;
        };

        metrics::counter!("bpa.bundle.reassembled").increment(1);

        let bundle = Bundle { metadata, bundle };
        if !self.store.insert_metadata(&bundle).await {
            // Bundle with matching id already exists in the metadata store
            metrics::counter!("bpa.bundle.received.duplicate").increment(1);

            // Drop the stored data and do not process further
            return self.store.delete_data(&storage_name).await;
        }

        metrics::gauge!("bpa.bundle.status", "state" => crate::otel_metrics::status_label(&bundle.metadata.status)).increment(1.0);

        // Box::pin breaks the type recursion; call inner directly (already in pool)
        Box::pin(self.ingest_bundle_inner(bundle, data)).await;
    }
}
