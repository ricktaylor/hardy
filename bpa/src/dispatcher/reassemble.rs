use super::*;
use storage::adu_reassembly::ReassemblyResult;

impl Dispatcher {
    pub async fn reassemble(&self, mut bundle: bundle::Bundle) {
        let (storage_name, data) = match self.store.adu_reassemble(&bundle).await {
            ReassemblyResult::NotReady => {
                let status = bundle::BundleStatus::AduFragment {
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

        let metadata = bundle::BundleMetadata {
            storage_name: Some(storage_name),
            ..Default::default()
        };

        let parsed = hardy_bpv7::bundle::ParsedBundle::parse(&data, self.key_provider());
        let Ok(hardy_bpv7::bundle::ParsedBundle { bundle, .. }) = parsed else {
            debug!("Reassembled bundle is invalid: {}", parsed.unwrap_err());

            // TODO: Report this as a reception failure for all the fragments
            // TODO: Wrap damaged bundle in "Junk Bundle Payload" for lost+found

            if let Some(storage_name) = metadata.storage_name {
                self.store.delete_data(&storage_name).await;
            }
            return;
        };

        let bundle = bundle::Bundle { metadata, bundle };
        self.store.insert_metadata(&bundle).await;

        // Box::pin breaks the type recursion; call inner directly (already in pool)
        Box::pin(self.ingest_bundle_inner(bundle, data)).await;
    }
}
