use tracing::debug;

use super::Dispatcher;
use crate::bundle::{Bundle, BundleStatus};
use crate::storage::adu_reassembly::ReassemblyResult;

impl Dispatcher {
    pub async fn reassemble(&self, mut bundle: Bundle) {
        let (_storage_name, data) = match self.store.adu_reassemble(&bundle).await {
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

        // Reassembled bundle enters the full ingress pipeline
        // Box::pin breaks the recursive async cycle: receive → route → reassemble → receive
        if let Some(result) = Box::pin(self.ingress().receive(data, None, None, None))
            .await
            .unwrap_or(None)
        {
            Box::pin(self.handle_route(result.bundle, result.route)).await;
        }
    }
}
