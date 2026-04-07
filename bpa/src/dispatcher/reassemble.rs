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
            storage_name: Some(storage_name),
            ..Default::default()
        };

        let parsed = ParsedBundle::parse(&data, self.key_provider());
        let Ok(ParsedBundle { bundle, .. }) = parsed else {
            metrics::counter!("bpa.bundle.reassembly.failed").increment(1);
            debug!("Reassembled bundle is invalid: {}", parsed.unwrap_err());

            // TODO: Report this as a reception failure for all the fragments
            // TODO: Wrap damaged bundle in "Junk Bundle Payload" for lost+found

            if let Some(storage_name) = metadata.storage_name {
                self.store.delete_data(&storage_name).await;
            }
            return;
        };

        let bundle = Bundle { metadata, bundle };
        self.store.insert_metadata(&bundle).await;

        // Box::pin breaks the type recursion; call inner directly (already in pool)
        Box::pin(self.ingest_bundle_inner(bundle, data)).await;
    }
}
