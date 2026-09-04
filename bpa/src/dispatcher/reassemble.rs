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

        let metadata = BundleMetadata::new(received_at, origin);

        // Box::pin breaks the async cycle: process_received_bundle executes
        // the gate's routing decision inline, whose Deliver-fragment arm is
        // this function. Depth is bounded — fragments reassemble into a
        // whole, which cannot be a fragment again. The reassembled bytes are
        // handed as the bundle stream and the pipeline's spool saves an
        // admitted bundle fresh; the pre-stored safety copy (which bridges
        // the crash window between fragment deletion and admission) is
        // stranded in every outcome and deleted below — a crash before the
        // delete re-admits it as a restart orphan, where it loses as a
        // duplicate.
        let mut data = data;
        match Box::pin(self.process_received_bundle(&mut data, metadata)).await {
            Received::Dispatched | Received::Disposed => {}
            // A reassembled ADU has no live transfer to refuse (the one
            // reachable refusal is the size cap — the refusal site logs it).
            Received::Refused => {
                warn!("Reassembled bundle refused, deleted");
            }
        }
        self.store.delete_data(&storage_name).await;
    }
}
