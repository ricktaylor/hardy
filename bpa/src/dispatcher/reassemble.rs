use super::*;
use storage::adu_reassembly::ReassemblyResult;

impl Dispatcher {
    pub async fn reassemble(&self, mut bundle: bundle::Bundle) {
        let result = self.store.adu_reassemble(&bundle).await;

        match result {
            ReassemblyResult::NotReady => {
                // Not all fragments have arrived yet — transition to AduFragment and wait
                let status = bundle::BundleStatus::AduFragment {
                    source: bundle.bundle.id.source.clone(),
                    timestamp: bundle.bundle.id.timestamp.clone(),
                };
                self.store.update_status(&mut bundle, &status).await;
                self.store.watch_bundle(bundle).await;
            }
            ReassemblyResult::Failed => {
                // All fragments collected but reassembly failed; data already cleaned up
                debug!("Fragment reassembly failed for bundle {}", bundle.bundle.id);
            }
            ReassemblyResult::Done(storage_name, data) => {
                let metadata = bundle::BundleMetadata {
                    storage_name: Some(storage_name),
                    ..Default::default()
                };

                // Reparse the reconstituted bundle, for sanity
                match hardy_bpv7::bundle::ParsedBundle::parse(&data, self.key_provider()) {
                    Ok(hardy_bpv7::bundle::ParsedBundle { bundle, .. }) => {
                        let bundle = bundle::Bundle { metadata, bundle };
                        self.store.insert_metadata(&bundle).await;

                        // Box::pin breaks the type recursion; call inner directly (already in pool)
                        Box::pin(self.ingest_bundle_inner(bundle, data)).await;
                    }
                    Err(error) => {
                        // Reconstituted bundle is garbage
                        debug!("Reassembled bundle is invalid: {error}");

                        // TODO: Report this as a reception failure for all the fragments, if we can identify them (we can at least report the fragment we just received)

                        // TODO: This is where we can wrap the damaged bundle in a "Junk Bundle Payload" and forward it to a 'lost+found' endpoint.  For now we just drop it.

                        if let Some(storage_name) = metadata.storage_name {
                            self.store.delete_data(&storage_name).await;
                        }
                    }
                }
            }
        }
    }
}
