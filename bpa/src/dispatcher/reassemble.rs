use trace_err::TraceErrResult;
use tracing::{debug, warn};

use super::{Dispatcher, ingress::Received};
use crate::{
    bundle::{Bundle, BundleMetadata, BundleStatus},
    storage::adu_reassembly::ReassemblyResult,
};

impl Dispatcher {
    pub async fn reassemble(&self, mut bundle: Bundle) {
        let (storage_name, data, received_at, origin) =
            match self.store.adu_reassemble(&bundle).await {
                ReassemblyResult::NotReady => {
                    let status = BundleStatus::AduFragment {
                        source: bundle.id().source.clone(),
                        timestamp: bundle.id().timestamp.clone(),
                    };
                    self.store.update_status(&mut bundle, &status).await;
                    return self.store.watch_bundle(bundle).await;
                }
                ReassemblyResult::Failed => {
                    debug!("Fragment reassembly failed for bundle {}", bundle.id());
                    return;
                }
                ReassemblyResult::Done {
                    storage_name,
                    data,
                    received_at,
                    origin,
                } => (storage_name, data, received_at, origin),
            };

        metrics::counter!("bpa.bundle.reassembled").increment(1);

        let mut metadata = BundleMetadata::new(received_at, origin);
        metadata.storage_name = Some(storage_name.clone());

        // TODO: Just push the entire bundle into the stream
        let (tx, mut rx) = hardy_async::channel::bounded(1);
        tx.send(crate::stream::Segment::Final(data))
            .await
            .trace_expect("New stream push failed?!?");

        match self.process_received_bundle(&mut rx, metadata).await {
            // Box::pin breaks the recursive async type cycle:
            //   ingress_bundle → process_bundle → reassemble →
            //   process_received_bundle → ingress_bundle
            Received::Bundle(bundle, data) => Box::pin(self.ingress_bundle(bundle, data)).await,
            // The reassembled data we pre-stored is now orphaned — delete it.
            Received::Disposed => {
                self.store.delete_data(&storage_name).await;
            }
            // A reassembled ADU has no live transfer to refuse (the one
            // reachable refusal is the size cap — the refusal site logs it);
            // delete the orphaned pre-stored data.
            Received::Refused => {
                warn!("Reassembled bundle refused, deleted");
                self.store.delete_data(&storage_name).await;
            }
        }
    }
}
