use trace_err::TraceErrResult;
use tracing::debug;

use super::Dispatcher;
use crate::{
    bundle::{Bundle, BundleMetadata, BundleStatus, ReadOnlyMetadata},
    storage::adu_reassembly::ReassemblyResult,
};

impl Dispatcher {
    pub async fn reassemble(&self, mut bundle: Bundle) {
        let (storage_name, data, received_at) = match self.store.adu_reassemble(&bundle).await {
            ReassemblyResult::NotReady => {
                let status = BundleStatus::AduFragment {
                    source: bundle.bundle.primary.id.source.clone(),
                    timestamp: bundle.bundle.primary.id.timestamp.clone(),
                };
                self.store.update_status(&mut bundle, &status).await;
                return self.store.watch_bundle(bundle).await;
            }
            ReassemblyResult::Failed => {
                debug!(
                    "Fragment reassembly failed for bundle {}",
                    bundle.bundle.primary.id
                );
                return;
            }
            ReassemblyResult::Done {
                storage_name,
                data,
                received_at,
            } => (storage_name, data, received_at),
        };

        metrics::counter!("bpa.bundle.reassembled").increment(1);

        let metadata = BundleMetadata {
            storage_name: Some(storage_name.clone()),
            status: BundleStatus::New,
            read_only: ReadOnlyMetadata {
                received_at,
                ..Default::default()
            },
            ..Default::default()
        };

        // TODO: Just push the entire bundle into the stream
        let (tx, rx) = hardy_async::channel::bounded(1);
        tx.send(crate::cla::Segment::Final(data))
            .await
            .trace_expect("New stream push failed?!?");

        match self.process_received_bundle(&rx, metadata).await {
            // Box::pin breaks the recursive async type cycle:
            //   ingress_bundle → process_bundle → reassemble →
            //   process_received_bundle → ingress_bundle
            Some((bundle, data)) => Box::pin(self.ingress_bundle(bundle, data)).await,
            // The reassembled data we pre-stored is now orphaned — delete it.
            None => {
                self.store.delete_data(&storage_name).await;
            }
        }
    }
}
