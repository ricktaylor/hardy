use tracing::debug;

use super::Dispatcher;
use crate::bundle::{Bundle, BundleMetadata, BundleStatus, ReadOnlyMetadata};
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

        metrics::counter!("bpa.bundle.reassembled").increment(1);

        let metadata = BundleMetadata {
            storage_name: Some(storage_name),
            status: BundleStatus::New,
            read_only: ReadOnlyMetadata {
                received_at: bundle.metadata.read_only.received_at,
                ..Default::default()
            },
            ..Default::default()
        };

        if let Some((bundle, data)) = self.process_received_bundle(data, metadata).await {
            // Box::pin breaks the recursive async type cycle:
            //   ingress_bundle → process_bundle → reassemble →
            //   process_received_bundle → ingress_bundle
            Box::pin(self.ingress_bundle(bundle, data)).await;
        }
    }
}
